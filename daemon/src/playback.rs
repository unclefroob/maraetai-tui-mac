//! The playback engine: a dedicated OS thread owning the audio output device,
//! decode pipeline, and the queue. It's a plain thread (not async) because
//! `rodio`'s `OutputStream`/`Sink` wrap a `cpal` audio callback that must
//! live on the thread that created it — bridging that into the daemon's
//! async (tokio) world happens via two channels: a `Command` channel in, and
//! either a shared `Snapshot` (for cheap, frequent reads like MPRIS's
//! `Position` getter) or an `Event` channel out (for state transitions worth
//! reacting to, like emitting an MPRIS `PropertiesChanged` signal).
//!
//! The queue lives here (in the daemon), not in the TUI, so that MPRIS's
//! `Next`/`Previous` — which real hardware media keys and desktop widgets
//! call directly over D-Bus, with no TUI involved at all — actually do
//! something. A client-side-only queue would leave those buttons inert,
//! which defeats a core point of building MPRIS support in the first place.

use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use maraetai_common::Credentials;
use maraetai_common::auth::AuthParams;
use maraetai_common::control_protocol::{QueueEntry, QueueRow, StatusTuple};
use rand::seq::SliceRandom;
use rodio::{Decoder, OutputStream, OutputStreamBuilder, Sink, Source};
use tokio::sync::mpsc::UnboundedSender;

use crate::range_reader::RangeReader;
use crate::visualizer::{self, SpectrumAnalyzer, VisualizerTap};

/// A track "counts" as scrobbled once at least half its duration (capped at
/// 4 minutes) has played — the classic Last.fm/AudioScrobbler threshold most
/// Subsonic-compatible clients use, so a skip-after-a-few-seconds doesn't
/// record a play but letting most of a track run does.
fn scrobble_threshold(duration: Duration) -> Duration {
    (duration / 2).min(Duration::from_secs(4 * 60))
}

/// How often the engine polls its own `Sink` for position/end-of-track while
/// idle-waiting on the command channel. Small enough that MPRIS `Seeked`
/// consumers and the idle timer both feel responsive; large enough not to
/// matter for CPU (this thread otherwise blocks entirely on `recv_timeout`).
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// A seek-to-current-track-restart (via `Previous`) counts as "already at the
/// start" below this position — matches the common player convention of
/// "previous" restarting a track you're partway through rather than always
/// jumping back a full track.
const RESTART_THRESHOLD: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Stopped,
    Playing,
    Paused,
}

/// `Track` loops the current track on natural end (not on a manual `Next` —
/// that still advances normally); `Queue` loops back to the start of the
/// queue (or a fresh shuffle of it, if shuffle is also on) once the last
/// track finishes, instead of stopping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepeatMode {
    #[default]
    Off,
    Track,
    Queue,
}

