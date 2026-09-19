//! macOS's control-channel transport: a newline-delimited-JSON protocol
//! over a Unix domain socket (`maraetai_common::paths::control_socket_path`),
//! used instead of D-Bus because macOS has no session bus available by
//! default — see `main.rs`'s platform split. Dispatches to exactly the same
//! `PlaybackHandle` and wire-shape conversions (`track_meta_from_entry`
//! etc., in `playback.rs`) that the Linux `control.rs`/D-Bus transport
//! uses, so the two transports can't drift in behavior, only in wire
//! format.
//!
//! Pure Unix-socket code — nothing here is actually macOS-specific (Unix
//! domain sockets are the same POSIX API on Linux), so this module is
//! compiled and unit-tested on every platform. Only `main.rs` decides
//! which platform actually *runs* it.

use std::path::PathBuf;
use std::sync::Arc;

use maraetai_common::control_protocol::{Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;

use crate::playback::{PlaybackHandle, queue_row_from_track, status_tuple_from_snapshot, track_meta_from_entry};

/// Binds the control socket and serves connections until `shutdown` fires.
/// One task per connection, each served strictly request-then-response (the
/// TUI only ever has one call in flight at a time), so no request-id
/// correlation is needed on the wire.
pub async fn serve(socket_path: PathBuf, playback: PlaybackHandle, shutdown: Arc<Notify>) -> std::io::Result<()> {
    // A stale socket file from a crashed previous run must not block
    // binding a fresh one — the daemon's single-instance pidfile lock
    // (`lifecycle::acquire_single_instance_lock`) already guarantees only
    // one daemon is alive at a time, so it's always safe to remove
    // whatever's sitting at this path before binding.
    let _ = std::fs::remove_file(&socket_path);
    if let Some(dir) = socket_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let listener = UnixListener::bind(&socket_path)?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                tokio::spawn(handle_connection(stream, playback.clone(), Arc::clone(&shutdown)));
            }
            () = shutdown.notified() => return Ok(()),
        }
    }
}

async fn handle_connection(stream: UnixStream, playback: PlaybackHandle, shutdown: Arc<Notify>) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => return, // client disconnected
            Err(_) => return,
        };
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => dispatch(request, &playback, &shutdown),
            Err(e) => Response::Error(format!("bad request: {e}")),
        };
        let Ok(mut encoded) = serde_json::to_string(&response) else {
            continue; // never actually fails for these types, but not worth a panic if it somehow did
        };
        encoded.push('\n');
        if writer.write_all(encoded.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// One request in, one response out — every branch mirrors a
/// `ControlInterface` method in `control.rs` exactly (same `PlaybackHandle`
/// calls, same conversions), just addressed by an enum tag instead of a
/// D-Bus method name.
fn dispatch(request: Request, playback: &PlaybackHandle, shutdown: &Arc<Notify>) -> Response {
    match request {
        Request::PlayQueue { tracks, start_index } => {
            playback.play_queue(tracks.into_iter().map(track_meta_from_entry).collect(), start_index as usize);
            Response::Unit
        }
        Request::PlayAt { index } => {
            playback.play_at(index as usize);
            Response::Unit
        }
        Request::AppendQueue { tracks } => {
            playback.append_queue(tracks.into_iter().map(track_meta_from_entry).collect());
            Response::Unit
        }
        Request::Queue => {
            Response::Queue(playback.snapshot().queue.iter().map(queue_row_from_track).collect())
        }
        Request::RemoveFromQueue { index } => {
            playback.remove_from_queue(index as usize);
            Response::Unit
        }
        Request::MoveInQueue { from, to } => {
            playback.move_in_queue(from as usize, to as usize);
            Response::Unit
        }
        Request::ClearQueue => {
            playback.clear_queue();
            Response::Unit
        }
        Request::Spectrum => Response::Spectrum(playback.snapshot().spectrum.to_vec()),
        Request::Next => {
            playback.next();
            Response::Unit
        }
        Request::Previous => {
            playback.previous();
            Response::Unit
        }
        Request::Pause => {
            playback.pause();
            Response::Unit
        }
        Request::Resume => {
            playback.resume();
            Response::Unit
        }
        Request::Stop => {
            playback.stop();
            Response::Unit
        }
        Request::SeekTo { position_secs } => {
            playback.seek(std::time::Duration::from_secs_f64(position_secs.max(0.0)));
            Response::Unit
        }
        Request::SetVolume { volume } => {
            playback.set_volume(volume.clamp(0.0, 1.0) as f32);
            Response::Unit
        }
        Request::CycleRepeat => {
            playback.cycle_repeat();
            Response::Unit
        }
        Request::ToggleShuffle => {
            playback.toggle_shuffle();
            Response::Unit
        }
        // Every connected TUI polls this every ~250ms for as long as it's
        // open, so it doubles as the daemon's "someone is using this"
        // signal for the idle timer (see `mark_client_activity`) —
        // otherwise an open TUI sitting on a paused/stopped track looks
        // identical to no client at all, and gets shut down out from
        // under it.
        Request::Status => {
            playback.mark_client_activity();
            Response::Status(status_tuple_from_snapshot(&playback.snapshot()))
        }
        Request::Quit => {
            shutdown.notify_one();
            Response::Unit
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire format is newline-delimited JSON — this locks that shape
    /// down directly (not just "it round-trips"), since a client on the
    /// other end of the socket depends on exactly one `Request`/`Response`
    /// per line.
    #[test]
    fn a_request_serializes_as_one_json_line_with_no_embedded_newline() {
        let json = serde_json::to_string(&Request::Next).unwrap();
        assert!(!json.contains('\n'));
        assert!(serde_json::from_str::<Request>(&json).is_ok());
    }
}
