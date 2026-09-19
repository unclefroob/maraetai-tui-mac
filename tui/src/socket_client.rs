//! Client-side transport for the daemon's control channel on macOS — a
//! newline-delimited-JSON protocol over a Unix socket (see the daemon's
//! `socket_control`), used instead of D-Bus because macOS has no session
//! bus available by default. Exposes the exact same method surface as
//! `dbus_client::ControlProxy` (same names, same `Result<T>` shapes) so
//! `app.rs`/`lifecycle.rs` don't need to know which transport is active —
//! see `main.rs`'s platform split.
//!
//! Pure Unix-socket code — nothing here is actually macOS-specific — so
//! it's compiled and unit-tested on every platform; only `main.rs` decides
//! which platform actually uses it.

use std::marker::PhantomData;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
pub use maraetai_common::control_protocol::QueueEntry;
use maraetai_common::control_protocol::{QueueRow, Request, Response, StatusTuple};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Mutex;

struct Inner {
    writer: OwnedWriteHalf,
    reader: BufReader<OwnedReadHalf>,
}

/// A connected control-channel client. Carries an unused lifetime purely so
/// `app.rs`/`lifecycle.rs` can hold this behind the exact same
/// `ControlProxy<'a>` shape `dbus_client`'s D-Bus proxy has — nothing here
/// actually borrows anything, the stream is owned outright.
#[derive(Clone)]
pub struct ControlProxy<'a> {
    inner: Arc<Mutex<Inner>>,
    _marker: PhantomData<&'a ()>,
}

impl ControlProxy<'_> {
    /// Sends one request and waits for its response — writes and the
    /// matching read happen under the same lock, so a proxy shared/cloned
    /// across tasks can't have two calls' requests and responses
    /// interleave on the wire (in practice only one call is ever in flight
    /// at a time, but this makes that an actual guarantee, not an
    /// assumption).
    async fn call(&self, request: Request) -> Result<Response> {
        let mut json = serde_json::to_string(&request).context("encoding request")?;
        json.push('\n');
        let mut inner = self.inner.lock().await;
        inner.writer.write_all(json.as_bytes()).await.context("writing to control socket")?;
        let mut line = String::new();
        let n = inner.reader.read_line(&mut line).await.context("reading from control socket")?;
        if n == 0 {
            bail!("daemon closed the control socket");
        }
        serde_json::from_str(&line).context("decoding response")
    }

    async fn call_unit(&self, request: Request) -> Result<()> {
        match self.call(request).await? {
            Response::Unit => Ok(()),
            Response::Error(e) => bail!(e),
            _ => bail!("unexpected response shape for this request"),
        }
    }

    pub async fn play_queue(&self, tracks: Vec<QueueEntry>, start_index: u32) -> Result<()> {
        self.call_unit(Request::PlayQueue { tracks, start_index }).await
    }
    pub async fn play_at(&self, index: u32) -> Result<()> {
        self.call_unit(Request::PlayAt { index }).await
    }
    pub async fn append_queue(&self, tracks: Vec<QueueEntry>) -> Result<()> {
        self.call_unit(Request::AppendQueue { tracks }).await
    }
    pub async fn remove_from_queue(&self, index: u32) -> Result<()> {
        self.call_unit(Request::RemoveFromQueue { index }).await
    }
    pub async fn move_in_queue(&self, from: u32, to: u32) -> Result<()> {
        self.call_unit(Request::MoveInQueue { from, to }).await
    }
    pub async fn clear_queue(&self) -> Result<()> {
        self.call_unit(Request::ClearQueue).await
    }
    pub async fn queue(&self) -> Result<Vec<QueueRow>> {
        match self.call(Request::Queue).await? {
            Response::Queue(q) => Ok(q),
            Response::Error(e) => bail!(e),
            _ => bail!("unexpected response shape for this request"),
        }
    }
    pub async fn spectrum(&self) -> Result<Vec<u8>> {
        match self.call(Request::Spectrum).await? {
            Response::Spectrum(s) => Ok(s),
            Response::Error(e) => bail!(e),
            _ => bail!("unexpected response shape for this request"),
        }
    }
    pub async fn next(&self) -> Result<()> {
        self.call_unit(Request::Next).await
    }
    pub async fn previous(&self) -> Result<()> {
        self.call_unit(Request::Previous).await
    }
    pub async fn pause(&self) -> Result<()> {
        self.call_unit(Request::Pause).await
    }
    pub async fn resume(&self) -> Result<()> {
        self.call_unit(Request::Resume).await
    }
    pub async fn stop(&self) -> Result<()> {
        self.call_unit(Request::Stop).await
    }
    pub async fn seek_to(&self, position_secs: f64) -> Result<()> {
        self.call_unit(Request::SeekTo { position_secs }).await
    }
    pub async fn set_volume(&self, volume: f64) -> Result<()> {
        self.call_unit(Request::SetVolume { volume }).await
    }
    pub async fn cycle_repeat(&self) -> Result<()> {
        self.call_unit(Request::CycleRepeat).await
    }
    pub async fn toggle_shuffle(&self) -> Result<()> {
        self.call_unit(Request::ToggleShuffle).await
    }
    pub async fn status(&self) -> Result<StatusTuple> {
        match self.call(Request::Status).await? {
            Response::Status(s) => Ok(s),
            Response::Error(e) => bail!(e),
            _ => bail!("unexpected response shape for this request"),
        }
    }
    pub async fn quit(&self) -> Result<()> {
        self.call_unit(Request::Quit).await
    }
}

