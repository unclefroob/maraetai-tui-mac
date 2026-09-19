//! Client-side proxy for the daemon's `com.maraetai.Daemon1` control
//! interface. The service/path/interface names are hardcoded literals here
//! (the `#[proxy]` macro needs literals, not `const` references) — the
//! `service_matches_common_dbus_constants` test below guards against these
//! ever drifting from `maraetai_common::dbus`, which stays the source of
//! truth for what the *daemon* actually registers.

pub use maraetai_common::control_protocol::QueueEntry;
use maraetai_common::control_protocol::{QueueRow, StatusTuple};
use zbus::Connection;
use zbus::proxy;

#[proxy(
    interface = "com.maraetai.Daemon1",
    default_service = "com.maraetai.Daemon",
    default_path = "/com/maraetai/Daemon"
)]
pub trait Control {
    async fn play_queue(&self, tracks: Vec<QueueEntry>, start_index: u32) -> zbus::Result<()>;
    async fn play_at(&self, index: u32) -> zbus::Result<()>;
    /// Adds tracks to the end of the queue without interrupting playback —
    /// distinct from `play_queue`, which always replaces the queue.
    async fn append_queue(&self, tracks: Vec<QueueEntry>) -> zbus::Result<()>;
    /// Removes one track from the queue by its current position.
    async fn remove_from_queue(&self, index: u32) -> zbus::Result<()>;
    /// Moves the track at `from` to position `to`, shifting everything
    /// between them.
    async fn move_in_queue(&self, from: u32, to: u32) -> zbus::Result<()>;
    /// Empties the queue and stops playback.
    async fn clear_queue(&self) -> zbus::Result<()>;
    /// One row per track, in order — `song_id` lets the TUI build a
    /// playlist straight from the current queue.
    async fn queue(&self) -> zbus::Result<Vec<QueueRow>>;
    async fn spectrum(&self) -> zbus::Result<Vec<u8>>;
    async fn next(&self) -> zbus::Result<()>;
    async fn previous(&self) -> zbus::Result<()>;
    async fn pause(&self) -> zbus::Result<()>;
    async fn resume(&self) -> zbus::Result<()>;
    async fn stop(&self) -> zbus::Result<()>;
    async fn seek_to(&self, position_secs: f64) -> zbus::Result<()>;
    async fn set_volume(&self, volume: f64) -> zbus::Result<()>;
    /// Cycles Off -> Track -> Queue -> Off.
    async fn cycle_repeat(&self) -> zbus::Result<()>;
    async fn toggle_shuffle(&self) -> zbus::Result<()>;
    async fn status(&self) -> zbus::Result<StatusTuple>;
    async fn quit(&self) -> zbus::Result<()>;
}

/// Connects to the running daemon's control interface. Returns an error if
/// no daemon currently owns the bus name — callers use this to decide
/// whether to auto-spawn one (see `lifecycle::ensure_daemon_running`).
pub async fn connect(connection: &Connection) -> zbus::Result<ControlProxy<'_>> {
    ControlProxy::new(connection).await
}

#[cfg(test)]
mod tests {
    #[test]
    fn service_matches_common_dbus_constants() {
        assert_eq!(maraetai_common::dbus::CONTROL_BUS_NAME, "com.maraetai.Daemon");
        assert_eq!(maraetai_common::dbus::CONTROL_OBJECT_PATH, "/com/maraetai/Daemon");
        assert_eq!(maraetai_common::dbus::CONTROL_INTERFACE, "com.maraetai.Daemon1");
    }
}
