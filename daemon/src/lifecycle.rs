//! Daemon lifecycle: single-instance enforcement via a locked pidfile, and
//! the idle-shutdown timer that's the actual point of this whole design (see
//! the plan doc) — nothing here should let the daemon sit around consuming
//! resources when nobody wants it running.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use nix::fcntl::{Flock, FlockArg};
use tokio::sync::Notify;

use crate::playback::PlaybackHandle;

/// Held for the daemon's entire lifetime. Dropping it releases the `flock`,
/// so a crashed process (which drops everything on exit, lock included)
/// never leaves a stale lock behind — only a stale *pidfile*, which the next
/// daemon simply overwrites once it acquires the lock.
pub struct SingleInstanceGuard {
    _locked_pidfile: Flock<File>,
}

/// Acquires the single-instance lock, returning an error (not panicking) if
/// another `maraetaid` already holds it — the TUI's auto-spawn path needs to
/// distinguish "already running, fine" from "something actually went wrong".
pub fn acquire_single_instance_lock() -> Result<SingleInstanceGuard> {
    let path = maraetai_common::paths::pidfile_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating runtime dir {}", dir.display()))?;
    }

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("opening pidfile {}", path.display()))?;

    let mut locked = Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_, errno)| {
        anyhow!(
            "another maraetaid is already running (pidfile {} is locked: {errno})",
            path.display()
        )
    })?;

    write!(locked, "{}", std::process::id())
        .with_context(|| format!("writing pid to {}", path.display()))?;

    Ok(SingleInstanceGuard {
        _locked_pidfile: locked,
    })
}

/// Runs forever, notifying `shutdown` once the daemon has been idle (nothing
/// playing, no D-Bus control activity) for `timeout`. This is the *automatic*
/// half of "don't perpetually use resources if unwanted" — `quit()` in
/// `control.rs` is the manual half.
///
/// Deliberately checks `playback.is_playing()` on every tick, not just
/// `take_activity()`: closing the terminal (so nothing is calling into the
/// daemon any more) must not stop music that's still playing — "idle" means
/// no playback *and* no client interaction, not just no client. "Client
/// interaction" specifically includes an open TUI merely polling `status()`
/// (see `ControlInterface::status`/`PlaybackHandle::mark_client_activity`) —
/// otherwise a TUI left open on a paused/stopped track for `timeout` looks
/// identical to no client at all, and gets shut down out from under it.
pub async fn run_idle_timer(playback: PlaybackHandle, shutdown: Arc<Notify>, timeout: Duration) {
    let check_interval = Duration::from_secs(30).min(timeout);
    let mut idle_for = Duration::ZERO;

    loop {
        tokio::time::sleep(check_interval).await;

        let active = playback.take_activity() || playback.is_playing();
        if active {
            idle_for = Duration::ZERO;
            continue;
        }

        idle_for += check_interval;
        if idle_for >= timeout {
            tracing::info!(
                "idle for {:?} with nothing playing — shutting down",
                idle_for
            );
            shutdown.notify_one();
            return;
        }
    }
}
