use futures_util::StreamExt;
use std::env;
use zbus::fdo;
use zbus::names::InterfaceName;
use zbus::proxy;

/// Proxy for the org.freedesktop.login1.Manager D-Bus interface.
#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Manager {
    /// PrepareForSleep signal.
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

pub enum Login1ToNiri {
    LidClosedChanged(bool),
    /// PrepareForSleep signal from logind. `true` = going to sleep, `false` = waking up.
    PrepareForSleep(bool),
    LockRequested,
    UnlockRequested,
}

pub fn start(
    to_niri: calloop::channel::Sender<Login1ToNiri>,
) -> anyhow::Result<zbus::blocking::Connection> {
    let conn = zbus::blocking::Connection::system()?;

    // Spawn task to monitor property changes (LidClosed).
    let async_conn = conn.inner().clone();
    let to_niri_clone = to_niri.clone();
    let props_future = async move {
        let proxy = fdo::PropertiesProxy::new(
            &async_conn,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
        )
        .await;
        let proxy = match proxy {
            Ok(x) => x,
            Err(err) => {
                warn!("error creating PropertiesProxy: {err:?}");
                return;
            }
        };

        let mut props_changed = match proxy.receive_properties_changed().await {
            Ok(x) => x,
            Err(err) => {
                warn!("error subscribing to PropertiesChanged: {err:?}");
                return;
            }
        };

        let props = proxy
            .get_all(InterfaceName::try_from("org.freedesktop.login1.Manager").unwrap())
            .await;
        let mut props = match props {
            Ok(x) => x,
            Err(err) => {
                warn!("error receiving initial properties: {err:?}");
                return;
            }
        };

        trace!("initial properties: {props:?}");

        let mut lid_closed = props
            .remove("LidClosed")
            .and_then(|value| bool::try_from(value).ok())
            .unwrap_or_default();

        if let Err(err) = to_niri_clone.send(Login1ToNiri::LidClosedChanged(lid_closed)) {
            warn!("error sending initial lid state to niri: {err:?}");
            return;
        };

        while let Some(signal) = props_changed.next().await {
            let args = match signal.args() {
                Ok(args) => args,
                Err(err) => {
                    warn!("error parsing PropertiesChanged args: {err:?}");
                    return;
                }
            };

            let mut new_lid_closed = lid_closed;
            let mut changed = false;
            for (name, value) in args.changed_properties() {
                trace!("changed property: {name} => {value:?}");
                if *name != "LidClosed" {
                    continue;
                }

                new_lid_closed = bool::try_from(value).unwrap_or(new_lid_closed);
                changed = true;
            }

            if !changed {
                continue;
            }

            if new_lid_closed == lid_closed {
                continue;
            }

            lid_closed = new_lid_closed;
            if let Err(err) = to_niri_clone.send(Login1ToNiri::LidClosedChanged(lid_closed)) {
                warn!("error sending message to niri: {err:?}");
                return;
            };
        }
    };

    let task = conn
        .inner()
        .executor()
        .spawn(props_future, "monitor login1 property changes");
    task.detach();

    // Spawn task to monitor PrepareForSleep signal.
    let async_conn = conn.inner().clone();
    let to_niri_clone = to_niri.clone();
    let sleep_future = async move {
        let manager_proxy = match ManagerProxy::new(&async_conn).await {
            Ok(x) => x,
            Err(err) => {
                warn!("error creating ManagerProxy: {err:?}");
                return;
            }
        };

        let mut prepare_for_sleep = match manager_proxy.receive_prepare_for_sleep().await {
            Ok(x) => x,
            Err(err) => {
                warn!("error subscribing to PrepareForSleep: {err:?}");
                return;
            }
        };

        while let Some(signal) = prepare_for_sleep.next().await {
            let args = match signal.args() {
                Ok(args) => args,
                Err(err) => {
                    warn!("error parsing PrepareForSleep args: {err:?}");
                    return;
                }
            };

            let start = args.start;
            debug!(
                "PrepareForSleep: {}",
                if start { "going to sleep" } else { "waking up" }
            );

            if let Err(err) = to_niri_clone.send(Login1ToNiri::PrepareForSleep(start)) {
                warn!("error sending PrepareForSleep to niri: {err:?}");
                return;
            };
        }
    };

    let task = conn
        .inner()
        .executor()
        .spawn(sleep_future, "monitor login1 PrepareForSleep signal");
    task.detach();

    // Start the lock/unlock signals monitor
    let async_conn = conn.inner().clone();
    let lock_signals_future = monitor_lock_signals(async_conn, to_niri);
    let lock_task = conn
        .inner()
        .executor()
        .spawn(lock_signals_future, "monitor login1 lock signals");
    lock_task.detach();

    Ok(conn)
}

