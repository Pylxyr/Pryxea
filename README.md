# Pryxea

A small, native Twitch song-request radio. Chat requests songs, Pryxea plays
them as **one Opus stream** for OBS and serves a now-playing overlay.

It is a ground-up Rust rewrite of [Twitch-Radio](https://github.com/Pylxyr/Twitch-Radio)
with one goal: **minimal binary size and memory**. No Electron, no bundled
Python, no ffmpeg processes.

## Chat commands

| Command | Who | What |
|---|---|---|
| `!sr <song or URL>` | anyone | request a song |
| `!skip` | mods, or the person who requested the current song | skip the current song |
| `!nowplaying` | anyone | what is playing and who asked |
| `!sq` | anyone | the song queue |
| `!radio [on\|off]` | anyone to read, mods to change | autoplay a related song when the queue runs dry |

Nothing else is recognised: no aliases, no pause/resume, no vote-skip, no
remove/position, no block list.

## Settings page

<http://127.0.0.1:8098/settings> edits the request limits (per-chatter pending,
cooldown, queue cap, longest song) and the radio autoplay switch, live, with no
restart. It also shows what is playing and queued. The same values live in
`data/tunables.json` and `data/toggles.json`, and `!radio on|off` (mods) flips the
switch from chat.

Saving only works from the page itself: a form submitted by another website is
refused, and a rejected form changes nothing.

## OBS setup

- Media Source: `http://127.0.0.1:8098/stream.opus`
- Browser Source: `http://127.0.0.1:8098/overlay`

The server listens on loopback only and rejects requests whose `Host` header
isn't `127.0.0.1`, `localhost` or `[::1]` on one of its ports (DNS rebinding).

## Configuration

`.env` in the home folder (`%APPDATA%\Pryxea`, `~/.local/share/pryxea`, or
`$PRYXEA_HOME`). A first run writes a commented template. Variable names match
Twitch-Radio, so an existing `.env` carries over. Runtime limits live in
`data/tunables.json` and `data/toggles.json`.

There is no loudness normalization: every song plays at its own volume, and
`LOUDNESS_MODE` is ignored (with a warning) if an old `.env` still sets it.

## Releases

Pushing a tag like `v0.2.0` builds five files (Windows x86-64, Linux x86-64 and
ARM64, macOS ARM64 and Intel), runs the tests on each, and publishes them with a
`SHA256SUMS` file (`.github/workflows/release.yml`). `ci.yml` runs the tests on
Linux, Windows and macOS for every push, plus a build on the oldest supported Rust.
The Windows build links the C runtime statically, so no Visual C++ redistributable is needed.

## Build

```sh
cargo build --release     # target/release/pryxea
cargo test
```

Requires Rust 1.85+.

## Quick start

1. Register an application at <https://dev.twitch.tv/console/apps>. Add
   `http://localhost:4343/oauth/callback` as an OAuth Redirect URL, and note the
   client ID and secret.
2. Download the file for your system from the Releases page and run it
   (`pryxea-windows-x86_64.exe`, `pryxea-linux-x86_64`, `pryxea-macos-aarch64`, ...;
   on Linux and macOS make it executable first: `chmod +x pryxea-*`). It creates
   its home folder with a commented `.env` (`%APPDATA%\Pryxea`,
   `~/.local/share/pryxea`, or `$PRYXEA_HOME`) and opens the setup page in your
   browser.