impl RepeatMode {
    fn cycle(self) -> Self {
        match self {
            RepeatMode::Off => RepeatMode::Track,
            RepeatMode::Track => RepeatMode::Queue,
            RepeatMode::Queue => RepeatMode::Off,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RepeatMode::Off => "off",
            RepeatMode::Track => "track",
            RepeatMode::Queue => "queue",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TrackMeta {
    pub stream_url: String,
    /// The Subsonic song id — needed to scrobble this track (maraetai-service
    /// records a play by id, not by title/artist) and to look up lyrics by
    /// id. Empty for a track played without library metadata (e.g. `maraetai
    /// play <song-id>` with no other lookup) — scrobbling and id-based
    /// lyrics are simply skipped for those.
    pub song_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art_url: Option<String>,
    /// Known/expected duration, if the caller has it (from library metadata)
    /// — independent of whatever `RangeReader` eventually learns from the
    /// HTTP response, since the engine reports playback *position*, not a
    /// duration derived from decoding.
    pub duration: Option<Duration>,
    /// A pre-formatted display label (e.g. "FLAC", "MP3 320") and whether
    /// it's lossless — computed client-side (see the TUI's
    /// `library::format_label`, the single source of truth for the
    /// format/lossless rules) and carried through as an opaque label rather
    /// than duplicating that logic here.
    pub format_label: String,
    pub lossless: bool,
    /// ReplayGain, in dB — the TUI resolves a `Song`'s track/album gain
    /// down to this one value (see `library::Song::replay_gain_db`)
    /// before it ever reaches here. `0.0` when the server sent neither, or
    /// for a track played without library metadata at all — a plain `f64`
    /// rather than `Option<f64>` since zvariant can't encode the latter
    /// over classic D-Bus, and a real 0 dB tag would mean the same "no
    /// adjustment" outcome anyway. See `replay_gain_scale`.
    pub replay_gain_db: f64,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub status: Status,
    pub position: Duration,
    pub track: Option<TrackMeta>,
    pub volume: f32,
    /// 0-based position within the queue, and the queue's length — together
    /// enough for a "track 3 of 12" display and for `CanGoNext`/
    /// `CanGoPrevious`, without exposing the whole queue to every consumer.
    pub queue_index: usize,
    pub queue_len: usize,
    /// The full queue, in order — for a TUI "Queue" view. Cloned into the
    /// snapshot only when the queue actually changes (`PlayQueue`), not on
    /// every poll tick, so this doesn't add per-tick cost.
    pub queue: Vec<TrackMeta>,
    /// Current spectrum bar levels (0..=`visualizer::MAX_LEVEL` each),
    /// recomputed from real decoded audio every poll tick — see
    /// `visualizer.rs`. All-zero while stopped/paused or right after a
    /// track/seek change (not yet enough buffered samples).
    pub spectrum: [u8; visualizer::BARS],
    pub repeat: RepeatMode,
    pub shuffle: bool,
}

impl Snapshot {
    pub fn has_next(&self) -> bool {
        self.queue_index + 1 < self.queue_len || self.repeat == RepeatMode::Queue
    }
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            status: Status::Stopped,
            position: Duration::ZERO,
            track: None,
            volume: 1.0,
            queue_index: 0,
            queue_len: 0,
            queue: Vec::new(),
            spectrum: [0; visualizer::BARS],
            repeat: RepeatMode::Off,
            shuffle: false,
        }
    }
}

/// Converts one wire-format queue entry into the engine's own `TrackMeta` —
/// shared by both control-channel transports (D-Bus's `PlayQueue`/
/// `AppendQueue` and the socket transport's equivalents) so they can't
/// disagree on the mapping.
pub fn track_meta_from_entry(entry: QueueEntry) -> TrackMeta {
    let (stream_url, title, artist, album, art_url, duration_secs, format_label, lossless, song_id) = entry;
    TrackMeta {
        stream_url,
        song_id,
        title,
        artist,
        album,
        art_url: (!art_url.is_empty()).then_some(art_url),
        duration: (duration_secs > 0.0).then(|| Duration::from_secs_f64(duration_secs)),
        format_label,
        lossless,
    }
}

/// The reverse of [`track_meta_from_entry`], for one queue row's *display*
/// shape — shared by both transports' `queue()`/`Queue` handlers.
pub fn queue_row_from_track(t: &TrackMeta) -> QueueRow {
    (
        t.title.clone(),
        t.artist.clone(),
        t.album.clone(),
        t.duration.map(|d| d.as_secs_f64()).unwrap_or(0.0),
        t.format_label.clone(),
        t.lossless,
        t.song_id.clone(),
    )
}

/// A point-in-time `Snapshot` flattened into the wire's `status()` shape —
/// shared by both transports' `status()`/`Status` handlers.
pub fn status_tuple_from_snapshot(snap: &Snapshot) -> StatusTuple {
    let status = match snap.status {
        Status::Playing => "playing",
        Status::Paused => "paused",
        Status::Stopped => "stopped",
    };
    let (title, artist, album, duration, format_label, lossless, art_url, song_id) = match &snap.track {
        Some(t) => (
            t.title.clone(),
            t.artist.clone(),
            t.album.clone(),
            t.duration.map(|d| d.as_secs_f64()).unwrap_or(0.0),
            t.format_label.clone(),
            t.lossless,
            t.art_url.clone().unwrap_or_default(),
            t.song_id.clone(),
        ),
        None => (String::new(), String::new(), String::new(), 0.0, String::new(), false, String::new(), String::new()),
    };
    (
        status.to_string(),
        title,
        artist,
        album,
        snap.position.as_secs_f64(),
        duration,
        snap.queue_index as u32,
        snap.queue_len as u32,
        snap.volume as f64,
        format_label,
        lossless,
        art_url,
        song_id,
        snap.repeat.as_str().to_string(),
        snap.shuffle,
    )
}

pub enum Command {
    /// Replaces the queue and starts playing at `start_index` immediately.
    PlayQueue {
        tracks: Vec<TrackMeta>,
        start_index: usize,
    },
    /// Jumps directly to `index` within the *current* queue (a TUI "Queue"
    /// view picking an arbitrary upcoming track) — distinct from `PlayQueue`,
    /// which replaces the queue itself.
    PlayAt(usize),
    /// Adds tracks to the end of the current queue without interrupting
    /// whatever's already playing — distinct from `PlayQueue`, which always
    /// replaces the queue and starts over. If nothing is currently playing,
    /// playback starts at the first appended track instead (otherwise
    /// "append" would silently do nothing audible).
    AppendQueue(Vec<TrackMeta>),
    /// Removes one track from the queue by its current index — for the
    /// TUI's Queue view "remove selected" (`d`). Removing the
    /// currently-playing track skips to whatever now occupies that slot (or
    /// stops if the queue becomes empty).
    RemoveFromQueue(usize),
    /// Reorders the queue: moves the track at `from` to position `to`,
    /// shifting everything between them — for the Queue view's "move
    /// up/down" (`J`/`K`).
    MoveInQueue { from: usize, to: usize },
    /// Empties the queue and stops playback — the Queue view's "clear"
    /// (`D`).
    ClearQueue,
    Next,
    Previous,
    Pause,
    Resume,
    Stop,
    Seek(Duration),
    SetVolume(f32),
    CycleRepeat,
    ToggleShuffle,
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum Event {
    StatusChanged(Status),
    /// A new track started loading — consumers re-read fresh metadata via
    /// [`PlaybackHandle::snapshot`] rather than carrying it here, since the
    /// MPRIS bridge needs it in `mpris_server`'s own `Metadata` shape anyway.
    TrackChanged,
    /// The queue was exhausted (no next track) rather than just this one
    /// track ending into another — distinct from `TrackChanged` so the MPRIS
    /// bridge knows there's nothing more to report metadata for.
    QueueEnded,
    /// A track failed to load/stream/decode — carries a message suitable for
    /// surfacing to the user (e.g. as a TUI toast), not full error internals.
    PlaybackError(String),
    Seeked(Duration),
}

/// The handle every other part of the daemon (MPRIS interface, custom
/// control interface, idle timer) uses to talk to the playback engine.
/// Cheap to clone — cloning shares the same channel and snapshot.
#[derive(Clone)]
pub struct PlaybackHandle {
    cmd_tx: Sender<Command>,
    snapshot: Arc<Mutex<Snapshot>>,
    /// Flipped whenever a command is sent or the engine's state changes —
    /// read (and reset) by the idle-shutdown timer so "activity" means real
    /// playback/control traffic, not just the timer's own polling.
    activity: Arc<AtomicBool>,
}

impl PlaybackHandle {
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().expect("snapshot mutex poisoned").clone()
    }

    /// Reports and clears whether there's been any activity since the last
    /// call — see [`Self::activity`].
    pub fn take_activity(&self) -> bool {
        self.activity.swap(false, Ordering::SeqCst)
    }

    /// True while a track is actually playing — the idle timer must never
    /// fire during active playback even with `take_activity() == false`
    /// (e.g. the terminal was closed, but music should keep going).
    pub fn is_playing(&self) -> bool {
        self.snapshot().status == Status::Playing
    }

    fn send(&self, cmd: Command) {
        self.activity.store(true, Ordering::SeqCst);
        // The engine thread only exits on Shutdown/Drop; a send error here
        // means it already died (panicked), which the daemon should treat as
        // a bug to log, not a reason to also panic the async side.
        if self.cmd_tx.send(cmd).is_err() {
            tracing::error!("playback engine is no longer running");
        }
    }

