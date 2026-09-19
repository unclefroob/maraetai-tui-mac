//! The daemon has two control-channel transports, picked at compile time
//! by target OS: D-Bus (`control`/`mpris`, Linux — a session bus is always
//! available there) or a Unix-socket protocol (`socket_control`, macOS —
//! no session bus exists by default, so this needs no setup step at all).
//! MPRIS rides along with D-Bus specifically, since it's a D-Bus interface
//! by definition and nothing on macOS reads it anyway (no desktop widget
//! there consumes MPRIS). Everything else — the playback engine, lifecycle,
//! scrobbling — is fully shared, unaware which transport is in use.

#[cfg(target_os = "linux")]
mod control;
mod lifecycle;
#[cfg(target_os = "linux")]
mod mpris;
mod playback;
mod range_reader;
// Compiled (and unit-tested) on every platform — it's plain Unix-socket
// code, nothing macOS-specific about it — but only actually wired into
// `main()` below under `target_os = "macos"`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod socket_control;
mod visualizer;

use std::sync::Arc;

use anyhow::{Context, Result};
use maraetai_common::{Config, Credentials};
use tokio::sync::Notify;

use playback::Event;

#[cfg(target_os = "linux")]
use maraetai_common::dbus;
#[cfg(target_os = "linux")]
use mpris_server::{PlayerInterface, Property, Server, Signal, Time};
#[cfg(target_os = "linux")]
use zbus::connection;

#[cfg(target_os = "linux")]
use control::ControlInterface;
#[cfg(target_os = "linux")]
use mpris::MprisPlayer;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let _instance_guard = lifecycle::acquire_single_instance_lock()
        .context("could not start maraetaid — is another instance already running?")?;

    // Config/credentials are loaded but not *required* to start: the daemon
    // should still come up (and answer `status`/be reachable) even before
    // `maraetai login` has been run, rather than crash-loop on a fresh
    // install. The idle timeout still applies either way — falling back to
    // Config::idle_timeout's built-in default when unconfigured.
    let idle_timeout = Config::load().map(|c| c.idle_timeout()).unwrap_or_else(|_| {
        std::time::Duration::from_secs(maraetai_common::config::DEFAULT_IDLE_TIMEOUT_SECS)
    });
    let credentials = match Credentials::load() {
        Ok(creds) => {
            tracing::info!(server = %creds.server_url, user = %creds.username, "credentials loaded");
            Some(creds)
        }
        Err(e) => {
            tracing::warn!("not configured yet ({e}) — run `maraetai login`");
            None
        }
    };

    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let playback = playback::spawn(event_tx, credentials);
    let shutdown = Arc::new(Notify::new());

    #[cfg(target_os = "linux")]
    {
        let mpris_server = Arc::new(
            Server::new(
                dbus::MPRIS_BUS_NAME_SUFFIX,
                MprisPlayer {
                    playback: playback.clone(),
                },
            )
            .await
            .context("failed to register MPRIS D-Bus service")?,
        );

        let control = ControlInterface::new(playback.clone(), Arc::clone(&shutdown));
        let _control_connection = connection::Builder::session()?
            .name(dbus::CONTROL_BUS_NAME)?
            .serve_at(dbus::CONTROL_OBJECT_PATH, control)?
            .build()
            .await
            .context("failed to register control D-Bus service")?;

        tokio::spawn(bridge_playback_events_to_mpris(
            event_rx,
            Arc::clone(&mpris_server),
        ));

        tracing::info!(
            mpris_bus = %mpris_server.bus_name(),
            control_bus = dbus::CONTROL_BUS_NAME,
            idle_timeout = ?idle_timeout,
            "maraetaid ready"
        );
    }

    #[cfg(target_os = "macos")]
    {
        let socket_path = maraetai_common::paths::control_socket_path();
        tokio::spawn(socket_control::serve(socket_path.clone(), playback.clone(), Arc::clone(&shutdown)));

        // No MPRIS bridge on macOS (see the module doc) — the playback
        // engine's events still need draining so the unbounded channel
        // doesn't grow forever with nothing ever calling `recv()`.
        tokio::spawn(async move {
            let mut events = event_rx;
            while events.recv().await.is_some() {}
        });

        tracing::info!(control_socket = %socket_path.display(), idle_timeout = ?idle_timeout, "maraetaid ready");
    }

    tokio::spawn(lifecycle::run_idle_timer(
        playback.clone(),
        Arc::clone(&shutdown),
        idle_timeout,
    ));

    tokio::select! {
        _ = shutdown.notified() => tracing::info!("shutdown requested"),
        _ = tokio::signal::ctrl_c() => tracing::info!("received Ctrl-C"),
        _ = wait_for_sigterm() => tracing::info!("received SIGTERM"),
    }

    playback.shutdown();
    Ok(())
}

/// Drains playback engine events and turns the ones that matter to MPRIS
/// clients into `PropertiesChanged`/`Seeked` signals. The engine itself only
/// updates a plain shared `Snapshot` (cheap, no async) — this task is the
/// only place that pays the cost of an actual D-Bus signal emission, and only
/// when something observable actually changed. Linux-only, same as MPRIS
/// itself.
#[cfg(target_os = "linux")]
async fn bridge_playback_events_to_mpris(
    mut events: tokio::sync::mpsc::UnboundedReceiver<Event>,
    server: Arc<Server<MprisPlayer>>,
) {
    while let Some(event) = events.recv().await {
        match event {
            Event::StatusChanged(status) => {
                let mpris_status = match status {
                    playback::Status::Playing => mpris_server::PlaybackStatus::Playing,
                    playback::Status::Paused => mpris_server::PlaybackStatus::Paused,
                    playback::Status::Stopped => mpris_server::PlaybackStatus::Stopped,
                };
                if let Err(e) = server
                    .properties_changed([Property::PlaybackStatus(mpris_status)])
                    .await
                {
                    tracing::warn!("failed to emit PlaybackStatus change: {e}");
                }
            }
            Event::TrackChanged => {
                if let Ok(meta) = server.imp().metadata().await {
                    if let Err(e) = server.properties_changed([Property::Metadata(meta)]).await {
                        tracing::warn!("failed to emit Metadata change: {e}");
                    }
                }
            }
            Event::Seeked(pos) => {
                if let Err(e) = server
                    .emit(Signal::Seeked {
                        position: Time::from_micros(pos.as_micros() as i64),
                    })
                    .await
                {
                    tracing::warn!("failed to emit Seeked signal: {e}");
                }
            }
            Event::QueueEnded => {
                tracing::debug!("queue ended");
            }
            Event::PlaybackError(msg) => {
                tracing::warn!("playback error: {msg}");
            }
        }
    }
}

async fn wait_for_sigterm() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut sig) => {
            sig.recv().await;
        }
        Err(e) => {
            tracing::warn!("could not install SIGTERM handler: {e}");
            std::future::pending::<()>().await;
        }
    }
}
