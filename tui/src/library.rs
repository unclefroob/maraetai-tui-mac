//! A minimal Subsonic API client for browsing — the same endpoints and auth
//! scheme `maraetai-service`'s own web client uses (`internal/web/static/
//! api.js` in that repo), so response shapes are known-good rather than
//! guessed: `getAlbumList2` for the album list, `getAlbum` for an album's
//! songs, `search3` for search.

use anyhow::{Context, Result, bail};
use maraetai_common::Credentials;
use maraetai_common::auth::AuthParams;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct Album {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub artist: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Artist {
    pub id: String,
    pub name: String,
    #[serde(default, rename = "albumCount")]
    pub album_count: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    #[serde(default, rename = "songCount")]
    pub song_count: u32,
    /// The Subsonic username that owns this playlist. Used to filter
    /// `getPlaylists` down to just the caller's own — Navidrome's endpoint
    /// (unlike the Subsonic spec's stated default) also returns other
    /// users' *public* playlists when no `username` filter is given.
    #[serde(default)]
    pub owner: String,
}

/// One entry of `getArtistInfo2`'s `similarArtist` list — `id` is what the
/// Info screen's "jump to this artist" (`Enter`) actually navigates to.
#[derive(Debug, Clone, Deserialize)]
pub struct SimilarArtist {
    pub id: String,
    pub name: String,
}

/// Artist biography + similar-artist recommendations, from OpenSubsonic's
/// `getArtistInfo2` — a maraetai-service-fronted Navidrome populates this
/// from its own metadata scan/Last.fm lookup, not something this client
/// computes. `Default`s to "nothing found" rather than an error, matching
/// this project's lyrics convention: an older server or an artist with no
/// info at all looks identical to a request that simply found nothing.
#[derive(Debug, Clone, Default)]
pub struct ArtistInfo {
    /// HTML tags already stripped (see `strip_html`) — safe to render
    /// directly in the terminal.
    pub biography: String,
    pub similar_artists: Vec<SimilarArtist>,
}

/// Album notes, from OpenSubsonic's `getAlbumInfo2` — same "empty means
/// nothing found" convention as `ArtistInfo`.
#[derive(Debug, Clone, Default)]
pub struct AlbumInfo {
    /// HTML tags already stripped (see `strip_html`).
    pub notes: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawArtistInfo {
    #[serde(default)]
    biography: String,
    #[serde(default, rename = "similarArtist")]
    similar_artist: Vec<SimilarArtist>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawAlbumInfo {
    #[serde(default)]
    notes: String,
}

/// Strips HTML tags and unescapes the handful of entities that
/// Last.fm-sourced biography/notes text actually contains (Navidrome passes
/// that text through close to verbatim, anchor tags and all — the same
/// "Read more on Last.fm" link every such bio ends with). Hand-rolled rather
/// than pulling in an HTML parser for what's really just one fixed, simple
/// shape — same reasoning as `parse_lrc`.
fn strip_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .trim()
        .to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct Genre {
    pub value: String,
    #[serde(default, rename = "songCount")]
    pub song_count: u32,
    #[serde(default, rename = "albumCount")]
    pub album_count: u32,
}

/// OpenSubsonic's `replayGain` object — `trackGain`/`albumGain` in
/// decibels, relative to the format's reference loudness. Either or both
/// may be absent (an older server, or a file with no embedded ReplayGain
/// tags at all).
#[derive(Debug, Clone, Deserialize)]
pub struct ReplayGain {
    #[serde(default, rename = "trackGain")]
    pub track_gain: Option<f64>,
    #[serde(default, rename = "albumGain")]
    pub album_gain: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Song {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    /// Seconds. Subsonic sends an integer; `f64` accepts that fine and lets
    /// the daemon's `PlayUrl` (which wants seconds as `f64`) take it as-is.
    #[serde(default)]
    pub duration: f64,
    #[serde(default, rename = "coverArt")]
    pub cover_art: Option<String>,
    /// File extension as Navidrome reports it (e.g. "flac", "mp3") — used to
    /// derive the format/lossless display, see [`format_label`].
    #[serde(default)]
    pub suffix: String,
    #[serde(default, rename = "bitRate")]
    pub bit_rate: Option<u32>,
    /// An ISO-8601 timestamp if this song is starred/favorited, per the
    /// Subsonic API convention — absent (not a bool) otherwise. Only the
    /// presence matters here; the actual timestamp value is never shown.
    #[serde(default)]
    pub starred: Option<String>,
    #[serde(default, rename = "replayGain")]
    pub replay_gain: Option<ReplayGain>,
}

impl Song {
    pub fn is_starred(&self) -> bool {
        self.starred.is_some()
    }

    /// The gain (in dB) to apply for ReplayGain when this song plays —
    /// track gain preferred over album gain (the standard "per-track
    /// normalization" convention most players default to), or `0.0` (no
    /// adjustment) if the server sent neither — most servers, and most
    /// files with no embedded ReplayGain tags. A plain `f64` rather than
    /// `Option<f64>`: the daemon's D-Bus control channel can't carry an
    /// `Option` (see `daemon::control::QueueEntry`), and a genuine 0 dB
    /// tag would mean the same "no adjustment" outcome anyway.
    pub fn replay_gain_db(&self) -> f64 {
        self.replay_gain.as_ref().and_then(|rg| rg.track_gain.or(rg.album_gain)).unwrap_or(0.0)
    }
}

/// Suffixes for formats that are lossless *as a container* — `m4a`/`mp4`
/// deliberately excluded: that extension is used for both lossy AAC and
/// lossless ALAC, and Subsonic doesn't reliably disambiguate which via
/// `suffix` alone, so it's shown as (potentially) lossy rather than guessed.
const LOSSLESS_SUFFIXES: &[&str] = &["flac", "alac", "ape", "wav", "wv", "aiff", "aif", "dsf", "dff"];

/// A short display label for a song's format — e.g. `"FLAC"` for a lossless
/// file, `"MP3 320"` for a lossy one with a known bitrate — plus whether
/// it's lossless, for styling. Empty suffix (metadata Navidrome didn't send)
/// yields an empty label rather than a guess.
pub fn format_label(suffix: &str, bit_rate: Option<u32>) -> (String, bool) {
    if suffix.is_empty() {
        return (String::new(), false);
    }
    let lossless = LOSSLESS_SUFFIXES.contains(&suffix.to_ascii_lowercase().as_str());
    let upper = suffix.to_ascii_uppercase();
    let label = match (lossless, bit_rate) {
        (true, _) => upper,
        (false, Some(kbps)) if kbps > 0 => format!("{upper} {kbps}"),
        (false, _) => upper,
    };
    (label, lossless)
}

/// One line of lyrics. `start_ms` is `Some` when the source was time-synced
/// (either OpenSubsonic's structured `getLyricsBySongId`, or an LRC-format
/// blob from the legacy `getLyrics`) — `None` for plain unsynced text, which
/// is rendered as a single line with no highlight-as-you-play behavior.
#[derive(Debug, Clone, PartialEq)]
pub struct LyricLine {
    pub start_ms: Option<i64>,
    pub text: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct StructuredLyrics {
    #[serde(default)]
    synced: bool,
    #[serde(default)]
    offset: Option<i64>,
    #[serde(default)]
    line: Vec<StructuredLyricLine>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct StructuredLyricLine {
    #[serde(default)]
    start: Option<i64>,
    #[serde(default)]
    value: String,
}

/// Parses simple LRC-format lyrics (`[mm:ss.xx]text`, possibly several
/// stacked timestamps per line, plus an optional leading `[offset:±ms]`
/// tag) — hand-rolled rather than pulling in a regex dependency for one
/// small, fixed grammar. A line with no recognizable leading timestamp is
/// skipped; if nothing in the blob parses as LRC at all, the caller falls
/// back to treating the whole thing as one unsynced block.
fn parse_lrc(text: &str) -> Vec<LyricLine> {
    let mut offset_ms: i64 = 0;
    let mut lines = Vec::new();

    for raw in text.lines() {
        let line = raw.trim().trim_start_matches('\u{feff}'); // strip a UTF-8 BOM, if present
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("[offset:").and_then(|s| s.strip_suffix(']')) {
            if let Ok(ms) = rest.parse::<i64>() {
                offset_ms = ms;
            }
            continue;
        }

        let mut rest = line;
        let mut timestamps = Vec::new();
        while let Some((ms, tail)) = parse_lrc_timestamp(rest) {
            timestamps.push(ms);
            rest = tail;
        }
        if timestamps.is_empty() {
            continue;
        }
        let text = rest.trim().to_string();
        for ms in timestamps {
            lines.push(LyricLine { start_ms: Some(ms + offset_ms), text: text.clone() });
        }
    }

    lines.sort_by_key(|l| l.start_ms.unwrap_or(i64::MAX));
    lines
}

/// Parses one leading `[mm:ss.xx]`/`[mm:ss.xxx]` timestamp off the front of
/// `s`, returning its value in milliseconds and the remainder of the
/// string — or `None` if `s` doesn't start with one.
fn parse_lrc_timestamp(s: &str) -> Option<(i64, &str)> {
    let inner = s.strip_prefix('[')?;
    let close = inner.find(']')?;
    let (tag, tail) = (&inner[..close], &inner[close + 1..]);

    let (mins_str, rest) = tag.split_once(':')?;
    let (secs_str, frac_str) = rest.split_once('.')?;
    let mins: i64 = mins_str.parse().ok()?;
    let secs: i64 = secs_str.parse().ok()?;
    if !(2..=3).contains(&frac_str.len()) || !frac_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let frac: i64 = frac_str.parse().ok()?;
    let frac_ms = if frac_str.len() == 2 { frac * 10 } else { frac };
    Some((mins * 60_000 + secs * 1_000 + frac_ms, tail))
}

#[derive(Clone)]
pub struct Client {
    creds: Credentials,
    http: reqwest::Client,
}

impl Client {
    pub fn new(creds: Credentials) -> Self {
        Self {
            creds,
            http: reqwest::Client::new(),
        }
    }

    /// Builds an authenticated, freshly-salted `/rest/stream.view` URL for a
    /// song id — used both by browsing (play a selected song) and by the
    /// `maraetai play <song-id>` CLI command.
    pub fn stream_url(&self, song_id: &str) -> String {
        self.authed_url("rest/stream.view", &[("id", song_id)])
    }

    /// Builds an authenticated `/rest/getCoverArt.view` URL, if `cover_art`
    /// (the opaque id Subsonic gives each song/album) is present. Feeds
    /// MPRIS's `art_url` metadata field — desktop notification widgets and
    /// media-key overlays that show cover art read this.
    pub fn cover_art_url(&self, cover_art: &Option<String>) -> String {
        match cover_art {
            Some(id) => self.authed_url("rest/getCoverArt.view", &[("id", id)]),
            None => String::new(),
        }
    }

    pub async fn albums(&self) -> Result<Vec<Album>> {
        let mut root = self
            .get_json("rest/getAlbumList2.view", &[("type", "alphabeticalByName"), ("size", "200")])
            .await?;
        parse(root["albumList2"]["album"].take())
    }

    /// The same `getAlbumList2` endpoint as `albums()`, sorted by most
    /// recently added instead of alphabetically — for the Albums tab's
    /// sort toggle (`t`).
    pub async fn newest_albums(&self) -> Result<Vec<Album>> {
        let mut root = self.get_json("rest/getAlbumList2.view", &[("type", "newest"), ("size", "200")]).await?;
        parse(root["albumList2"]["album"].take())
    }

    pub async fn album_songs(&self, album_id: &str) -> Result<Vec<Song>> {
        let mut root = self.get_json("rest/getAlbum.view", &[("id", album_id)]).await?;
        parse(root["album"]["song"].take())
    }

    pub async fn search(&self, query: &str) -> Result<Vec<Song>> {
        let mut root = self
            .get_json(
                "rest/search3.view",
                &[("query", query), ("artistCount", "0"), ("albumCount", "0"), ("songCount", "50")],
            )
            .await?;
        parse(root["searchResult3"]["song"].take())
    }

    /// The full artist index, flattened across Subsonic's alphabetical index
    /// groups (`artists.index[].artist[]`) — the same flattening the web
    /// client does client-side.
    pub async fn artists(&self) -> Result<Vec<Artist>> {
        let root = self.get_json("rest/getArtists.view", &[]).await?;
        let groups = root["artists"]["index"].as_array().cloned().unwrap_or_default();
        let mut out = Vec::new();
        for mut group in groups {
            out.extend(parse::<Artist>(group["artist"].take())?);
        }
        Ok(out)
    }

    pub async fn artist_albums(&self, artist_id: &str) -> Result<Vec<Album>> {
        let mut root = self.get_json("rest/getArtist.view", &[("id", artist_id)]).await?;
        parse(root["artist"]["album"].take())
    }

    /// Only the caller's *own* playlists — Navidrome's `getPlaylists`
    /// returns other users' public playlists too when called with no
    /// `username` filter, so this filters client-side by `owner` rather
    /// than relying on server-side behavior that doesn't match the
    /// Subsonic spec's own documented default.
    pub async fn playlists(&self) -> Result<Vec<Playlist>> {
        let mut root = self.get_json("rest/getPlaylists.view", &[]).await?;
        let all: Vec<Playlist> = parse(root["playlists"]["playlist"].take())?;
        Ok(all.into_iter().filter(|p| p.owner == self.creds.username).collect())
    }

    /// Note: Subsonic's `getPlaylist` nests its songs under `entry`, not
    /// `song` (unlike every other endpoint here) — matched exactly against
    /// `maraetai-service`'s web client, which has the same quirk.
    pub async fn playlist_songs(&self, playlist_id: &str) -> Result<Vec<Song>> {
        let mut root = self.get_json("rest/getPlaylist.view", &[("id", playlist_id)]).await?;
        parse(root["playlist"]["entry"].take())
    }

    pub async fn genres(&self) -> Result<Vec<Genre>> {
        let mut root = self.get_json("rest/getGenres.view", &[]).await?;
        parse(root["genres"]["genre"].take())
    }

    pub async fn albums_by_genre(&self, genre: &str) -> Result<Vec<Album>> {
        let mut root = self
            .get_json("rest/getAlbumList2.view", &[("type", "byGenre"), ("genre", genre), ("size", "200")])
            .await?;
        parse(root["albumList2"]["album"].take())
    }

    /// `maraetai-service`'s native `getRecentlyPlayed` — per-song
    /// de-duplicated, most-recent-first listen history, built from the
    /// server's own play store (which the daemon now actually feeds via
    /// scrobbling). Not part of the Subsonic spec; a plain Navidrome/
    /// Subsonic server without this proxy in front of it would 404, which
    /// just surfaces as an error like any other failed fetch.
    pub async fn recently_played(&self) -> Result<Vec<Song>> {
        let mut root = self.get_json("rest/getRecentlyPlayed.view", &[]).await?;
        parse(root["recentlyPlayed"]["song"].take())
    }

    /// `maraetai-service`'s native `getOnRepeat` — the most-replayed songs,
    /// from the same play store.
    pub async fn on_repeat(&self) -> Result<Vec<Song>> {
        let mut root = self.get_json("rest/getOnRepeat.view", &[]).await?;
        parse(root["onRepeat"]["song"].take())
    }

    /// `maraetai-service`'s native `getSongsForYou` — a personalized mix
    /// built from play history.
    pub async fn songs_for_you(&self) -> Result<Vec<Song>> {
        let mut root = self.get_json("rest/getSongsForYou.view", &[]).await?;
        parse(root["songsForYou"]["song"].take())
    }

    /// `maraetai-service`'s native `getFavourites` — a paged view of the
    /// user's starred songs (the proxy pages the non-pageable `getStarred2`
    /// on the fast side).
    pub async fn favourites(&self) -> Result<Vec<Song>> {
        let mut root = self.get_json("rest/getFavourites.view", &[]).await?;
        parse(root["favourites"]["song"].take())
    }

    /// Stars/unstars a song — the standard Subsonic `star`/`unstar`
    /// endpoints, passed straight through `maraetai-service`'s proxy to the
    /// real Navidrome underneath (no native endpoint needed for this half —
    /// only *reading back* the starred list is a maraetai-service addition).
    pub async fn set_starred(&self, song_id: &str, starred: bool) -> Result<()> {
        let path = if starred { "rest/star.view" } else { "rest/unstar.view" };
        self.get_json(path, &[("id", song_id)]).await?;
        Ok(())
    }

    /// Creates a new playlist with the given name, seeded with `song_ids`
    /// (empty is fine — a playlist can start with nothing and be built up
    /// via `add_song_to_playlist`). Returns the new playlist's id, so
    /// callers like "save the current queue as a playlist" don't need a
    /// separate `playlists()` round-trip just to find what they made.
    /// Standard Subsonic `createPlaylist`, passed straight through
    /// `maraetai-service`'s proxy — no native endpoint involved.
    pub async fn create_playlist(&self, name: &str, song_ids: &[String]) -> Result<String> {
        let mut params: Vec<(&str, &str)> = vec![("name", name)];
        params.extend(song_ids.iter().map(|id| ("songId", id.as_str())));
        let root = self.get_json("rest/createPlaylist.view", &params).await?;
        root["playlist"]["id"].as_str().map(String::from).context("createPlaylist response had no playlist id")
    }

    pub async fn delete_playlist(&self, playlist_id: &str) -> Result<()> {
        self.get_json("rest/deletePlaylist.view", &[("id", playlist_id)]).await?;
        Ok(())
    }

    pub async fn rename_playlist(&self, playlist_id: &str, name: &str) -> Result<()> {
        self.get_json("rest/updatePlaylist.view", &[("playlistId", playlist_id), ("name", name)]).await?;
        Ok(())
    }

    pub async fn add_song_to_playlist(&self, playlist_id: &str, song_id: &str) -> Result<()> {
        self.get_json("rest/updatePlaylist.view", &[("playlistId", playlist_id), ("songIdToAdd", song_id)]).await?;
        Ok(())
    }

    /// Removes one song from a playlist by its position within that
    /// playlist — Subsonic identifies playlist entries by index rather than
    /// song id (the same song can appear more than once), via
    /// `updatePlaylist`'s `songIndexToRemove`. Callers must re-fetch
    /// `playlist_songs` afterward: every later index has now shifted down
    /// by one.
    pub async fn remove_song_from_playlist(&self, playlist_id: &str, song_index: usize) -> Result<()> {
        let index = song_index.to_string();
        self.get_json("rest/updatePlaylist.view", &[("playlistId", playlist_id), ("songIndexToRemove", &index)]).await?;
        Ok(())
    }

    /// Artist biography + similar artists, from OpenSubsonic's
    /// `getArtistInfo2` — a `Default` (empty) result, not an error, when the
    /// server has nothing for this artist (an older server, or simply no
    /// match found), same convention as `lyrics`.
    pub async fn artist_info(&self, artist_id: &str) -> Result<ArtistInfo> {
        let root = self.get_json("rest/getArtistInfo2.view", &[("id", artist_id)]).await?;
        if root["artistInfo2"].is_null() {
            return Ok(ArtistInfo::default());
        }
        let raw: RawArtistInfo =
            serde_json::from_value(root["artistInfo2"].clone()).context("unexpected artistInfo2 response shape")?;
        Ok(ArtistInfo { biography: strip_html(&raw.biography), similar_artists: raw.similar_artist })
    }

    /// Album notes, from OpenSubsonic's `getAlbumInfo2` — same "empty, not
    /// an error" convention as `artist_info`.
    pub async fn album_info(&self, album_id: &str) -> Result<AlbumInfo> {
        let root = self.get_json("rest/getAlbumInfo2.view", &[("id", album_id)]).await?;
        if root["albumInfo"].is_null() {
            return Ok(AlbumInfo::default());
        }
        let raw: RawAlbumInfo =
            serde_json::from_value(root["albumInfo"].clone()).context("unexpected albumInfo response shape")?;
        Ok(AlbumInfo { notes: strip_html(&raw.notes) })
    }

    /// Fetches lyrics for a track: OpenSubsonic's id-based, potentially
    /// time-synced `getLyricsBySongId` first, falling back to the legacy
    /// artist+title `getLyrics` (parsed as LRC if it looks synced, else
    /// treated as one unsynced block) — the same pipeline every other
    /// maraetai client already uses. Never errors: a server that doesn't
    /// support the newer endpoint, has no lyrics at all, or is briefly
    /// unreachable all just yield an empty list, since "no lyrics" and "the
    /// request failed" look identical to the user either way.
    pub async fn lyrics(&self, song_id: &str, artist: &str, title: &str) -> Vec<LyricLine> {
        if let Ok(lines) = self.lyrics_by_song_id(song_id).await {
            if !lines.is_empty() {
                return lines;
            }
        }
        self.lyrics_legacy(artist, title).await.unwrap_or_default()
    }

    async fn lyrics_by_song_id(&self, song_id: &str) -> Result<Vec<LyricLine>> {
        let root = self.get_json("rest/getLyricsBySongId.view", &[("id", song_id)]).await?;
        let structured: Vec<StructuredLyrics> = parse(root["lyricsList"]["structuredLyrics"].clone())?;
        // Prefer a synced entry when more than one language/version comes back.
        let Some(chosen) = structured.iter().find(|s| s.synced).or_else(|| structured.first()) else {
            return Ok(Vec::new());
        };
        let offset = chosen.offset.unwrap_or(0);
        let mut lines: Vec<LyricLine> = chosen
            .line
            .iter()
            .map(|l| LyricLine { start_ms: l.start.map(|s| s + offset), text: l.value.clone() })
            .collect();
        lines.sort_by_key(|l| l.start_ms.unwrap_or(i64::MAX));
        Ok(lines)
    }

    async fn lyrics_legacy(&self, artist: &str, title: &str) -> Result<Vec<LyricLine>> {
        let root = self.get_json("rest/getLyrics.view", &[("artist", artist), ("title", title)]).await?;
        let text = root["lyrics"]["value"].as_str().unwrap_or("").to_string();
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let parsed = parse_lrc(&text);
        if !parsed.is_empty() {
            return Ok(parsed);
        }
        // Plain, unsynced text — one block, no timestamp.
        Ok(vec![LyricLine { start_ms: None, text }])
    }

    fn authed_url(&self, path: &str, extra: &[(&str, &str)]) -> String {
        let auth = AuthParams::new(&self.creds.username, &self.creds.password);
        let mut pairs: Vec<(String, String)> =
            extra.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        auth.append_to(&mut pairs);
        pairs.push(("f".to_string(), "json".to_string()));
        let query = pairs
            .iter()
            .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{}/{path}?{query}", self.creds.server_url.trim_end_matches('/'))
    }

    async fn get_json(&self, path: &str, extra: &[(&str, &str)]) -> Result<Value> {
        let url = self.authed_url(path, extra);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("requesting {path}"))?
            .error_for_status()
            .with_context(|| format!("{path} returned an error status"))?;
        let mut body: Value = resp.json().await.with_context(|| format!("parsing {path} response"))?;
        let root = body["subsonic-response"].take();
        if root["status"] != "ok" {
            let msg = root["error"]["message"].as_str().unwrap_or("unknown error");
            bail!("{path}: {msg}");
        }
        Ok(root)
    }
}

/// Parses a JSON array value into `Vec<T>`, treating "field absent" (a
/// missing array — e.g. an album with no songs, a query with no results) the
/// same as "empty", rather than an error.
fn parse<T: for<'de> Deserialize<'de>>(value: Value) -> Result<Vec<T>> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    serde_json::from_value(value).context("unexpected response shape")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Starts a one-shot async HTTP server that always returns `body` as a
    /// 200 JSON response, then returns its base URL. Used to check that
    /// `Client`'s JSON-path navigation (`root["albumList2"]["album"]` etc.)
    /// actually matches real Subsonic response shapes, not just that it
    /// compiles.
    async fn respond_once(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        format!("http://{addr}")
    }

    /// Like `respond_once`, but keeps serving the same JSON body to every
    /// connection instead of exiting after the first — for tests exercising
    /// a pipeline that makes more than one request (lyrics' structured-then-
    /// legacy fallback).
    async fn respond_always(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { break };
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        format!("http://{addr}")
    }

    /// Like `respond_once`, but also hands back the captured request line +
    /// headers so a test can assert on the exact path/query the client sent
    /// — for endpoints where *which* URL was requested is the thing under
    /// test, not just how the response gets parsed.
    async fn respond_once_capturing(body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
            request
        });
        (format!("http://{addr}"), handle)
    }

    fn test_creds(server_url: String) -> Credentials {
        Credentials {
            server_url,
            username: "alice".into(),
            password: "hunter2".into(),
        }
    }

    #[tokio::test]
    async fn parses_real_shaped_recently_played() {
        // Shape matches maraetai-service's actual subsonic.RecentlyPlayed
        // container (internal/subsonic/response.go): a plain song list
        // under "recentlyPlayed", each with the non-standard playedAt
        // extension.
        let url = respond_once(
            r#"{"subsonic-response":{"status":"ok","recentlyPlayed":{"song":[
                {"id":"s1","title":"Angel","artist":"Massive Attack","album":"Mezzanine","duration":379,"playedAt":1700000000}
            ]}}}"#,
        )
        .await;
        let client = Client::new(test_creds(url));
        let songs = client.recently_played().await.unwrap();
        assert_eq!(songs.len(), 1);
        assert_eq!(songs[0].title, "Angel");
    }

    #[tokio::test]
    async fn parses_real_shaped_on_repeat() {
        let url = respond_once(
            r#"{"subsonic-response":{"status":"ok","onRepeat":{"song":[
                {"id":"s1","title":"Teardrop","artist":"Massive Attack","playCount":12}
            ]}}}"#,
        )
        .await;
        let client = Client::new(test_creds(url));
        let songs = client.on_repeat().await.unwrap();
        assert_eq!(songs.len(), 1);
        assert_eq!(songs[0].title, "Teardrop");
    }

    #[tokio::test]
    async fn parses_real_shaped_songs_for_you() {
        let url = respond_once(
            r#"{"subsonic-response":{"status":"ok","songsForYou":{"song":[
                {"id":"s1","title":"Glory Box","artist":"Portishead","reason":"Because you play Massive Attack"}
            ]}}}"#,
        )
        .await;
        let client = Client::new(test_creds(url));
        let songs = client.songs_for_you().await.unwrap();
        assert_eq!(songs.len(), 1);
        assert_eq!(songs[0].title, "Glory Box");
    }

    #[tokio::test]
    async fn parses_real_shaped_favourites_including_starred_flag() {
        let url = respond_once(
            r#"{"subsonic-response":{"status":"ok","favourites":{"song":[
                {"id":"s1","title":"Angel","starred":"2024-01-01T00:00:00.000Z"}
            ]}}}"#,
        )
        .await;
        let client = Client::new(test_creds(url));
        let songs = client.favourites().await.unwrap();
        assert_eq!(songs.len(), 1);
        assert!(songs[0].is_starred());
    }

    #[test]
    fn song_without_a_starred_field_is_not_starred() {
        let song: Song = serde_json::from_str(r#"{"id":"s1","title":"Angel"}"#).unwrap();
        assert!(!song.is_starred());
    }

    #[test]
    fn replay_gain_prefers_track_gain_over_album_gain() {
        let song: Song = serde_json::from_str(
            r#"{"id":"s1","title":"Angel","replayGain":{"trackGain":-6.5,"albumGain":-7.2}}"#,
        )
        .unwrap();
        assert_eq!(song.replay_gain_db(), -6.5);
    }

    #[test]
    fn replay_gain_falls_back_to_album_gain_when_track_gain_is_absent() {
        let song: Song = serde_json::from_str(r#"{"id":"s1","title":"Angel","replayGain":{"albumGain":-7.2}}"#).unwrap();
        assert_eq!(song.replay_gain_db(), -7.2);
    }

    #[test]
    fn replay_gain_is_zero_without_the_field_at_all() {
        let song: Song = serde_json::from_str(r#"{"id":"s1","title":"Angel"}"#).unwrap();
        assert_eq!(song.replay_gain_db(), 0.0);
    }

    /// Confirms `set_starred` hits the exact endpoints/params the Subsonic
    /// spec (and every other maraetai client) uses — `star`/`unstar` with an
    /// `id` query param — by capturing the real outgoing request rather than
    /// just trusting the URL-building code reads correctly.
    #[tokio::test]
    async fn set_starred_true_requests_star_view_with_the_song_id() {
        let (url, request) = respond_once_capturing(r#"{"subsonic-response":{"status":"ok"}}"#).await;
        let client = Client::new(test_creds(url));
        client.set_starred("song123", true).await.unwrap();
        let request_line = request.await.unwrap();
        assert!(request_line.starts_with("GET /rest/star.view?"), "got: {request_line}");
        assert!(request_line.contains("id=song123"), "got: {request_line}");
    }

    #[tokio::test]
    async fn set_starred_false_requests_unstar_view() {
        let (url, request) = respond_once_capturing(r#"{"subsonic-response":{"status":"ok"}}"#).await;
        let client = Client::new(test_creds(url));
        client.set_starred("song123", false).await.unwrap();
        let request_line = request.await.unwrap();
        assert!(request_line.starts_with("GET /rest/unstar.view?"), "got: {request_line}");
    }

    #[tokio::test]
    async fn create_playlist_sends_name_and_repeated_song_id_params() {
        let (url, request) = respond_once_capturing(
            r#"{"subsonic-response":{"status":"ok","playlist":{"id":"pl9","name":"Mix"}}}"#,
        )
        .await;
        let client = Client::new(test_creds(url));
        let id = client.create_playlist("Mix", &["s1".into(), "s2".into()]).await.unwrap();
        assert_eq!(id, "pl9");
        let request_line = request.await.unwrap();
        assert!(request_line.starts_with("GET /rest/createPlaylist.view?"), "got: {request_line}");
        assert!(request_line.contains("name=Mix"), "got: {request_line}");
        assert!(request_line.contains("songId=s1") && request_line.contains("songId=s2"), "got: {request_line}");
    }

    #[tokio::test]
    async fn delete_playlist_requests_delete_playlist_view() {
        let (url, request) = respond_once_capturing(r#"{"subsonic-response":{"status":"ok"}}"#).await;
        let client = Client::new(test_creds(url));
        client.delete_playlist("pl9").await.unwrap();
        let request_line = request.await.unwrap();
        assert!(request_line.starts_with("GET /rest/deletePlaylist.view?"), "got: {request_line}");
        assert!(request_line.contains("id=pl9"), "got: {request_line}");
    }

    #[tokio::test]
    async fn rename_playlist_sends_playlist_id_and_new_name() {
        let (url, request) = respond_once_capturing(r#"{"subsonic-response":{"status":"ok"}}"#).await;
        let client = Client::new(test_creds(url));
        client.rename_playlist("pl9", "New Name").await.unwrap();
        let request_line = request.await.unwrap();
        assert!(request_line.starts_with("GET /rest/updatePlaylist.view?"), "got: {request_line}");
        assert!(request_line.contains("playlistId=pl9"), "got: {request_line}");
        assert!(request_line.contains("name=New"), "got: {request_line}");
    }

    #[tokio::test]
    async fn add_song_to_playlist_sends_song_id_to_add() {
        let (url, request) = respond_once_capturing(r#"{"subsonic-response":{"status":"ok"}}"#).await;
        let client = Client::new(test_creds(url));
        client.add_song_to_playlist("pl9", "s1").await.unwrap();
        let request_line = request.await.unwrap();
        assert!(request_line.contains("playlistId=pl9"), "got: {request_line}");
        assert!(request_line.contains("songIdToAdd=s1"), "got: {request_line}");
    }

    #[tokio::test]
    async fn remove_song_from_playlist_sends_song_index_to_remove() {
        let (url, request) = respond_once_capturing(r#"{"subsonic-response":{"status":"ok"}}"#).await;
        let client = Client::new(test_creds(url));
        client.remove_song_from_playlist("pl9", 2).await.unwrap();
        let request_line = request.await.unwrap();
        assert!(request_line.contains("playlistId=pl9"), "got: {request_line}");
        assert!(request_line.contains("songIndexToRemove=2"), "got: {request_line}");
    }

    #[tokio::test]
    async fn parses_real_shaped_artist_info_and_strips_bio_html() {
        let url = respond_once(
            r#"{"subsonic-response":{"status":"ok","artistInfo2":{
                "biography":"A great band from Bristol. &lt;3 <a href=\"http://last.fm\">Read more</a>",
                "similarArtist":[{"id":"ar2","name":"Tricky"},{"id":"ar3","name":"Portishead"}]
            }}}"#,
        )
        .await;
        let client = Client::new(test_creds(url));
        let info = client.artist_info("ar1").await.unwrap();
        assert_eq!(info.biography, "A great band from Bristol. <3 Read more");
        assert_eq!(info.similar_artists.len(), 2);
        assert_eq!(info.similar_artists[0].name, "Tricky");
    }

    #[tokio::test]
    async fn missing_artist_info_is_empty_not_an_error() {
        let url = respond_once(r#"{"subsonic-response":{"status":"ok"}}"#).await;
        let client = Client::new(test_creds(url));
        let info = client.artist_info("ar1").await.unwrap();
        assert!(info.biography.is_empty());
        assert!(info.similar_artists.is_empty());
    }

    #[tokio::test]
    async fn parses_real_shaped_album_info() {
        let url = respond_once(
            r#"{"subsonic-response":{"status":"ok","albumInfo":{"notes":"Recorded in 1998."}}}"#,
        )
        .await;
        let client = Client::new(test_creds(url));
        let info = client.album_info("al1").await.unwrap();
        assert_eq!(info.notes, "Recorded in 1998.");
    }

    #[tokio::test]
    async fn missing_album_info_is_empty_not_an_error() {
        let url = respond_once(r#"{"subsonic-response":{"status":"ok"}}"#).await;
        let client = Client::new(test_creds(url));
        let info = client.album_info("al1").await.unwrap();
        assert!(info.notes.is_empty());
    }

    #[test]
    fn strip_html_removes_tags_and_unescapes_entities() {
        assert_eq!(strip_html("Tom &amp; Jerry <b>rock</b>"), "Tom & Jerry rock");
        assert_eq!(strip_html("  padded  "), "padded");
    }

    #[tokio::test]
    async fn newest_albums_requests_the_newest_sort_type() {
        let (url, request) = respond_once_capturing(
            r#"{"subsonic-response":{"status":"ok","albumList2":{"album":[
                {"id":"al3","name":"Blue Lines","artist":"Massive Attack"}
            ]}}}"#,
        )
        .await;
        let client = Client::new(test_creds(url));
        let albums = client.newest_albums().await.unwrap();
        assert_eq!(albums.len(), 1);
        let request_line = request.await.unwrap();
        assert!(request_line.starts_with("GET /rest/getAlbumList2.view?"), "got: {request_line}");
        assert!(request_line.contains("type=newest"), "got: {request_line}");
    }

    #[tokio::test]
    async fn parses_real_shaped_album_list() {
        // Field names/nesting match the Subsonic getAlbumList2 shape that
        // maraetai-service's own web client (internal/web/static/api.js)
        // already relies on.
        let url = respond_once(
            r#"{"subsonic-response":{"status":"ok","albumList2":{"album":[
                {"id":"al1","name":"Mezzanine","artist":"Massive Attack","coverArt":"ar-al1","songCount":11},
                {"id":"al2","name":"Dummy","artist":"Portishead","coverArt":"ar-al2","songCount":11}
            ]}}}"#,
        )
        .await;

        let client = Client::new(test_creds(url));
        let albums = client.albums().await.unwrap();
        assert_eq!(albums.len(), 2);
        assert_eq!(albums[0].id, "al1");
        assert_eq!(albums[0].name, "Mezzanine");
        assert_eq!(albums[0].artist, "Massive Attack");
    }

    #[tokio::test]
    async fn parses_real_shaped_album_songs() {
        let url = respond_once(
            r#"{"subsonic-response":{"status":"ok","album":{"id":"al1","name":"Mezzanine","song":[
                {"id":"s1","title":"Angel","artist":"Massive Attack","album":"Mezzanine","duration":379,"coverArt":"al1"},
                {"id":"s2","title":"Teardrop","artist":"Massive Attack","album":"Mezzanine","duration":331}
            ]}}}"#,
        )
        .await;

        let client = Client::new(test_creds(url));
        let songs = client.album_songs("al1").await.unwrap();
        assert_eq!(songs.len(), 2);
        assert_eq!(songs[0].title, "Angel");
        assert_eq!(songs[0].duration, 379.0);
        assert_eq!(songs[0].cover_art.as_deref(), Some("al1"));
        // Missing coverArt on the second song must not be a parse error.
        assert_eq!(songs[1].cover_art, None);
    }

    #[tokio::test]
    async fn playlists_filters_out_other_users_public_playlists() {
        // Navidrome's getPlaylists returns other users' *public* playlists
        // too when called with no `username` filter — this must be filtered
        // client-side down to just the caller's own (test_creds uses "alice").
        let url = respond_once(
            r#"{"subsonic-response":{"status":"ok","playlists":{"playlist":[
                {"id":"pl1","name":"My Mix","songCount":5,"owner":"alice"},
                {"id":"pl2","name":"Bob's Public Mix","songCount":3,"owner":"bob","public":true},
                {"id":"pl3","name":"Another Alice List","songCount":1,"owner":"alice"}
            ]}}}"#,
        )
        .await;

        let client = Client::new(test_creds(url));
        let playlists = client.playlists().await.unwrap();
        assert_eq!(playlists.len(), 2);
        assert!(playlists.iter().all(|p| p.owner == "alice"));
        assert_eq!(playlists[0].name, "My Mix");
        assert_eq!(playlists[1].name, "Another Alice List");
    }

    #[tokio::test]
    async fn empty_search_results_are_not_an_error() {
        let url = respond_once(
            r#"{"subsonic-response":{"status":"ok","searchResult3":{}}}"#,
        )
        .await;

        let client = Client::new(test_creds(url));
        let songs = client.search("nonexistent").await.unwrap();
        assert!(songs.is_empty());
    }

    #[tokio::test]
    async fn subsonic_error_status_surfaces_the_message() {
        let url = respond_once(
            r#"{"subsonic-response":{"status":"failed","error":{"code":40,"message":"Wrong username or password"}}}"#,
        )
        .await;

        let client = Client::new(test_creds(url));
        let err = client.albums().await.unwrap_err();
        assert!(err.to_string().contains("Wrong username or password"));
    }

    #[test]
    fn cover_art_url_is_empty_when_absent() {
        let client = Client::new(test_creds("https://example.com".into()));
        assert_eq!(client.cover_art_url(&None), "");
        assert!(client.cover_art_url(&Some("art1".into())).contains("id=art1"));
    }

    #[test]
    fn format_label_marks_flac_lossless_without_bitrate() {
        let (label, lossless) = format_label("flac", Some(1234));
        assert_eq!(label, "FLAC");
        assert!(lossless, "FLAC must be reported lossless");
    }

    #[test]
    fn format_label_shows_bitrate_for_lossy_formats() {
        let (label, lossless) = format_label("mp3", Some(320));
        assert_eq!(label, "MP3 320");
        assert!(!lossless);
    }

    #[test]
    fn format_label_handles_missing_bitrate() {
        let (label, lossless) = format_label("ogg", None);
        assert_eq!(label, "OGG");
        assert!(!lossless);
    }

    #[test]
    fn format_label_treats_m4a_as_lossy_not_a_guess() {
        // m4a is used for both lossy AAC and lossless ALAC; Subsonic's
        // `suffix` alone can't disambiguate, so this must not claim lossless.
        let (label, lossless) = format_label("m4a", Some(256));
        assert_eq!(label, "M4A 256");
        assert!(!lossless);
    }

    #[test]
    fn format_label_empty_suffix_yields_empty_label() {
        let (label, lossless) = format_label("", None);
        assert_eq!(label, "");
        assert!(!lossless);
    }

    #[test]
    fn format_label_is_case_insensitive_for_lossless_detection() {
        let (_, lossless) = format_label("FLAC", None);
        assert!(lossless);
    }

    #[test]
    fn lrc_parses_single_timestamp_lines() {
        let lines = parse_lrc("[00:12.50]Hello there\n[00:15.00]General Kenobi");
        assert_eq!(
            lines,
            vec![
                LyricLine { start_ms: Some(12_500), text: "Hello there".into() },
                LyricLine { start_ms: Some(15_000), text: "General Kenobi".into() },
            ]
        );
    }

    #[test]
    fn lrc_expands_multiple_stacked_timestamps_on_one_line() {
        // A common LRC convention for a repeated chorus line.
        let lines = parse_lrc("[00:10.00][00:40.00]La la la");
        assert_eq!(
            lines,
            vec![
                LyricLine { start_ms: Some(10_000), text: "La la la".into() },
                LyricLine { start_ms: Some(40_000), text: "La la la".into() },
            ]
        );
    }

    #[test]
    fn lrc_applies_the_offset_tag() {
        let lines = parse_lrc("[offset:500]\n[00:10.00]Delayed line");
        assert_eq!(lines, vec![LyricLine { start_ms: Some(10_500), text: "Delayed line".into() }]);
    }

    #[test]
    fn lrc_three_digit_fraction_is_milliseconds_not_centiseconds() {
        let lines = parse_lrc("[00:01.234]Fast line");
        assert_eq!(lines[0].start_ms, Some(1_234));
    }

    #[test]
    fn lrc_ignores_lines_with_no_timestamp() {
        // A stray metadata line (e.g. "[ar:Some Artist]") some LRC files
        // include — not a timestamp, so it must not produce a lyric line.
        let lines = parse_lrc("[ar:Some Artist]\n[00:05.00]Real line");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "Real line");
    }

    #[test]
    fn plain_text_with_no_lrc_timestamps_parses_as_empty() {
        assert!(parse_lrc("Just some plain lyrics\nwith no timing at all").is_empty());
    }

    #[tokio::test]
    async fn lyrics_prefers_structured_synced_over_legacy() {
        let url = respond_once(
            r#"{"subsonic-response":{"status":"ok","lyricsList":{"structuredLyrics":[
                {"synced":true,"offset":100,"line":[{"start":1000,"value":"Synced line"}]}
            ]}}}"#,
        )
        .await;

        let client = Client::new(test_creds(url));
        let lines = client.lyrics("s1", "Some Artist", "Some Title").await;
        assert_eq!(lines, vec![LyricLine { start_ms: Some(1100), text: "Synced line".into() }]);
    }

    #[tokio::test]
    async fn lyrics_falls_back_to_legacy_when_structured_is_empty() {
        // getLyricsBySongId succeeds but returns nothing (e.g. an older
        // Navidrome, or a song with no synced lyrics available) — the
        // fallback endpoint must still be tried rather than giving up. The
        // mocked body has no `lyricsList` at all, so the first (structured)
        // request naturally parses as empty and the second (legacy) request
        // is what actually supplies the line.
        let url = respond_always(r#"{"subsonic-response":{"status":"ok","lyrics":{"value":"Plain unsynced lyrics"}}}"#).await;

        let client = Client::new(test_creds(url));
        let lines = client.lyrics("s1", "Some Artist", "Some Title").await;
        assert_eq!(lines, vec![LyricLine { start_ms: None, text: "Plain unsynced lyrics".into() }]);
    }

    #[tokio::test]
    async fn lyrics_is_empty_not_an_error_when_nothing_is_found() {
        let url = respond_always(r#"{"subsonic-response":{"status":"failed","error":{"code":70,"message":"not found"}}}"#).await;

        let client = Client::new(test_creds(url));
        let lines = client.lyrics("s1", "Some Artist", "Some Title").await;
        assert!(lines.is_empty());
    }
}