    pub fn play_queue(&self, tracks: Vec<TrackMeta>, start_index: usize) {
        self.send(Command::PlayQueue { tracks, start_index });
    }
    pub fn play_at(&self, index: usize) {
        self.send(Command::PlayAt(index));
    }
    pub fn append_queue(&self, tracks: Vec<TrackMeta>) {
        self.send(Command::AppendQueue(tracks));
    }
    pub fn remove_from_queue(&self, index: usize) {
        self.send(Command::RemoveFromQueue(index));
    }
    pub fn move_in_queue(&self, from: usize, to: usize) {
        self.send(Command::MoveInQueue { from, to });
    }
    pub fn clear_queue(&self) {
        self.send(Command::ClearQueue);
    }
    pub fn next(&self) {
        self.send(Command::Next);
    }
    pub fn previous(&self) {
        self.send(Command::Previous);
    }
    pub fn pause(&self) {
        self.send(Command::Pause);
    }
    pub fn resume(&self) {
        self.send(Command::Resume);
    }
    pub fn stop(&self) {
        self.send(Command::Stop);
    }
    pub fn seek(&self, position: Duration) {
        self.send(Command::Seek(position));
    }
    pub fn set_volume(&self, volume: f32) {
        self.send(Command::SetVolume(volume));
    }
    pub fn cycle_repeat(&self) {
        self.send(Command::CycleRepeat);
    }
    pub fn toggle_shuffle(&self) {
        self.send(Command::ToggleShuffle);
    }
    pub fn shutdown(&self) {
        self.send(Command::Shutdown);
    }
}

/// Spawns the engine thread and returns a handle to it. `events` is drained
/// by an async task elsewhere (see `main.rs`) to emit MPRIS signals and feed
/// the idle timer. `credentials` is `None` when the daemon starts before
/// `maraetai login` has ever been run — scrobbling is then silently skipped,
/// same as everything else that needs credentials.
pub fn spawn(events: UnboundedSender<Event>, credentials: Option<Credentials>, output_device: Option<String>) -> PlaybackHandle {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let snapshot = Arc::new(Mutex::new(Snapshot::default()));
    let activity = Arc::new(AtomicBool::new(false));

    let handle = PlaybackHandle {
        cmd_tx,
        snapshot: Arc::clone(&snapshot),
        activity,
    };

    std::thread::Builder::new()
        .name("maraetai-audio".into())
        .spawn(move || run_engine(cmd_rx, snapshot, events, credentials, output_device))
        .expect("failed to spawn audio thread");

    handle
}

/// Converts a ReplayGain value in decibels into a linear volume multiplier
/// (the standard `10^(dB/20)` conversion), clamped to never exceed 1.0 —
/// deliberately attenuate-only, since amplifying a quiet track up to its
/// ReplayGain target could clip without a peak limiter, which this engine
/// doesn't have. `0.0` dB (no ReplayGain metadata, or a genuine 0 dB tag —
/// see `TrackMeta::replay_gain_db`) is unity gain either way.
fn replay_gain_scale(gain_db: f64) -> f32 {
    (10f64.powf(gain_db / 20.0) as f32).min(1.0)
}

/// Picks the output device name (from `available`, as reported by the
/// audio backend) that best matches `requested` — an exact match if one
/// exists, else the first case-insensitive substring match (so a saved
/// config value that's gone slightly stale, e.g. a USB interface that
/// renumbered, still has a chance of matching), else `None` — the signal
/// to fall back to the system default rather than fail outright.
fn match_output_device<'a>(available: &'a [String], requested: &str) -> Option<&'a str> {
    if let Some(exact) = available.iter().find(|n| n.as_str() == requested) {
        return Some(exact.as_str());
    }
    let requested_lower = requested.to_lowercase();
    available.iter().find(|n| n.to_lowercase().contains(&requested_lower)).map(String::as_str)
}

/// Opens the audio output stream — the system default, unless
/// `output_device` names one to use instead (see `match_output_device`).
/// Any failure to find or open the requested device falls back to the
/// default rather than refusing to start playback at all.
fn open_output_stream(output_device: Option<&str>) -> Result<OutputStream, rodio::StreamError> {
    use rodio::cpal::traits::{DeviceTrait, HostTrait};

    let Some(requested) = output_device else {
        return OutputStreamBuilder::open_default_stream();
    };
    let host = rodio::cpal::default_host();
    let names: Vec<String> = host.output_devices().map(|d| d.filter_map(|d| d.name().ok()).collect()).unwrap_or_default();
    let Some(matched) = match_output_device(&names, requested).map(str::to_string) else {
        tracing::warn!(requested, "no matching output device found — falling back to the default");
        return OutputStreamBuilder::open_default_stream();
    };
    let device = host.output_devices().ok().and_then(|mut devices| devices.find(|d| d.name().map(|n| n == matched).unwrap_or(false)));
    let Some(device) = device else {
        tracing::warn!(requested, "matched output device disappeared before it could be opened — falling back to the default");
        return OutputStreamBuilder::open_default_stream();
    };
    match OutputStreamBuilder::from_device(device).and_then(|b| b.open_stream()) {
        Ok(stream) => Ok(stream),
        Err(e) => {
            tracing::warn!(requested, error = %e, "failed to open the requested output device — falling back to the default");
            OutputStreamBuilder::open_default_stream()
        }
    }
}

/// Prints every available output device's name, marking the system
/// default — for `maraetaid --list-devices`, so a user has the exact
/// string to put in `output_device` in their config. Doesn't touch the
/// single-instance lock or open a stream, so it's safe to run alongside
/// an already-running daemon.
pub fn list_output_devices() {
    use rodio::cpal::traits::{DeviceTrait, HostTrait};

    let host = rodio::cpal::default_host();
    let default_name = host.default_output_device().and_then(|d| d.name().ok());
    let Ok(devices) = host.output_devices() else {
        eprintln!("could not list output devices");
        return;
    };
    for device in devices {
        let Ok(name) = device.name() else { continue };
        if Some(&name) == default_name.as_ref() {
            println!("{name} (default)");
        } else {
            println!("{name}");
        }
    }
}

