//! Runtime paths for daemon lifecycle bookkeeping — the pidfile the TUI reads
//! to decide whether to auto-spawn a daemon, and that the daemon itself locks
//! to prevent two instances starting at once.

use std::path::PathBuf;

/// Directory for runtime state: `$XDG_RUNTIME_DIR/maraetai` when set (the
/// normal case on any systemd-managed Linux desktop — cleared automatically
/// on logout, which is exactly the lifetime a stale pidfile should have),
/// falling back to a per-user temp directory otherwise so this still works
/// on a minimal/non-systemd system.
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("maraetai");
        }
    }
    let uid = uid();
    std::env::temp_dir().join(format!("maraetai-{uid}"))
}

/// Path to the daemon's pidfile (also used as the `flock` target for
/// single-instance enforcement — see the daemon crate's `lifecycle` module).
pub fn pidfile_path() -> PathBuf {
    runtime_dir().join("daemon.pid")
}

/// Path to the daemon's control-channel Unix socket — macOS's transport
/// (no D-Bus session bus is available there by default; see the daemon's
/// `socket_control` and the TUI's `socket_client`). Unused on Linux, which
/// talks to the daemon over D-Bus instead, but defined unconditionally
/// since it's plain data with no platform-specific meaning of its own.
pub fn control_socket_path() -> PathBuf {
    runtime_dir().join("control.sock")
}

#[cfg(unix)]
fn uid() -> u32 {
    // SAFETY: getuid() has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn uid() -> u32 {
    0
}