async fn monitor_lock_signals(
    async_conn: zbus::Connection,
    to_niri: calloop::channel::Sender<Login1ToNiri>,
) {
    // Get the current session ID from environment
    let session_id = match env::var("XDG_SESSION_ID") {
        Ok(id) => id,
        Err(_) => {
            warn!("XDG_SESSION_ID environment variable not found");
            return;
        }
    };

    // Get the session object path
    let manager_proxy = match zbus::proxy::Proxy::new(
        &async_conn,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await
    {
        Ok(proxy) => proxy,
        Err(err) => {
            warn!("error creating Manager proxy: {err:?}");
            return;
        }
    };

    // Call GetSession to get our session path
    let session_path: zbus::zvariant::OwnedObjectPath = match manager_proxy
        .call::<_, _, zbus::zvariant::OwnedObjectPath>("GetSession", &(session_id,))
        .await
    {
        Ok(path) => path,
        Err(err) => {
            warn!("error getting session path: {err:?}");
            return;
        }
    };

    trace!("Found session path: {}", session_path);

    // Create a proxy for the session
    let session_proxy = match zbus::proxy::Proxy::new(
        &async_conn,
        "org.freedesktop.login1",
        session_path,
        "org.freedesktop.login1.Session",
    )
    .await
    {
        Ok(proxy) => proxy,
        Err(err) => {
            warn!("error creating Session proxy: {err:?}");
            return;
        }
    };

    // Listen for the Lock signal
    let mut lock_stream = match session_proxy.receive_signal("Lock").await {
        Ok(stream) => stream,
        Err(err) => {
            warn!("error subscribing to Lock signal: {err:?}");
            return;
        }
    };

    // Listen for the Unlock signal
    let mut unlock_stream = match session_proxy.receive_signal("Unlock").await {
        Ok(stream) => stream,
        Err(err) => {
            warn!("error subscribing to Unlock signal: {err:?}");
            return;
        }
    };

    trace!("Successfully subscribed to Lock and Unlock signals");

    // Report that we support screen locking by setting the LockedHint property
    // to false initially (meaning we're unlocked but capable of locking)
    if let Err(err) = session_proxy.call::<_, _, ()>("SetLockedHint", &(false,)).await {
        warn!("error setting initial LockedHint: {err:?}");
    }

    // Loop to listen for signals
    loop {
        futures_util::select! {
            lock = lock_stream.next() => {
                if lock.is_some() {
                    trace!("Received Lock signal from systemd-logind");
                    if let Err(err) = to_niri.send(Login1ToNiri::LockRequested) {
                        warn!("error sending LockRequested to niri: {err:?}");
                        return;
                    }
                } else {
                    warn!("Lock signal stream ended");
                    return;
                }
            },
            unlock = unlock_stream.next() => {
                if unlock.is_some() {
                    trace!("Received Unlock signal from systemd-logind");
                    if let Err(err) = to_niri.send(Login1ToNiri::UnlockRequested) {
                        warn!("error sending UnlockRequested to niri: {err:?}");
                        return;
                    }
                } else {
                    warn!("Unlock signal stream ended");
                    return;
                }
            }
        }
    }
}
