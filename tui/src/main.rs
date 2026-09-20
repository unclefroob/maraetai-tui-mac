mod app;
mod art;
#[cfg(target_os = "linux")]
mod dbus_client;
mod library;
mod lifecycle;
mod login;
// Compiled (and unit-tested) on every platform — it's plain Unix-socket
// code, nothing macOS-specific about it — but only actually used under
// `target_os = "macos"` (see `lifecycle.rs`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod socket_client;
mod update_check;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use maraetai_common::Credentials;

#[derive(Parser)]
#[command(name = "maraetai", about = "Terminal client for a Navidrome library via maraetai-service")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Save server URL/username, and store the password in the OS keyring.
    Login,
    /// Control the background daemon directly, without opening the TUI.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Play a specific song id directly, without opening the TUI.
    Play {
        song_id: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        artist: Option<String>,
        #[arg(long)]
        album: Option<String>,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Reports whether a daemon is running and what it's doing.
    Status,
    /// Asks a running daemon to shut down cleanly. This is the explicit
    /// "kill it" path — the daemon also shuts itself down automatically
    /// after being idle (see the plan doc).
    Stop,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    match cli.command {
        Some(Command::Login) => login::run(),
        Some(Command::Daemon { action }) => run_daemon_action(action).await,
        Some(Command::Play {
            song_id,
            title,
            artist,
            album,
        }) => run_play(song_id, title, artist, album).await,
        None => run_tui().await,
    }
}

async fn run_daemon_action(action: DaemonAction) -> Result<()> {
    let session = lifecycle::connect_existing().await;
    let proxy = match &session {
        Ok(session) => lifecycle::connect(session).await.ok(),
        Err(_) => None,
    };

    match action {
        DaemonAction::Status => match proxy {
            Some(proxy) => match proxy.status().await {
                Ok((
                    status,
                    title,
                    artist,
                    _album,
                    position,
                    duration,
                    queue_index,
                    queue_len,
                    _volume,
                    format_label,
                    _lossless,
                    _art_url,
                    _song_id,
                    _repeat,
                    _shuffle,
                )) => {
                    if title.is_empty() {
                        println!("daemon running — {status}");
                    } else {
                        let by = if artist.is_empty() { String::new() } else { format!(" by {artist}") };
                        let dur = if duration > 0.0 {
                            format!("{position:.1}s / {duration:.1}s")
                        } else {
                            format!("{position:.1}s")
                        };
                        let fmt = if format_label.is_empty() { String::new() } else { format!(" [{format_label}]") };
                        if queue_len > 0 {
                            println!(
                                "daemon running — {status}: {title}{by} ({dur}){fmt} [{} of {}]",
                                queue_index + 1,
                                queue_len
                            );
                        } else {
                            println!("daemon running — {status}: {title}{by} ({dur}){fmt}");
                        }
                    }
                }
                Err(_) => println!("daemon not running"),
            },
            None => println!("daemon not running"),
        },
        DaemonAction::Stop => match proxy {
            Some(proxy) => {
                proxy.quit().await.context("sending Quit to the daemon")?;
                println!("stop requested");
            }
            None => println!("daemon not running — nothing to stop"),
        },
    }
    Ok(())
}

async fn run_play(
    song_id: String,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
) -> Result<()> {
    let creds = Credentials::load().context("run `maraetai login` first")?;
    let stream_url = library::Client::new(creds).stream_url(&song_id);

    let session = lifecycle::ensure_daemon_running().await?;
    let proxy = lifecycle::connect(&session).await?;
    let track = (
        stream_url,
        title.unwrap_or_default(),
        artist.unwrap_or_default(),
        album.unwrap_or_default(),
        String::new(),
        0.0,
        String::new(), // format unknown — this command only has a raw song id, no library metadata
        false,
        song_id.clone(),
        0.0, // no ReplayGain data available from a bare song id either
    );
    proxy
        .play_queue(vec![track], 0)
        .await
        .context("sending PlayQueue to the daemon")?;
    println!("playing song {song_id}");
    Ok(())
}

async fn run_tui() -> Result<()> {
    let creds = Credentials::load().context("run `maraetai login` first")?;
    let session = lifecycle::ensure_daemon_running().await?;
    let proxy = lifecycle::connect(&session).await?;
    app::run(proxy, creds).await
}