struct EngineState {
    // Must stay alive for as long as `sink` plays anything — cpal tears down
    // the output device when it's dropped. Also the source of the `Mixer`
    // each new `Sink` connects to (rodio 0.21 folded the old separate
    // `OutputStreamHandle` into `OutputStream::mixer()`).
    stream: OutputStream,
    sink: Option<Sink>,
    http: reqwest::blocking::Client,
    queue: Vec<TrackMeta>,
    index: usize,
    /// A permutation of `0..queue.len()` that `Next`/`Previous` actually
    /// step through — the identity order when `shuffle` is off. Invariant
    /// maintained by `start_playback_at`: `play_order[order_pos] ==
    /// index` always holds after it returns, regardless of *how* playback
    /// got there (a shuffled `Next`, or a manual `PlayAt` jump).
    play_order: Vec<usize>,
    order_pos: usize,
    repeat: RepeatMode,
    shuffle: bool,
    analyzer: SpectrumAnalyzer,
    sample_ring: Arc<visualizer::SampleRing>,
    /// Channels/sample-rate of whatever's currently loaded — captured from
    /// the decoder at load time (needed for the FFT's frequency-bucket math,
    /// and no longer queryable once the decoder is consumed into the sink).
    current_format: Option<(u16, u32)>,
    /// The track actually loaded into `sink`, if any — kept separate from
    /// `queue[index]` because `PlayQueue` overwrites `queue` (and can reuse
    /// the same `index`) *before* `start_playback_at` runs, which would
    /// otherwise make "what was just playing" silently resolve to the *new*
    /// queue's track instead of the one that's actually stopping. This is
    /// scrobbling's only reason to exist — nothing else needs it.
    current: Option<TrackMeta>,
    /// `None` until `maraetai login` has been run.
    credentials: Option<Credentials>,
    /// The user's own volume setting (0.0-1.0), independent of the
    /// `Snapshot`'s copy of the same value — kept here too because a fresh
    /// `Sink` (built for every track, in `start_playback_at`) doesn't
    /// inherit whatever the *previous* sink's volume was, and needs this
    /// re-applied (combined with `replay_gain_scale`) every time.
    volume: f32,
}

