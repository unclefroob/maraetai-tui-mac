//! The daemon's own control interface (`com.maraetai.Daemon1`), for
//! everything MPRIS doesn't cover: loading a queue of tracks, and explicit
//! shutdown. `Next`/`Previous` are *not* here — they're real MPRIS methods
//! (see `mpris.rs`), since the queue lives in the daemon precisely so that
//! MPRIS's Next/Previous (callable by hardware media keys, with no TUI
//! involved) actually work.
//!
//! Linux-only: this is D-Bus, and D-Bus is Linux's control-channel
//! transport in this project (see `crate::socket_control` for macOS's).
//! The wire-shape conversions (`to_track_meta`/`queue_row`/`status_tuple`)
//! live in `playback.rs` instead of here, shared with `socket_control` —
//! this module is just that logic's D-Bus front door.

use std::sync::Arc;
use std::time::Duration;

use maraetai_common::control_protocol::{QueueEntry, QueueRow, StatusTuple};
use tokio::sync::Notify;
use zbus::interface;

use crate::playback::{PlaybackHandle, queue_row_from_track, status_tuple_from_snapshot, track_meta_from_entry};

pub struct ControlInterface {
    playback: PlaybackHandle,
    /// Notified by `quit()` — `main.rs` awaits this to begin graceful
    /// shutdown (stop the audio thread, release both D-Bus names, exit).
    /// Kept separate from `PlaybackHandle::shutdown()` (which only stops the
    /// *audio thread*) because quitting the daemon is a bigger action than
    /// stopping playback.
    shutdown: Arc<Notify>,
}

impl ControlInterface {
    pub fn new(playback: PlaybackHandle, shutdown: Arc<Notify>) -> Self {
        Self { playback, shutdown }
    }
}

#[interface(name = "com.maraetai.Daemon1")]
impl ControlInterface {
    /// Replaces the queue and starts playing at `start_index` immediately.
    /// A single-track "play just this" is simply a one-entry queue with
    /// `start_index: 0`.
    async fn play_queue(&self, tracks: Vec<QueueEntry>, start_index: u32) {
        let tracks = tracks.into_iter().map(track_meta_from_entry).collect();
        self.playback.play_queue(tracks, start_index as usize);
    }

    /// Jumps directly to `index` within the *current* queue — for a TUI
    /// "Queue" view where the user picks an arbitrary upcoming track,
    /// distinct from `PlayQueue` (which replaces the queue).
    async fn play_at(&self, index: u32) {
        self.playback.play_at(index as usize);
    }

    /// Adds tracks to the end of the queue without interrupting whatever's
    /// already playing — distinct from `PlayQueue`, which always replaces
    /// the queue and starts over.
    async fn append_queue(&self, tracks: Vec<QueueEntry>) {
        let tracks = tracks.into_iter().map(track_meta_from_entry).collect();
        self.playback.append_queue(tracks);
    }

    /// Inserts tracks to play immediately after the current one —
    /// distinct from `append_queue` (goes to the very end) and
    /// `play_queue` (replaces the queue) — for the `A` ("play next") key.
    async fn play_next(&self, tracks: Vec<QueueEntry>) {
        let tracks = tracks.into_iter().map(track_meta_from_entry).collect();
        self.playback.play_next(tracks);
    }

    /// The full current queue, in order — for a TUI "Queue" view. Not a
    /// property (like MPRIS's `Metadata`) since it's a list, not a single
    /// value, and this project doesn't implement MPRIS's TrackList
    /// interface.
    async fn queue(&self) -> Vec<QueueRow> {
        self.playback.snapshot().queue.iter().map(queue_row_from_track).collect()
    }

    /// Removes one track from the queue by its current position — the
    /// Queue view's "remove selected" (`d`). Removing the currently-playing
    /// track skips to whatever now occupies that slot.
    async fn remove_from_queue(&self, index: u32) {
        self.playback.remove_from_queue(index as usize);
    }

    /// Reorders the queue, moving the track at `from` to position `to` —
    /// the Queue view's "move up/down" (`J`/`K`).
    async fn move_in_queue(&self, from: u32, to: u32) {
        self.playback.move_in_queue(from as usize, to as usize);
    }

    /// Empties the queue and stops playback — the Queue view's "clear"
    /// (`D`).
    async fn clear_queue(&self) {
        self.playback.clear_queue();
    }

    /// Current spectrum bar levels, `0..=7` each (see `visualizer.rs`) — for
    /// the TUI's now-playing visualizer. All-zero while nothing is playing.
    async fn spectrum(&self) -> Vec<u8> {
        self.playback.snapshot().spectrum.to_vec()
    }

    /// Also exposed here (identical to MPRIS's own `Next`) purely so the TUI
    /// only needs one D-Bus connection/proxy — real hardware media keys go
    /// through the standard `org.mpris.MediaPlayer2.Player.Next` in
    /// `mpris.rs`, which calls the exact same `PlaybackHandle::next()`.
    async fn next(&self) {
        self.playback.next();
    }

    async fn previous(&self) {
        self.playback.previous();
    }

    async fn pause(&self) {
        self.playback.pause();
    }

    async fn resume(&self) {
        self.playback.resume();
    }

    async fn stop(&self) {
        self.playback.stop();
    }

    /// Seeks to an absolute position, in seconds — distinct from MPRIS's
    /// `Seek` (which is a relative offset).
    async fn seek_to(&self, position_secs: f64) {
        self.playback
            .seek(Duration::from_secs_f64(position_secs.max(0.0)));
    }

    async fn set_volume(&self, volume: f64) {
        self.playback.set_volume(volume.clamp(0.0, 1.0) as f32);
    }

    /// Cycles Off -> Track -> Queue -> Off. The new mode is picked up on the
    /// next `status()` poll rather than returned here, matching every other
    /// mutating method on this interface.
    async fn cycle_repeat(&self) {
        self.playback.cycle_repeat();
    }

    async fn toggle_shuffle(&self) {
        self.playback.toggle_shuffle();
    }

    /// A compact status summary for `maraetai daemon status` and the TUI's
    /// now-playing bar — see `status_tuple_from_snapshot` for the
    /// field-by-field meaning. Kept as a plain method (not properties)
    /// since it's a point-in-time snapshot read by a one-shot CLI command
    /// or a polling loop, not something a D-Bus client watches for changes
    /// — that's what MPRIS's properties (which do emit `PropertiesChanged`)
    /// are for.
    ///
    /// Every connected TUI polls this every ~250ms for as long as it's
    /// open, so it doubles as the daemon's "someone is using this" signal
    /// for the idle timer (see `mark_client_activity`) — otherwise an open
    /// TUI sitting on a paused/stopped track looks identical to no client
    /// at all, and gets shut down out from under it.
    async fn status(&self) -> StatusTuple {
        self.playback.mark_client_activity();
        status_tuple_from_snapshot(&self.playback.snapshot())
    }

    /// Begins graceful daemon shutdown — stops playback, releases both D-Bus
    /// names, and exits the process. This is the explicit "kill it" path
    /// (`maraetai daemon stop`); the idle timer is the automatic one.
    async fn quit(&self) {
        self.shutdown.notify_one();
    }
}
