//! Wire types shared between the daemon's two control-channel transports —
//! D-Bus (`com.maraetai.Daemon1`, Linux) and a Unix-socket protocol
//! (macOS; see the daemon's `socket_control` and the TUI's `socket_client`
//! in their respective crates) — so the two transports can't drift apart
//! on what a queue entry or a status snapshot looks like. D-Bus encodes
//! the tuples below directly; the socket transport serializes the same
//! tuples as JSON via [`Request`]/[`Response`].
//!
//! Defined here (rather than in the daemon crate) purely so the TUI can
//! use the exact same types without depending on the daemon binary.

use serde::{Deserialize, Serialize};

/// One queue entry: (stream_url, title, artist, album, art_url,
/// duration_secs, format_label, lossless, song_id). A plain tuple, not a
/// named struct, so D-Bus/zvariant encode it with no extra wiring — see
/// `daemon::control` for the field-by-field meaning.
pub type QueueEntry = (String, String, String, String, String, f64, String, bool, String);

/// One queue row for display: (title, artist, album, duration_secs,
/// format_label, lossless, song_id).
pub type QueueRow = (String, String, String, f64, String, bool, String);

/// (status, title, artist, album, position, duration, queue_index,
/// queue_len, volume, format_label, lossless, art_url, song_id,
/// repeat_mode, shuffle) — see `daemon::control`'s `status()` for the
/// field-by-field meaning.
#[allow(clippy::type_complexity)]
pub type StatusTuple =
    (String, String, String, String, f64, f64, u32, u32, f64, String, bool, String, String, String, bool);

/// One control-channel call, for the socket transport. Named to match the
/// D-Bus interface's own method names 1:1, so the two transports are
/// obviously equivalent — see `daemon::control` for what each one does.
///
/// No `Debug` derive: `StatusTuple` is a 15-element tuple, past the
/// standard library's arity limit for auto-deriving `Debug` on tuples
/// (12) — `Response::Status` would refuse to compile with it. Not needed
/// in practice: a bad request/response is reported through `Response::Error`
/// on the wire, not `{:?}`-printed locally.
#[derive(Clone, Serialize, Deserialize)]
pub enum Request {
    PlayQueue { tracks: Vec<QueueEntry>, start_index: u32 },
    PlayAt { index: u32 },
    AppendQueue { tracks: Vec<QueueEntry> },
    Queue,
    RemoveFromQueue { index: u32 },
    MoveInQueue { from: u32, to: u32 },
    ClearQueue,
    Spectrum,
    Next,
    Previous,
    Pause,
    Resume,
    Stop,
    SeekTo { position_secs: f64 },
    SetVolume { volume: f64 },
    CycleRepeat,
    ToggleShuffle,
    Status,
    Quit,
}

/// The reply to one [`Request`] — exactly one variant per distinct return
/// shape (several requests share `Unit`), plus `Error` for anything that
/// went wrong handling the request. No `Debug` derive — see `Request`'s
/// doc comment.
///
/// `Status`'s 15-tuple makes this enum noticeably bigger than its other
/// variants — irrelevant here: one of these is constructed and immediately
/// serialized per request/response, at most a few times a second, not in
/// any hot loop worth boxing around.
#[derive(Clone, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum Response {
    Unit,
    Queue(Vec<QueueRow>),
    Spectrum(Vec<u8>),
    Status(StatusTuple),
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `StatusTuple` is 15 elements — past `Debug`'s 12-element std arity
    /// limit (see the doc comments above), but serde's own tuple impls go
    /// further than std's; this confirms that's actually true rather than
    /// assumed, for the one type here big enough to matter.
    #[test]
    fn a_15_element_status_tuple_round_trips_through_json() {
        let status: StatusTuple = (
            "playing".into(),
            "Angel".into(),
            "Massive Attack".into(),
            "Mezzanine".into(),
            12.5,
            379.0,
            2,
            10,
            0.8,
            "FLAC".into(),
            true,
            "http://example.com/art".into(),
            "song123".into(),
            "queue".into(),
            true,
        );
        let json = serde_json::to_string(&Response::Status(status.clone())).unwrap();
        let Response::Status(round_tripped) = serde_json::from_str(&json).unwrap() else {
            panic!("expected a Status response");
        };
        // A 15-element tuple has no `PartialEq`/`Debug` either (same std
        // arity limit) — compare field by field instead of the whole thing.
        assert_eq!(round_tripped.0, status.0);
        assert_eq!(round_tripped.1, status.1);
        assert_eq!(round_tripped.4, status.4);
        assert_eq!(round_tripped.6, status.6);
        assert_eq!(round_tripped.10, status.10);
        assert_eq!(round_tripped.13, status.13);
        assert_eq!(round_tripped.14, status.14);
    }

    #[test]
    fn requests_round_trip_through_json() {
        for request in [
            Request::Next,
            Request::PlayAt { index: 3 },
            Request::SeekTo { position_secs: 12.5 },
            Request::MoveInQueue { from: 1, to: 4 },
        ] {
            let json = serde_json::to_string(&request).unwrap();
            let _: Request = serde_json::from_str(&json).unwrap();
        }
    }
}
