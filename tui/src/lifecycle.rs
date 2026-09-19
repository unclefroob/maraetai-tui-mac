//! Auto-spawn + connect to the daemon's control channel: D-Bus on Linux (a
//! session bus is always available on a normal desktop), a Unix-socket
//! protocol on macOS (no session bus there by default — see `socket_client`
//! and the daemon's `socket_control`). `Session` hides which one is active;
//! `main.rs` and `app.rs` only ever deal in `Session`/`ControlProxy`, never
//! the underlying transport.

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};

#[cfg(target_os = "linux")]
use crate::dbus_client::{self, ControlProxy};
#[cfg(target_os = "macos")]
use crate::socket_client::{self, ControlProxy};

const SPAWN_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const SPAWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Whatever a platform's transport needs kept alive for its proxies to stay
/// valid: a real D-Bus `Connection` on Linux (the proxy borrows from it),
/// nothing at all on macOS (each socket proxy owns its stream outright).
#[cfg(target_os = "linux")]
pub struct Session(zbus::Connection);
#[cfg(target_os = "macos")]
pub struct Session;

/// Builds a proxy against an already-established `Session`.
pub async fn connect(session: &Session) -> Result<ControlProxy<'_>> {
    #[cfg(target_os = "linux")]
    {
        dbus_client::connect(&session.0).await.context("connecting to daemon control interface")
    }
    #[cfg(target_os = "macos")]
    {
        let _ = session; // nothing to borrow from on this platform
        socket_client::connect().await
    }
}

/// Establishes a `Session` against whatever transport this platform uses —
/// on Linux this always succeeds as long as *a* D-Bus session bus exists,
/// regardless of whether our daemon is actually on it (same as macOS: it
/// always succeeds regardless of whether anything is listening on the
/// socket yet). Real liveness is `connect(&session)` plus an actual RPC —
/// see `ensure_daemon_running`.
async fn establish_session() -> Result<Session> {
    #[cfg(target_os = "linux")]
    {
        Ok(Session(zbus::Connection::session().await.context("connecting to the D-Bus session bus")?))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Session)
    }
}

/// Establishes a session and confirms a daemon is actually reachable on it
/// — no auto-spawn. For `maraetai daemon status`/`stop`, which must report
/// "not running" rather than start one just to ask.
pub async fn connect_existing() -> Result<Session> {
    let session = establish_session().await?;
    if daemon_is_alive(&session).await {
        Ok(session)
    } else {
        bail!("no daemon is currently running")
    }
}

/// Makes sure a daemon is reachable, spawning one if not, and returns a
/// `Session` to build proxies against. Liveness is checked with a real RPC
/// (`status()`), not just whether the transport itself is available — a
/// D-Bus `Proxy`/socket connection can both be constructed successfully
/// even when nothing is actually listening as our daemon yet.
pub async fn ensure_daemon_running() -> Result<Session> {
    let session = establish_session().await?;
    if daemon_is_alive(&session).await {
        return Ok(session);
    }

    tracing::info!("no daemon running — spawning maraetaid");
    let exe = daemon_binary_path()?;
    std::process::Command::new(&exe)
        .stdin(Stdio::null())
        .stdout(daemon_log_stdio())
        .stderr(daemon_log_stdio())
        .spawn()
        .with_context(|| format!("failed to spawn {}", exe.display()))?;

    let deadline = tokio::time::Instant::now() + SPAWN_WAIT_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(SPAWN_POLL_INTERVAL).await;
        if daemon_is_alive(&session).await {
            return Ok(session);
        }
    }

    bail!(
        "daemon did not become reachable within {:?} of starting {}",
        SPAWN_WAIT_TIMEOUT,
        exe.display()
    );
}

/// A fresh handle onto the daemon's log file, in append mode — never the
/// TUI's own stdout/stderr. Without this, the auto-spawned daemon inherits
/// the TUI's terminal directly, and raw libasound diagnostics (e.g. a PCM
/// underrun) bypass our own logging entirely and get written straight into
/// the alternate screen, corrupting the display. Falls back to discarding
/// output entirely if the log file can't be opened, rather than falling
/// back to inheriting the terminal (which is the exact problem being
/// avoided here).
fn daemon_log_stdio() -> Stdio {
    let log_path = maraetai_common::paths::runtime_dir().join("daemon.log");
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_or_else(|_| Stdio::null(), Stdio::from)
}

async fn daemon_is_alive(session: &Session) -> bool {
    match connect(session).await {
        Ok(proxy) => proxy.status().await.is_ok(),
        Err(_) => false,
    }
}

/// Locates the `maraetaid` binary: alongside our own executable first (the
/// normal case — a workspace build puts both binaries in the same
/// `target/{debug,release}` directory), falling back to relying on `PATH`
/// (the case once this is actually installed system-wide) so `Command::spawn`
/// can still find it and produce a clear "not found" error if not.
fn daemon_binary_path() -> Result<PathBuf> {
    if let Ok(mut exe) = std::env::current_exe() {
        exe.set_file_name("maraetaid");
        if exe.exists() {
            return Ok(exe);
        }
    }
    Ok(PathBuf::from("maraetaid"))
}