/// Connects to the daemon's control socket. Returns an error if nothing is
/// listening — callers use this to decide whether to auto-spawn one (see
/// `lifecycle::ensure_daemon_running`).
pub async fn connect() -> Result<ControlProxy<'static>> {
    let path = maraetai_common::paths::control_socket_path();
    let stream = UnixStream::connect(&path).await.with_context(|| format!("connecting to {}", path.display()))?;
    let (read_half, write_half) = stream.into_split();
    let inner = Inner { writer: write_half, reader: BufReader::new(read_half) };
    Ok(ControlProxy { inner: Arc::new(Mutex::new(inner)), _marker: PhantomData })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    /// A unique-per-test socket path — several of these tests run
    /// concurrently in the same process, so a fixed name would collide.
    fn test_socket_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("maraetai-socket-client-test-{label}-{}.sock", std::process::id()))
    }

    /// Exercises the real client against a minimal stand-in server (not the
    /// daemon's actual `socket_control`, which needs a live `PlaybackHandle`
    /// — this only needs to prove the wire framing/round-trip works) —
    /// confirms `status()` actually sends one JSON line and parses the
    /// reply back into a `StatusTuple`.
    #[tokio::test]
    async fn status_round_trips_over_a_real_unix_socket() {
        let path = test_socket_path("status");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();

        tokio::spawn({
            let path = path.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                let line = lines.next_line().await.unwrap().unwrap();
                let request: Request = serde_json::from_str(&line).unwrap();
                assert!(matches!(request, Request::Status));
                let status: StatusTuple = (
                    "playing".into(),
                    "Angel".into(),
                    "Massive Attack".into(),
                    "Mezzanine".into(),
                    1.0,
                    2.0,
                    0,
                    1,
                    1.0,
                    "FLAC".into(),
                    true,
                    String::new(),
                    "song1".into(),
                    "off".into(),
                    false,
                );
                let mut reply = serde_json::to_string(&Response::Status(status)).unwrap();
                reply.push('\n');
                writer.write_all(reply.as_bytes()).await.unwrap();
                let _ = std::fs::remove_file(&path);
            }
        });

        let proxy = connect_to(&path).await.unwrap();
        let status = proxy.status().await.unwrap();
        assert_eq!(status.0, "playing");
        assert_eq!(status.1, "Angel");
        assert_eq!(status.12, "song1");
    }

    #[tokio::test]
    async fn an_error_response_surfaces_as_an_error() {
        let path = test_socket_path("error");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();

        tokio::spawn({
            let path = path.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                let _ = lines.next_line().await.unwrap().unwrap();
                let mut reply = serde_json::to_string(&Response::Error("no such track".into())).unwrap();
                reply.push('\n');
                writer.write_all(reply.as_bytes()).await.unwrap();
                let _ = std::fs::remove_file(&path);
            }
        });

        let proxy = connect_to(&path).await.unwrap();
        let err = proxy.play_at(99).await.unwrap_err();
        assert!(err.to_string().contains("no such track"));
    }

    /// Test-only variant of `connect()` that takes an explicit path instead
    /// of always using the real `control_socket_path()` — lets these tests
    /// use their own throwaway sockets without touching (or needing) the
    /// real runtime directory.
    async fn connect_to(path: &std::path::Path) -> Result<ControlProxy<'static>> {
        let stream = UnixStream::connect(path).await?;
        let (read_half, write_half) = stream.into_split();
        let inner = Inner { writer: write_half, reader: BufReader::new(read_half) };
        Ok(ControlProxy { inner: Arc::new(Mutex::new(inner)), _marker: PhantomData })
    }
}