3. Fill in the client ID and secret, plus `TWITCH_BOT_ID` (the bot account's
   numeric user ID) and `TWITCH_OWNER_ID` (the channel's), then run it again.
4. On the setup page (<http://127.0.0.1:8098/setup>) authorize the bot account.
   Authorize the broadcaster account too unless the bot is a moderator of the
   channel.
5. In OBS add a Media Source (`http://127.0.0.1:8098/stream.opus`) and a Browser
   Source (`http://127.0.0.1:8098/overlay`).

yt-dlp and a small JavaScript runtime are downloaded automatically the first time
a song is requested. Existing `.env`, token, queue and settings files from the
Python Twitch-Radio are picked up as they are.

**No tray icon.** Pryxea is a single small program that idles at a few MB, so it
has no tray icon. It runs in its console window; close that window, press
Ctrl+C, or use **Quit Pryxea** on the setup or settings page to stop it.

**Updates.** Once a day it checks the Releases page. If a newer version exists the
settings page shows an **Update now** button; the download is verified against the
release's `SHA256SUMS` and swapped in, and nothing restarts until you do.
`CHECK_FOR_UPDATES=false` turns the check off, and `PRYXEA_UPDATE_REPO=owner/name`
points it at a different repository.

## Status

| Step | Piece | State |
|---|---|---|
| 1 | Config, `.env`, JSON stores, five-command parser, Ogg stream hub, HTTP + WebSocket server, overlay | done |
| 2 | Audio engine: decode (Opus, AAC-LC), resample, Opus/Ogg encode, gapless handoff | done |
| 3 | HTTPS client, seekable range source, yt-dlp lookups, tool installer/updater, radio mix | done |
| 4 | Twitch (OAuth, chat in and out), queue and player, radio autoplay, setup page, thumbnail relay, `main` wiring | done |
| 5 | `/settings` page for the request limits and the radio switch | done |
| 6 | Release packaging and CI, update checks, first-run browser opening, Quit button (no tray icon, by design) | done |

## How a request flows

`!sr` is checked against the limits (per-chatter pending, cooldown, queue cap,
duplicates, length), looked up, and queued ahead of any radio filler. The player
resolves the next song about 20 s before the current one ends and opens its
stream 3 s before the end, so songs join without a gap. When the queue is empty
and radio is on, YouTube's Mix supplies a related song. `!skip` works for mods
and for whoever requested the current song. The queue survives a restart.

## Audio

One thread per *playing* track decodes into a small queue (about 1 s ahead);
one engine task mixes, encodes and publishes, five 20 ms Opus packets per
Ogg page. Nothing is encoded while nobody is connected to `/stream.opus`, and
with no track and no listener the engine sleeps entirely.

- **Formats:** Opus (WebM or MP4) and AAC-LC (MP4, including YouTube's
  fragmented MP4), mono or stereo, any common sample rate. Everything else is
  rejected with a clear error (HE-AAC, surround, ...).
- **Volume:** none touched. Songs play at their own level.
- **Gapless:** the next track rolls in mid-block, with no silence added.
- **Mono** sources are duplicated to both channels at full level.
- **Known small differences from the old ffmpeg pipeline:** AAC tracks keep
  their ~23 ms encoder priming, and Opus tracks keep up to ~14 ms of encoder
  padding at the end.

## Lookups and network

- **yt-dlp runs only while a song is being looked up**, as a child process with
  a timeout (and its whole process tree is killed with it), so it costs no
  memory in between. A fast first attempt (`visionos` client, QuickJS) falls back
  to yt-dlp's default clients. Results are cached (`YTDLP_CACHE_TTL_SECONDS`),
  identical lookups in flight share one process, and `YTDLP_CONCURRENCY` caps
  parallel ones.
- Only YouTube links and searches are accepted. Format selection prefers Opus,
  then AAC-LC; live streams, HLS/DASH manifests and HE-AAC are refused with a
  reason.
- **Tools are downloaded on first use** into `bin/` (not bundled): the official
  yt-dlp build for your platform, checked against the release's `SHA2-256SUMS`,
  refreshed at most once a day (a failed update keeps the working copy), and a
  pinned, hash-checked QuickJS-ng (~2 MB) as yt-dlp's JavaScript runtime. Set
  `YTDLP_PATH` to manage yt-dlp yourself.
- **HTTPS** is rustls with the operating system's trust store, so antivirus or
  corporate HTTPS inspection keeps working. Media is read in 4 MiB `Range`
  requests with reconnect and resume, so a dropped connection never restarts a song.

## Footprint (Linux x86-64, measured)

| | |
|---|---|
| The complete bot, release binary | **3.1 MB** |
| Resident memory, idle, chat connected | **5.1 MB**, 2 threads |
| Resident memory, a song playing with OBS listening | ~6 MB (about 11 MB peak while decoding AAC music) |
| CPU while streaming | ~1.2 % of one core (Opus), ~1.4 % (AAC + resample) |
| yt-dlp + QuickJS on disk (downloaded, not in the binary) | ~20 MB Windows, ~43 MB Linux, ~39 MB macOS |

For comparison, Twitch-Radio (Electron, a Python core and ffmpeg) installs at
roughly 500 MB and needs hundreds of MB of RAM while playing.

Build requirements: Rust 1.85+ and `cmake` (libopus is built from the bundled source).

## Tools

```sh
cargo run --example decode_dump -- song.webm out.raw         # raw 48 kHz stereo f32
cargo run --release --example stream_dump -- out.ogg a.webm b.m4a   # the stream a listener gets
cargo run --release --example lookup -- "song name or YouTube URL"  # installs yt-dlp, resolves, streams 10 s, asks for the radio mix
```

`cargo test` runs everything that works offline. Checks against the real
internet are opt-in: `cargo test --test internet -- --ignored`, and
`PRYXEA_REAL_YTDLP=/path/to/yt-dlp cargo test --test ytdlp real_ -- --ignored`.

## License

Public domain (Unlicense), like the project it descends from.
