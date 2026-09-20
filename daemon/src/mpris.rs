//! Implements the two MPRIS interfaces (`org.mpris.MediaPlayer2` and
//! `org.mpris.MediaPlayer2.Player`) on top of the playback engine, using the
//! `mpris-server` crate rather than hand-rolling the D-Bus interface — the
//! spec has a lot of required properties/methods, and this crate is a
//! purpose-built, already-correct implementation of that surface.

use std::sync::Arc;

use mpris_server::{
    LoopStatus, Metadata, PlaybackRate, PlaybackStatus, PlayerInterface, RootInterface, Time,
    TrackId, Volume, zbus::fdo,
};
use tokio::sync::Notify;

use crate::playback::{PlaybackHandle, RepeatMode, Status};

pub struct MprisPlayer {
    pub playback: PlaybackHandle,
    pub shutdown: Arc<Notify>,
}

impl RootInterface for MprisPlayer {
    async fn raise(&self) -> fdo::Result<()> {
        // No GUI to raise — a TUI in a terminal isn't something we can bring
        // to the front over D-Bus. `can_raise` below is false, so a
        // well-behaved client shouldn't call this anyway.
        Ok(())
    }

    async fn quit(&self) -> fdo::Result<()> {
        self.shutdown.notify_one();
        Ok(())
    }

    async fn can_quit(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn set_fullscreen(&self, _fullscreen: bool) -> mpris_server::zbus::Result<()> {
        Ok(())
    }

    async fn can_set_fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn can_raise(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn has_track_list(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn identity(&self) -> fdo::Result<String> {
        Ok("Maraetai".into())
    }

    async fn desktop_entry(&self) -> fdo::Result<String> {
        // No .desktop file ships with a TUI/daemon pair — leaving this empty
        // is within spec (it's an optional property).
        Ok(String::new())
    }

    async fn supported_uri_schemes(&self) -> fdo::Result<Vec<String>> {
        Ok(vec!["https".into()])
    }

    async fn supported_mime_types(&self) -> fdo::Result<Vec<String>> {
        Ok(vec![
            "audio/flac".into(),
            "audio/mpeg".into(),
            "audio/ogg".into(),
            "audio/mp4".into(),
        ])
    }
}

impl PlayerInterface for MprisPlayer {
    async fn next(&self) -> fdo::Result<()> {
        self.playback.next();
        Ok(())
    }

    async fn previous(&self) -> fdo::Result<()> {
        self.playback.previous();
        Ok(())
    }

    async fn pause(&self) -> fdo::Result<()> {
        self.playback.pause();
        Ok(())
    }

    async fn play_pause(&self) -> fdo::Result<()> {
        match self.playback.snapshot().status {
            Status::Playing => self.playback.pause(),
            Status::Paused | Status::Stopped => self.playback.resume(),
        }
        Ok(())
    }

    async fn stop(&self) -> fdo::Result<()> {
        self.playback.stop();
        Ok(())
    }

    async fn play(&self) -> fdo::Result<()> {
        self.playback.resume();
        Ok(())
    }

    async fn seek(&self, offset: Time) -> fdo::Result<()> {
        let current = self.playback.snapshot().position;
        let micros = current.as_micros() as i64 + offset.as_micros();
        let target = std::time::Duration::from_micros(micros.max(0) as u64);
        self.playback.seek(target);
        Ok(())
    }

    async fn set_position(&self, _track_id: TrackId, position: Time) -> fdo::Result<()> {
        let micros = position.as_micros().max(0) as u64;
        self.playback.seek(std::time::Duration::from_micros(micros));
        Ok(())
    }

    async fn open_uri(&self, _uri: String) -> fdo::Result<()> {
        // Arbitrary URI playback isn't supported in v1 — only tracks the TUI
        // resolves through `maraetai-service` — so this is a no-op rather
        // than an error, matching how many MPRIS players treat unsupported
        // optional methods.
        Ok(())
    }

    async fn playback_status(&self) -> fdo::Result<PlaybackStatus> {
        Ok(match self.playback.snapshot().status {
            Status::Playing => PlaybackStatus::Playing,
            Status::Paused => PlaybackStatus::Paused,
            Status::Stopped => PlaybackStatus::Stopped,
        })
    }

    async fn loop_status(&self) -> fdo::Result<LoopStatus> {
        Ok(match self.playback.snapshot().repeat {
            RepeatMode::Off => LoopStatus::None,
            RepeatMode::Track => LoopStatus::Track,
            RepeatMode::Queue => LoopStatus::Playlist,
        })
    }

    async fn set_loop_status(&self, loop_status: LoopStatus) -> mpris_server::zbus::Result<()> {
        let repeat = match loop_status {
            LoopStatus::None => RepeatMode::Off,
            LoopStatus::Track => RepeatMode::Track,
            LoopStatus::Playlist => RepeatMode::Queue,
        };
        self.playback.set_repeat(repeat);
        Ok(())
    }

    async fn rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn set_rate(&self, _rate: PlaybackRate) -> mpris_server::zbus::Result<()> {
        Ok(())
    }

    async fn shuffle(&self) -> fdo::Result<bool> {
        Ok(self.playback.snapshot().shuffle)
    }

    async fn set_shuffle(&self, shuffle: bool) -> mpris_server::zbus::Result<()> {
        self.playback.set_shuffle(shuffle);
        Ok(())
    }

    async fn metadata(&self) -> fdo::Result<Metadata> {
        let snap = self.playback.snapshot();
        let Some(track) = snap.track else {
            return Ok(Metadata::new());
        };
        let mut builder = Metadata::builder()
            .title(track.title)
            .album(track.album)
            .artist([track.artist])
            .trackid(
                TrackId::try_from("/com/maraetai/track/current")
                    .unwrap_or(TrackId::NO_TRACK),
            );
        if let Some(dur) = track.duration {
            builder = builder.length(Time::from_micros(dur.as_micros() as i64));
        }
        if let Some(art) = track.art_url {
            builder = builder.art_url(art);
        }
        Ok(builder.build())
    }

    async fn volume(&self) -> fdo::Result<Volume> {
        Ok(self.playback.snapshot().volume as f64)
    }

    async fn set_volume(&self, volume: Volume) -> mpris_server::zbus::Result<()> {
        self.playback.set_volume(volume.clamp(0.0, 1.0) as f32);
        Ok(())
    }

    async fn position(&self) -> fdo::Result<Time> {
        let micros = self.playback.snapshot().position.as_micros() as i64;
        Ok(Time::from_micros(micros))
    }

    async fn minimum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn maximum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn can_go_next(&self) -> fdo::Result<bool> {
        Ok(self.playback.snapshot().has_next())
    }

    async fn can_go_previous(&self) -> fdo::Result<bool> {
        // Always true in practice — "previous" restarts the current track
        // when there's nothing earlier in the queue, matching common player
        // convention (see playback.rs's RESTART_THRESHOLD).
        Ok(true)
    }

    async fn can_play(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_pause(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_seek(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_control(&self) -> fdo::Result<bool> {
        Ok(true)
    }
}
