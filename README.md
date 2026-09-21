# maraetai-tui (macOS)

A macOS port of [maraetai-tui](https://github.com/unclefroob/maraetai-tui) — a
terminal client for a Navidrome library via
[`maraetai-service`](https://github.com/unclefroob/maraetai-service). Same
feature set as the Linux original, sharing its full history; the one thing
that's genuinely different is how the daemon and TUI talk to each other,
since macOS has no D-Bus.

## Why two binaries

Playback needs to survive the TUI's terminal closing, so this is split like
`mpd`/`ncmpcpp`:

- **`maraetaid`** — the daemon. Owns the audio output device, the decode
  pipeline, and the **queue**.
- **`maraetai`** — the TUI. A thin client: talks to `maraetaid` for
  playback/queue control, and talks directly to `maraetai-service`'s
  Subsonic API for browsing/search/playlists, the same way every other
  maraetai client does. Also carries the CLI (`login`, `daemon status`/
  `stop`, `play <song-id>`).

## macOS vs. the Linux original

- **Control channel.** Linux talks to the daemon over D-Bus, which also
  gets it MPRIS media-key/lock-screen integration for free. macOS has no
  D-Bus session bus by default, so this fork replaces that whole channel
  with a small newline-delimited-JSON protocol over a Unix domain socket
  (`$TMPDIR/maraetai-<uid>/control.sock`) — no setup step, nothing to
  install or configure. The wire-shape conversions live in one shared
  place (`daemon/src/playback.rs`) so the two transports can't drift in
  behavior, only in format.
- **No MPRIS-equivalent media-key/lock-screen integration.** MPRIS is a
  D-Bus interface; nothing on macOS speaks it, so that half of the Linux
  build's OS integration doesn't carry over here. A native
  `MPNowPlayingInfoCenter` integration would be a separate, unbuilt
  feature — everything else (browsing, playback, queue, playlists) is
  identical.
- **Credential storage.** Passwords go into the macOS Keychain (`keyring`
  crate's `apple-native` backend) instead of Secret Service/KWallet.

## Not perpetually running by design

- `maraetai` auto-spawns `maraetaid` on first use if it isn't already
  running (checked via a real round-trip call, not just process/socket
  presence).
- The daemon shuts itself down automatically after being idle — nothing
  playing **and** no client connected — for a configurable timeout
  (default 20 minutes; set `idle_timeout_secs` in `config.toml` to
  change it). An open TUI counts as "connected" even while paused/
  stopped, so it won't get shut out from under you just for sitting idle
  on-screen — only a TUI that's actually been closed (or a daemon nobody
  ever opened one for) triggers the timeout.
- `maraetai daemon stop` (or pressing `Q` in the TUI) asks it to quit
  immediately.
- A locked pidfile under `$TMPDIR/maraetai-<uid>/` (macOS has no
  `$XDG_RUNTIME_DIR`) prevents two daemons starting at once.

## Streaming, not buffer-then-play

`rodio`'s decoder requires `Seek`, but an HTTP response body only supports
sequential reads. `daemon/src/range_reader.rs` bridges that with real HTTP
Range requests — a seek drops the current connection and lazily reissues a
ranged GET for the new position on the next read, so no seek downloads more
than the bytes actually needed, and playback can start before the whole file
arrives. Degrades gracefully (read-and-discard) if the server ignores
`Range` and returns the whole resource from byte 0 instead.

## Output device & ReplayGain

- `maraetaid --list-devices` prints available audio output devices (safe
  to run even while a daemon is already active). Put the name you want in
  `output_device` in `config.toml`; matched by exact name or
  case-insensitive substring, falling back to the system default on any
  mismatch.
- ReplayGain (track gain preferred over album gain) is applied
  automatically when a song has tags for it — attenuate-only, never
  amplified past a track's original level, since there's no peak limiter
  here to catch clipping.

## Credentials

Server URL + username live in `config.toml`, resolved automatically via the
`directories` crate (`~/Library/Application Support/maraetai/config.toml` on
macOS). The password itself is never written there — it's stored in the
Keychain, looked up by username at connect time.

## Setup

### Precompiled release (recommended)

Download the archive for your Mac from the
[GitHub Releases page](https://github.com/unclefroob/maraetai-tui-mac/releases):

```sh
# Apple Silicon (M1/M2/M3/M4)
curl -LO https://github.com/unclefroob/maraetai-tui-mac/releases/latest/download/maraetai-tui-macos-arm64.tar.gz
tar -xzf maraetai-tui-macos-arm64.tar.gz
install -m 755 maraetai-tui-macos-arm64/maraetai ~/.local/bin/
install -m 755 maraetai-tui-macos-arm64/maraetaid ~/.local/bin/
```

Use `maraetai-tui-macos-x64.tar.gz` instead on an Intel Mac. Ensure
`~/.local/bin` is on your `PATH`, then run `maraetai login` followed by
`maraetai`.

### From source

```sh
xcode-select --install        # once, if you don't already have it — needed
                               # to build native deps (Keychain bindings, TLS)
cargo install --path daemon --locked
cargo install --path tui --locked
maraetai login                 # prompts for server URL, username, password
maraetai                       # launches the TUI, auto-spawning maraetaid
```

## Updating

```sh
git pull
cargo install --path daemon --locked
cargo install --path tui --locked
```

Then restart the daemon (`Q` in the TUI, or `maraetai daemon stop`) so it
actually picks up the new build — reinstalling the binary doesn't affect a
daemon already running in memory.

## Keybindings

Full reference is always available in-app via `?`. Highlights:

**Navigation** — `1`-`8` / `[`/`]` switch tabs, `Up`/`k` `Down`/`j` move,
`Enter` open/play, `Esc`/`Backspace` back, `/` filter the current list.

**Playback** — `space` play/pause, `n`/`p` next/previous, `Left`/`Right`
seek, `-`/`+` volume, `r` cycle repeat, `x` toggle shuffle, `s` stop.

**Queue** — `a` add to queue, `A` play next (right after the current
track), `d` remove selected / `D` clear (Queue tab), `J`/`K` reorder,
`S` save the queue as a playlist.

**Playlists** — `N` new, `c` rename, `d` delete (Playlists tab), `P` add
the selected song to a playlist, `d` remove a song from one you're
viewing.

**Selection & info** — `v` mark/unmark songs for a bulk action (`a`/`A`/
`f`/`P`/`d` then act on everything marked), `f` toggle favorite, `t`
toggle Albums sort (A-Z / newest), `e` expand a Home section, `i` artist
bio/similar artists or album notes, `l` toggle the lyrics panel.

**Other** — `q` quit (daemon keeps running), `Q` quit and stop it.

## Settings tab & updating from within the TUI

The Settings tab (`8`) shows the commit this binary was built from and
lets you check GitHub for a newer one: `c` checks, and if an update is
available, `u` twice (a "press again to confirm" gate, since it's a real
action) reinstalls both binaries with `cargo install --git ... --force` —
this works even if you no longer have a local checkout, unlike `cargo
install --path`, which keeps no link back to any repository once
installed. You'll still need to restart the daemon and the TUI afterward
for the new build to actually take over — reinstalling the binary doesn't
touch an already-running process, same as any other update.

## Out of scope (for now)

- Gapless playback / crossfade — needs a real audio-engine rework
  (pre-buffering the next track onto the same sink) that's risky enough to
  want live-testing each step, deliberately deferred.
- Podcasts / internet radio, offline download/caching.
- Multiple accounts from within the TUI (one at a time, via `maraetai
  login`).
- No in-TUI output-device picker or a toggle to disable ReplayGain —
  `output_device` is `config.toml`-only for now.
- No launchd unit shipped by default — spawn-on-demand + idle-shutdown is
  the default experience, not an always-on service.

See `.autofeature/designs/` for the original Linux-only design this was
built from — the reasoning there still holds for everything except the
control-channel transport, which this fork changes as described above.
