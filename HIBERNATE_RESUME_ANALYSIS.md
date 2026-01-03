# Hibernate/Suspend Resume Display Issues - Analysis

This document captures the analysis of display issues after hibernate/suspend resume in niri,
including root cause investigation, the fix implemented in this branch, and notes for potential
future kernel-level investigation.

## Problem Statement

After hibernating or suspending the system, external monitors exhibit issues:

1. **"Atomic Test failed for new properties on crtc" errors** - External monitors fail to
   resume properly, showing atomic commit failures in the logs.

2. **Stale connector state** - `niri msg outputs` reports monitors as connected/available
   even when they are physically disconnected, off, or unusable.

3. **Inconsistent recovery** - Sometimes monitors recover on their own, sometimes they don't.

### Related Issues

- niri #1722 - Original issue report
- niri #2236, #2907, #2265 - Related suspend/resume display problems
- Smithay #1772 - Display wake issues

## Root Cause Analysis

### Why libseat Doesn't Help

The libseat session interface (used by niri via Smithay) provides `Enable` and `Disable`
events for session state changes. However, **these events only fire for VT (virtual terminal)
switches, NOT for suspend/hibernate**.

This can be verified by checking journal logs:
```
journalctl --user -u niri --since '120m ago' --grep 'session'
```

After hibernate, you'll see "locking/unlocking session" messages but NO "pausing/resuming
session" messages. The session events that would trigger DRM state refresh never fire.

### The DRM State Problem

When the system hibernates:
1. The kernel suspends all devices including the GPU and display controllers
2. DRM (Direct Rendering Manager) state is saved
3. On resume, the kernel restores devices

However, the compositor (niri) is unaware that a suspend/resume cycle occurred because:
- libseat only notifies about VT switches
- No event fires to tell the compositor to refresh its DRM state

This leads to stale state where:
- The compositor's cached connector/CRTC state doesn't match reality
- Atomic commits fail because they're based on outdated state
- External monitors appear "connected" in software but are unusable

### The Standard Solution: logind PrepareForSleep

The correct way to handle suspend/hibernate in Linux userspace is to monitor the
`org.freedesktop.login1.Manager` D-Bus interface for the `PrepareForSleep` signal:

- Signal argument `true` = system is about to sleep
- Signal argument `false` = system has woken up

This is the same mechanism used by screen lockers, media players, and other
session-aware applications.

## The Fix

This branch adds handling for the `PrepareForSleep` D-Bus signal in niri:

### Files Changed

1. **`src/dbus/freedesktop_login1.rs`**
   - Added `PrepareForSleep` signal subscription via zbus
   - Added `Login1ToNiri::PrepareForSleep(bool)` message variant

2. **`src/niri.rs`**
   - Extended `on_login1_msg()` to handle `PrepareForSleep`
   - On resume (`start = false`), calls `backend.on_sleep_resume()`

3. **`src/backend/mod.rs`**
   - Added `on_sleep_resume()` method to `Backend` enum

4. **`src/backend/tty.rs`**
   - Added `Tty::on_sleep_resume()` which:
     - Calls `device_changed()` for all DRM devices (forces connector rescan)
     - Calls `refresh_ipc_outputs()` to update IPC state
     - Calls `notify_activity()` and `queue_redraw_all()`

### Why This Works

On resume from sleep:
1. logind emits `PrepareForSleep(false)`
2. niri receives this via D-Bus
3. niri calls `device_changed()` for each DRM device
4. This triggers `DrmScanner` to rescan connectors
5. Stale CRTC/connector state is refreshed
6. Atomic commits now succeed with accurate state

## Alternative Approach: Smithay-Level Fix

A parallel fix was developed that adds this functionality to Smithay itself:

- **Smithay branch**: `feat/prepare-for-sleep-session-event`
- **niri branch**: `fix/prepare-for-sleep-smithay`

This approach:
1. Adds `backend_session_logind` feature to Smithay
2. Creates `LogindSessionNotifier` calloop event source
3. Adds `SessionEvent::PreparingSleep` and `SessionEvent::ResumedFromSleep` variants
4. Allows any Smithay-based compositor to handle suspend/resume

The Smithay approach is more general but requires upstream Smithay changes.
The niri-only approach (this branch) works immediately without Smithay modifications.

## What This Fix Does NOT Address

### Kernel DRM Subsystem Issues

If the kernel's DRM subsystem itself reports incorrect connector state after hibernate,
this fix cannot help. Specifically:

- If `drm_connector.status` reports `connected` for a disconnected monitor
- If EDID data is stale or cached incorrectly
- If the GPU driver fails to properly reinitialize display outputs

These would require kernel-level fixes in:
- The DRM core (`drivers/gpu/drm/drm_connector.c`, `drm_probe_helper.c`)
- GPU-specific drivers (i915, amdgpu, nouveau, etc.)

### Debugging Kernel Issues

If issues persist after this fix, investigate:

1. **Check connector state directly**:
   ```bash
   cat /sys/class/drm/card*/card*-*/status
   cat /sys/class/drm/card*/card*-*/enabled
   ```

2. **Force connector reprobe**:
   ```bash
   echo detect > /sys/class/drm/card0/card0-DP-1/status
   ```

3. **Check dmesg for DRM errors**:
   ```bash
   dmesg | grep -i drm
   dmesg | grep -i connector
   ```

4. **Trace DRM atomic commits**:
   ```bash
   echo 0x1ff > /sys/module/drm/parameters/debug
   # Reproduce issue
   dmesg | grep -i atomic
   ```

### Relevant Kernel Code Paths

For future kernel investigation:

- `drivers/gpu/drm/drm_probe_helper.c` - Connector probing logic
- `drivers/gpu/drm/drm_atomic_helper.c` - Atomic commit helpers
- `drivers/gpu/drm/drm_crtc_helper.c` - CRTC helpers
- GPU-specific suspend/resume:
  - Intel: `drivers/gpu/drm/i915/i915_driver.c` (`i915_drm_suspend`, `i915_drm_resume`)
  - AMD: `drivers/gpu/drm/amd/amdgpu/amdgpu_device.c`
  - Nouveau: `drivers/gpu/drm/nouveau/nouveau_drm.c`

## Testing

To test this fix:

1. Build niri with this branch
2. Connect external monitor(s)
3. Hibernate: `systemctl hibernate`
4. Resume and check:
   - `niri msg outputs` - Should show accurate state
   - External monitors should be usable
   - Check logs: `journalctl --user -u niri -f`
     - Should see "system waking from sleep, refreshing connectors"

## References

- [logind D-Bus API](https://www.freedesktop.org/software/systemd/man/org.freedesktop.login1.html)
- [libseat documentation](https://sr.ht/~kennylevinsen/seatd/)
- [DRM documentation](https://docs.kernel.org/gpu/drm-kms.html)
- [Smithay session handling](https://github.com/Smithay/smithay/tree/master/src/backend/session)

## Conversation Context

This analysis was developed through debugging session on 2026-01-03.
The fix addresses the userspace portion of the problem. If kernel-level
DRM issues are discovered, this document provides a starting point for
that investigation.