fn run_engine(
    cmd_rx: Receiver<Command>,
    snapshot: Arc<Mutex<Snapshot>>,
    events: UnboundedSender<Event>,
    credentials: Option<Credentials>,
    output_device: Option<String>,
) {
    let stream = match open_output_stream(output_device.as_deref()) {
        Ok(stream) => stream,
        Err(e) => {
            tracing::error!("no audio output device available: {e}");
            let _ = events.send(Event::PlaybackError(format!("no audio output device: {e}")));
            return;
        }
    };
    let (analyzer, sample_ring) = SpectrumAnalyzer::new();
    let mut state = EngineState {
        stream,
        sink: None,
        http: reqwest::blocking::Client::new(),
        queue: Vec::new(),
        index: 0,
        play_order: Vec::new(),
        order_pos: 0,
        repeat: RepeatMode::Off,
        shuffle: false,
        analyzer,
        sample_ring,
        current_format: None,
        current: None,
        credentials,
        volume: 1.0,
    };

    loop {
        match cmd_rx.recv_timeout(POLL_INTERVAL) {
            Ok(Command::PlayQueue { tracks, start_index }) => {
                state.queue = tracks;
                snapshot.lock().expect("poisoned").queue = state.queue.clone();
                regenerate_play_order(&mut state, None);
                let start_index = start_index.min(state.queue.len().saturating_sub(1));
                start_playback_at(&mut state, &snapshot, &events, start_index);
            }
            Ok(Command::AppendQueue(tracks)) => {
                if !tracks.is_empty() {
                    let start_of_new = state.queue.len();
                    state.queue.extend(tracks);
                    // New tracks join the end of the play order regardless
                    // of shuffle — "append" means "play later", not
                    // "shuffle in somewhere unpredictable".
                    state.play_order.extend(start_of_new..state.queue.len());
                    snapshot.lock().expect("poisoned").queue = state.queue.clone();
                    if state.sink.is_none() {
                        // Nothing was playing — appending to an empty/
                        // stopped queue must still be audible, so start at
                        // what was just added.
                        start_playback_at(&mut state, &snapshot, &events, start_of_new);
                    } else {
                        snapshot.lock().expect("poisoned").queue_len = state.queue.len();
                    }
                }
            }
            Ok(Command::RemoveFromQueue(remove_index)) => {
                if remove_index < state.queue.len() {
                    let removing_current = remove_index == state.index;
                    state.queue.remove(remove_index);
                    state.play_order = play_order_after_remove(&state.play_order, remove_index);
                    if removing_current {
                        // The playing track is gone — stop it (scrobbling if
                        // it qualified) and move on to whatever now
                        // occupies this slot, same transition any other
                        // track change goes through.
                        if let Some(sink) = state.sink.take() {
                            maybe_scrobble_outgoing(&state, sink.get_pos());
                        }
                        state.current = None;
                        if state.queue.is_empty() {
                            state.index = 0;
                            state.order_pos = 0;
                            {
                                let mut snap = snapshot.lock().expect("poisoned");
                                snap.queue = Vec::new();
                                snap.queue_len = 0;
                                snap.queue_index = 0;
                                snap.track = None;
                                snap.spectrum = [0; visualizer::BARS];
                            }
                            set_status(&snapshot, &events, Status::Stopped);
                            let _ = events.send(Event::QueueEnded);
                        } else {
                            let next = remove_index.min(state.queue.len() - 1);
                            start_playback_at(&mut state, &snapshot, &events, next);
                        }
                    } else {
                        if remove_index < state.index {
                            state.index -= 1;
                        }
                        state.order_pos = state.play_order.iter().position(|&i| i == state.index).unwrap_or(0);
                        let mut snap = snapshot.lock().expect("poisoned");
                        snap.queue = state.queue.clone();
                        snap.queue_len = state.queue.len();
                        snap.queue_index = state.index;
                    }
                }
            }
            Ok(Command::MoveInQueue { from, to }) => {
                let len = state.queue.len();
                if from < len && to < len && from != to {
                    let track = state.queue.remove(from);
                    state.queue.insert(to, track);
                    state.play_order = play_order_after_move(&state.play_order, from, to);
                    state.index = remap_index_after_move(state.index, from, to);
                    state.order_pos = state.play_order.iter().position(|&i| i == state.index).unwrap_or(0);
                    let mut snap = snapshot.lock().expect("poisoned");
                    snap.queue = state.queue.clone();
                    snap.queue_index = state.index;
                }
            }
            Ok(Command::ClearQueue) => {
                if let Some(sink) = state.sink.take() {
                    maybe_scrobble_outgoing(&state, sink.get_pos());
                    sink.stop();
                }
                state.current = None;
                state.queue.clear();
                state.play_order.clear();
                state.index = 0;
                state.order_pos = 0;
                {
                    let mut snap = snapshot.lock().expect("poisoned");
                    snap.queue = Vec::new();
                    snap.queue_len = 0;
                    snap.queue_index = 0;
                    snap.track = None;
                    snap.spectrum = [0; visualizer::BARS];
                }
                set_status(&snapshot, &events, Status::Stopped);
                let _ = events.send(Event::QueueEnded);
            }
            Ok(Command::PlayAt(index)) => {
                if index < state.queue.len() {
                    start_playback_at(&mut state, &snapshot, &events, index);
                }
            }
            Ok(Command::Next) => {
                if state.order_pos + 1 < state.play_order.len() {
                    let next = state.play_order[state.order_pos + 1];
                    start_playback_at(&mut state, &snapshot, &events, next);
                } else if state.repeat == RepeatMode::Queue && !state.play_order.is_empty() {
                    // Wrapping the queue is a natural point to reshuffle, so
                    // a shuffled repeat doesn't replay the exact same order
                    // every lap.
                    if state.shuffle {
                        regenerate_play_order(&mut state, None);
                    }
                    let next = state.play_order[0];
                    start_playback_at(&mut state, &snapshot, &events, next);
                }
                // Otherwise: at the end of the queue with repeat off —
                // matches the common player convention of doing nothing on
                // "next" rather than stopping.
            }
            Ok(Command::Previous) => {
                let position = current_position(&state);
                let restart_current = position > RESTART_THRESHOLD || state.order_pos == 0;
                if restart_current {
                    // Restarting counts as finishing this listen of the
                    // track (a later restart-and-relisten is a separate,
                    // independently-eligible play) — `state.current` stays
                    // set to the same track, since we're not switching away.
                    maybe_scrobble_outgoing(&state, position);
                    if let Some(sink) = &state.sink {
                        let _ = sink.try_seek(Duration::ZERO);
                        snapshot.lock().expect("poisoned").position = Duration::ZERO;
                    }
                } else {
                    let prev = state.play_order[state.order_pos - 1];
                    start_playback_at(&mut state, &snapshot, &events, prev);
                }
            }
            Ok(Command::Pause) => {
                if let Some(sink) = &state.sink {
                    sink.pause();
                    set_status(&snapshot, &events, Status::Paused);
                }
            }
            Ok(Command::Resume) => {
                if let Some(sink) = &state.sink {
                    sink.play();
                    set_status(&snapshot, &events, Status::Playing);
                }
            }
            Ok(Command::Stop) => {
                if let Some(sink) = state.sink.take() {
                    maybe_scrobble_outgoing(&state, sink.get_pos());
                    sink.stop();
                }
                state.current = None;
                snapshot.lock().expect("poisoned").spectrum = [0; visualizer::BARS];
                set_status(&snapshot, &events, Status::Stopped);
            }
            Ok(Command::Seek(pos)) => {
                if let Some(sink) = &state.sink {
                    match sink.try_seek(pos) {
                        Ok(()) => {
                            snapshot.lock().expect("poisoned").position = pos;
                            let _ = events.send(Event::Seeked(pos));
                        }
                        Err(e) => {
                            tracing::warn!("seek failed: {e}");
                        }
                    }
                }
            }
            Ok(Command::SetVolume(v)) => {
                state.volume = v;
                if let Some(sink) = &state.sink {
                    let gain = state.current.as_ref().map(|t| t.replay_gain_db).unwrap_or(0.0);
                    sink.set_volume(v * replay_gain_scale(gain));
                }
                snapshot.lock().expect("poisoned").volume = v;
            }
            Ok(Command::CycleRepeat) => {
                state.repeat = state.repeat.cycle();
                snapshot.lock().expect("poisoned").repeat = state.repeat;
            }
            Ok(Command::ToggleShuffle) => {
                state.shuffle = !state.shuffle;
                // Anchoring to the current track means turning shuffle on
                // mid-play doesn't jump away from what's playing — only
                // what's queued *after* it gets randomized.
                let current = state.index;
                regenerate_play_order(&mut state, Some(current));
                snapshot.lock().expect("poisoned").shuffle = state.shuffle;
            }
            Ok(Command::Shutdown) => {
                if let Some(sink) = state.sink.take() {
                    sink.stop();
                }
                return;
            }
            Err(RecvTimeoutError::Timeout) => {
                poll_progress(&mut state, &snapshot, &events);
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn current_position(state: &EngineState) -> Duration {
    state.sink.as_ref().map(Sink::get_pos).unwrap_or_default()
}

/// Rebuilds `play_order` for the current `queue` and resyncs `order_pos` to
/// wherever `state.index` ends up in it — the invariant `Next`/`Previous`
/// (and `start_playback_at`, after any jump) rely on. With shuffle off,
/// it's just the identity order `0..len`. With shuffle on, `anchor` (when
/// it's a valid index) is kept at the front of the new order — so toggling
/// shuffle on mid-track doesn't jump away from what's currently playing,
/// only what's queued *after* it gets randomized; `None` (a freshly
/// replaced queue, or reshuffling on a repeat-queue wrap) shuffles
/// everything with nothing pinned.
fn regenerate_play_order(state: &mut EngineState, anchor: Option<usize>) {
    state.play_order = shuffled_play_order(state.queue.len(), state.shuffle, anchor);
    state.order_pos = state.play_order.iter().position(|&i| i == state.index).unwrap_or(0);
}

/// The pure permutation logic behind [`regenerate_play_order`] — factored
/// out so it's unit-testable without needing a real `EngineState` (which
/// owns a live audio output stream, not something to open in a test).
fn shuffled_play_order(len: usize, shuffle: bool, anchor: Option<usize>) -> Vec<usize> {
    if !shuffle || len == 0 {
        return (0..len).collect();
    }
    if let Some(anchor) = anchor.filter(|&a| a < len) {
        let mut rest: Vec<usize> = (0..len).filter(|&i| i != anchor).collect();
        rest.shuffle(&mut rand::thread_rng());
        std::iter::once(anchor).chain(rest).collect()
    } else {
        let mut order: Vec<usize> = (0..len).collect();
        order.shuffle(&mut rand::thread_rng());
        order
    }
}

/// Recomputes `play_order` after removing `queue[removed]` — every entry
/// that pointed at `removed` is dropped, and every entry pointing past it
/// shifts down by one, so the existing traversal order (shuffled or not)
/// survives a single removal instead of being thrown away and reshuffled
/// from scratch.
fn play_order_after_remove(order: &[usize], removed: usize) -> Vec<usize> {
    order.iter().filter(|&&i| i != removed).map(|&i| if i > removed { i - 1 } else { i }).collect()
}

/// Where a single queue index lands after `queue[from]` moves to position
/// `to` (everything strictly between shifts one step to close/open the gap)
/// — the building block behind both `play_order_after_move` (remapping
/// every entry in the play order) and remapping `state.index` itself.
fn remap_index_after_move(i: usize, from: usize, to: usize) -> usize {
    if i == from {
        to
    } else if from < to && i > from && i <= to {
        i - 1
    } else if to < from && i >= to && i < from {
        i + 1
    } else {
        i
    }
}

/// Recomputes `play_order` after moving `queue[from]` to position `to` —
/// same "preserve the existing order, don't reshuffle" principle as
/// `play_order_after_remove`.
fn play_order_after_move(order: &[usize], from: usize, to: usize) -> Vec<usize> {
    order.iter().map(|&i| remap_index_after_move(i, from, to)).collect()
}

/// Starts playing `state.queue[index]`, replacing whatever was playing.
fn start_playback_at(
    state: &mut EngineState,
    snapshot: &Arc<Mutex<Snapshot>>,
    events: &UnboundedSender<Event>,
    index: usize,
) {
    // Scrobble whatever was playing *before* touching `state.queue` — a
    // `PlayQueue` command already overwrote it with the new list by the time
    // this runs, so `state.current` (not `queue[index]`) is the only
    // reliable record of what's actually stopping.
    maybe_scrobble_outgoing(state, current_position(state));

    if let Some(sink) = state.sink.take() {
        sink.stop();
    }
    let Some(meta) = state.queue.get(index).cloned() else {
        state.current = None;
        return;
    };
    state.index = index;
    // Keeps `play_order[order_pos] == index` true regardless of *how* we
    // got here — a shuffled `Next`/`Previous` step, or a manual `PlayAt`
    // jump from the Queue view that lands somewhere play_order wasn't
    // already pointing at.
    state.order_pos = state.play_order.iter().position(|&i| i == index).unwrap_or(index);

    let mut reader = RangeReader::new(state.http.clone(), meta.stream_url.clone());
    // `RangeReader` only learns the resource's total length lazily, from the
    // first response it gets — but `DecoderBuilder::with_byte_len` needs it
    // supplied up front. Prime it with a throwaway 1-byte read, then seek
    // back to the start before handing the reader to the decoder. Without a
    // known byte length, Symphonia's FLAC seeking fails outright with
    // `SeekError::Unseekable` (confirmed against a real FLAC file over a
    // real HTTP Range server) — this priming step is what makes seeking
    // actually work, not just look supported.
    let mut probe = [0u8; 1];
    let byte_len = reader.read_exact(&mut probe).ok().and_then(|()| {
        let _ = reader.seek(SeekFrom::Start(0));
        reader.known_len()
    });
    let mut decoder_builder = Decoder::builder().with_data(reader).with_seekable(true);
    if let Some(len) = byte_len {
        decoder_builder = decoder_builder.with_byte_len(len);
    }
    let decoder = match decoder_builder.build() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("failed to decode stream: {e}");
            let _ = events.send(Event::PlaybackError(format!("could not play track: {e}")));
            set_status(snapshot, events, Status::Stopped);
            state.current = None;
            return;
        }
    };

    // Captured before the decoder is consumed into the tap/sink — needed by
    // the visualizer's FFT frequency-bucket math on every poll tick.
    state.current_format = Some((decoder.channels(), decoder.sample_rate()));
    state.analyzer.reset();
    let tapped = VisualizerTap::new(decoder, Arc::clone(&state.sample_ring));

    let sink = Sink::connect_new(state.stream.mixer());
    sink.append(tapped);
    // A fresh sink doesn't inherit the previous one's volume — re-apply
    // the user's own setting, combined with this track's ReplayGain scale
    // (see `replay_gain_scale`).
    sink.set_volume(state.volume * replay_gain_scale(meta.replay_gain_db));
    {
        let mut snap = snapshot.lock().expect("poisoned");
        snap.status = Status::Playing;
        snap.position = Duration::ZERO;
        snap.track = Some(meta.clone());
        snap.queue_index = index;
        snap.queue_len = state.queue.len();
    }
    state.sink = Some(sink);
    spawn_scrobble(state.http.clone(), state.credentials.clone(), meta.song_id.clone(), false);
    state.current = Some(meta);
    let _ = events.send(Event::TrackChanged);
    let _ = events.send(Event::StatusChanged(Status::Playing));
}

fn poll_progress(state: &mut EngineState, snapshot: &Arc<Mutex<Snapshot>>, events: &UnboundedSender<Event>) {
    let Some(sink) = &state.sink else { return };
    let ended = sink.empty();
    let currently_playing = snapshot.lock().expect("poisoned").status == Status::Playing;

    if ended && currently_playing {
        if state.repeat == RepeatMode::Track {
            // Reaching the natural end (not a manual `Next`) loops the same
            // track — reloading fresh rather than seeking the now-exhausted
            // sink back to zero, which isn't reliably possible once a
            // source has already reported end-of-stream. `start_playback_at`
            // scrobbles the just-finished play (if eligible) at its top
            // before reloading, exactly like any other track transition.
            let current = state.index;
            start_playback_at(state, snapshot, events, current);
        } else if state.order_pos + 1 < state.play_order.len() {
            let next = state.play_order[state.order_pos + 1];
            start_playback_at(state, snapshot, events, next);
        } else if state.repeat == RepeatMode::Queue && !state.play_order.is_empty() {
            if state.shuffle {
                regenerate_play_order(state, None);
            }
            let next = state.play_order[0];
            start_playback_at(state, snapshot, events, next);
        } else {
            // The queue is truly exhausted — `start_playback_at` won't run
            // to do this for us, so scrobble the just-finished track here.
            // A track that reached natural end-of-stream is, almost by
            // definition, past the scrobble threshold.
            maybe_scrobble_outgoing(state, current_position(state));
            state.current = None;
            let mut snap = snapshot.lock().expect("poisoned");
            snap.status = Status::Stopped;
            snap.spectrum = [0; visualizer::BARS];
            drop(snap);
            let _ = events.send(Event::QueueEnded);
            let _ = events.send(Event::StatusChanged(Status::Stopped));
        }
        return;
    }

    // Only recompute the spectrum while actually producing new audio —
    // while paused, the last frame is simply left in place (a frozen
    // visualizer, matching the frozen position), not recomputed from a ring
    // buffer that isn't receiving new samples anyway.
    if currently_playing {
        if let Some((channels, sample_rate)) = state.current_format {
            let spectrum = state.analyzer.compute(channels, sample_rate);
            snapshot.lock().expect("poisoned").spectrum = spectrum;
        }
    }

    if let Some(sink) = &state.sink {
        snapshot.lock().expect("poisoned").position = sink.get_pos();
    }
}

fn set_status(snapshot: &Arc<Mutex<Snapshot>>, events: &UnboundedSender<Event>, status: Status) {
    snapshot.lock().expect("poisoned").status = status.clone();
    let _ = events.send(Event::StatusChanged(status));
}

/// Scrobbles `state.current` if it's been played past [`scrobble_threshold`]
/// — call this right before a track stops being current (a track change, an
/// explicit stop, or the queue running out), passing that track's position
/// at the moment of the transition. A silent no-op when there's no current
/// track, no credentials, no song id (a bare `maraetai play <id>` with no
/// library duration to judge against), or the position doesn't clear the
/// threshold — scrobbling is inherently best-effort background bookkeeping,
/// never something playback should stall or error out over.
fn maybe_scrobble_outgoing(state: &EngineState, position: Duration) {
    let Some(track) = &state.current else { return };
    if track.song_id.is_empty() {
        return;
    }
    let Some(duration) = track.duration else { return };
    if duration > Duration::ZERO && position >= scrobble_threshold(duration) {
        spawn_scrobble(state.http.clone(), state.credentials.clone(), track.song_id.clone(), true);
    }
}

/// Fires a Subsonic `scrobble` request on its own thread — matching
/// `maraetai-service`'s own scrobble tee, which records the play
/// asynchronously and does not want the caller to wait on it either.
/// `submission=false` is the "now playing" notification (sent once per track
/// start, purely informational — other Subsonic clients/the maraetai web
/// app use it to show what's currently playing); `submission=true` is an
/// actual recorded play. A missing `credentials` or empty `song_id` is a
/// silent no-op — nothing to authenticate with, or nothing to identify the
/// track by.
fn spawn_scrobble(http: reqwest::blocking::Client, credentials: Option<Credentials>, song_id: String, submission: bool) {
    let Some(creds) = credentials else { return };
    if song_id.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let auth = AuthParams::new(&creds.username, &creds.password);
        let mut pairs: Vec<(String, String)> = vec![("id".into(), song_id), ("submission".into(), submission.to_string())];
        auth.append_to(&mut pairs);
        let url = format!("{}/rest/scrobble.view", creds.server_url.trim_end_matches('/'));
        match http.get(&url).query(&pairs).send() {
            Ok(resp) if !resp.status().is_success() => {
                tracing::warn!(status = %resp.status(), submission, "scrobble request returned an error status");
            }
            Err(e) => tracing::warn!("scrobble request failed: {e}"),
            Ok(_) => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::sync::mpsc as std_mpsc;

    use super::*;

    #[test]
    fn replay_gain_scale_of_zero_db_is_unity() {
        assert_eq!(replay_gain_scale(0.0), 1.0);
    }

    #[test]
    fn replay_gain_scale_attenuates_a_negative_gain() {
        // -6 dB is very close to half amplitude (10^(-6/20) ≈ 0.501).
        assert!((replay_gain_scale(-6.0) - 0.501).abs() < 0.01);
    }

    #[test]
    fn replay_gain_scale_never_amplifies_past_unity() {
        // A positive gain (a quiet track that ReplayGain would normally
        // boost) is clamped to 1.0 rather than actually amplified — see
        // the doc comment: no peak limiter here, so amplifying risks
        // clipping.
        assert_eq!(replay_gain_scale(6.0), 1.0);
    }

    #[test]
    fn match_output_device_prefers_an_exact_match() {
        let available = vec!["USB DAC".to_string(), "USB DAC (2)".to_string()];
        assert_eq!(match_output_device(&available, "USB DAC"), Some("USB DAC"));
    }

    #[test]
    fn match_output_device_falls_back_to_a_substring_match() {
        let available = vec!["HDA Intel PCH: ALC1220 Analog".to_string()];
        assert_eq!(match_output_device(&available, "ALC1220"), Some("HDA Intel PCH: ALC1220 Analog"));
    }

    #[test]
    fn match_output_device_is_case_insensitive() {
        let available = vec!["USB DAC".to_string()];
        assert_eq!(match_output_device(&available, "usb dac"), Some("USB DAC"));
    }

    #[test]
    fn match_output_device_returns_none_when_nothing_matches() {
        let available = vec!["USB DAC".to_string()];
        assert_eq!(match_output_device(&available, "Bluetooth Headphones"), None);
    }

    #[test]
    fn scrobble_threshold_is_half_the_track_for_short_songs() {
        assert_eq!(scrobble_threshold(Duration::from_secs(60)), Duration::from_secs(30));
    }

    #[test]
    fn scrobble_threshold_caps_at_four_minutes_for_long_songs() {
        assert_eq!(scrobble_threshold(Duration::from_secs(60 * 20)), Duration::from_secs(4 * 60));
    }

    #[test]
    fn repeat_mode_cycles_off_track_queue_and_back_to_off() {
        assert_eq!(RepeatMode::Off.cycle(), RepeatMode::Track);
        assert_eq!(RepeatMode::Track.cycle(), RepeatMode::Queue);
        assert_eq!(RepeatMode::Queue.cycle(), RepeatMode::Off);
    }

    #[test]
    fn play_order_is_identity_when_shuffle_is_off_regardless_of_anchor() {
        assert_eq!(shuffled_play_order(5, false, Some(3)), vec![0, 1, 2, 3, 4]);
        assert_eq!(shuffled_play_order(5, false, None), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn play_order_pins_the_anchor_at_the_front_when_shuffled() {
        // Run several times since shuffling is random — the anchor's
        // position is the one thing that must never vary.
        for _ in 0..20 {
            let order = shuffled_play_order(6, true, Some(4));
            assert_eq!(order[0], 4, "anchor must stay pinned at the front: {order:?}");
            let mut sorted = order.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, vec![0, 1, 2, 3, 4, 5], "must still be a full permutation: {order:?}");
        }
    }

    #[test]
    fn play_order_is_a_full_permutation_with_no_anchor() {
        for _ in 0..20 {
            let order = shuffled_play_order(6, true, None);
            let mut sorted = order.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, vec![0, 1, 2, 3, 4, 5]);
        }
    }

    #[test]
    fn play_order_of_an_empty_queue_is_empty() {
        assert!(shuffled_play_order(0, true, None).is_empty());
        assert!(shuffled_play_order(0, false, None).is_empty());
    }

    #[test]
    fn play_order_ignores_an_out_of_range_anchor() {
        // Defensive: an anchor that's somehow no longer a valid index (e.g.
        // stale after the queue shrank) must not panic or produce a short
        // permutation — it's simply treated as "no anchor".
        let order = shuffled_play_order(4, true, Some(99));
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 1, 2, 3]);
    }

    #[test]
    fn play_order_after_remove_drops_the_removed_entry_and_shifts_greater_ones_down() {
        // Original order [2, 0, 3, 1]; removing queue index 2 drops that
        // entry (wherever it appears) and every remaining index greater than
        // 2 (i.e. 3) shifts down to 2.
        assert_eq!(play_order_after_remove(&[2, 0, 3, 1], 2), vec![0, 2, 1]);
    }

    #[test]
    fn play_order_after_remove_leaves_smaller_indices_untouched() {
        assert_eq!(play_order_after_remove(&[0, 1, 2, 3], 3), vec![0, 1, 2]);
    }

    #[test]
    fn remap_index_after_move_forward_shifts_the_gap_closed() {
        // Moving index 1 to position 3: indices 2 and 3 (between the old and
        // new slot) shift down by one to fill the gap; 1 itself becomes 3.
        assert_eq!(remap_index_after_move(1, 1, 3), 3);
        assert_eq!(remap_index_after_move(2, 1, 3), 1);
        assert_eq!(remap_index_after_move(3, 1, 3), 2);
        assert_eq!(remap_index_after_move(0, 1, 3), 0, "untouched index before the move must be unchanged");
    }

    #[test]
    fn remap_index_after_move_backward_shifts_the_gap_closed() {
        // Moving index 3 to position 1: indices 1 and 2 shift up by one to
        // make room; 3 itself becomes 1.
        assert_eq!(remap_index_after_move(3, 3, 1), 1);
        assert_eq!(remap_index_after_move(1, 3, 1), 2);
        assert_eq!(remap_index_after_move(2, 3, 1), 3);
        assert_eq!(remap_index_after_move(0, 3, 1), 0);
    }

    #[test]
    fn play_order_after_move_is_still_a_full_permutation() {
        let order = play_order_after_move(&[0, 1, 2, 3, 4], 1, 3);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 1, 2, 3, 4], "moving must never drop or duplicate an index: {order:?}");
    }

    /// Verifies `spawn_scrobble` against `maraetai-service`'s actual
    /// `scrobbleTee` contract (internal/proxy/scrobble.go in that repo): a
    /// GET to `/rest/scrobble.view` carrying `id`, `submission`, and the
    /// standard Subsonic auth params (`u`, `t`, `s`, `c`, `v`) as query
    /// parameters — that handler reads `id`/`submission`/`u` straight off
    /// `r.URL.Query()`, so the request line (not a body) is what matters.
    #[test]
    fn scrobble_request_matches_maraetai_services_scrobble_tee_contract() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std_mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = tx.send(request);
        });

        let creds = Credentials {
            server_url: format!("http://{addr}"),
            username: "alice".into(),
            password: "hunter2".into(),
        };
        spawn_scrobble(reqwest::blocking::Client::new(), Some(creds), "song123".into(), true);

        let request = rx.recv_timeout(Duration::from_secs(5)).expect("no scrobble request received in time");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            request_line.starts_with("GET /rest/scrobble.view?"),
            "expected a GET to /rest/scrobble.view, got: {request_line}"
        );
        for expected_param in ["id=song123", "submission=true", "u=alice", "c=maraetai-tui"] {
            assert!(request_line.contains(expected_param), "missing {expected_param} in: {request_line}");
        }
        // t (token) and s (salt) are present but their values are randomized
        // per request — just confirm the keys exist.
        assert!(request_line.contains("t="), "missing auth token param in: {request_line}");
        assert!(request_line.contains("s="), "missing auth salt param in: {request_line}");
    }

    #[test]
    fn scrobble_is_a_no_op_with_no_credentials() {
        // Must not panic or spawn a request with nothing to authenticate
        // with — there's no server to assert against here; the test's
        // value is that this simply returns immediately rather than doing
        // anything (e.g. panicking on an unwrap of `None`).
        spawn_scrobble(reqwest::blocking::Client::new(), None, "song123".into(), true);
    }
}
