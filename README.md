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

## OBS setup

- Media Source: `http://127.0.0.1:8098/stream.opus`
- Browser Source: `http://127.0.0.1:8098/overlay`

The server listens on loopback only and rejects requests whose `Host` header
isn't `127.0.0.1`, `localhost` or `[::1]` on the configured port (DNS rebinding).

## Configuration

`.env` in the home folder (`%APPDATA%\Pryxea`, `~/.local/share/pryxea`, or
`$PRYXEA_HOME`). A first run writes a commented template. Variable names match
Twitch-Radio, so an existing `.env` carries over. Runtime limits live in
`data/tunables.json` and `data/toggles.json`.

There is no loudness normalization: every song plays at its own volume, and
`LOUDNESS_MODE` is ignored (with a warning) if an old `.env` still sets it.

## Build

```sh
cargo build --release     # target/release/pryxea
cargo test
```

Requires Rust 1.85+.

## Status

| Step | Piece | State |
|---|---|---|
| 1 | Config, `.env`, JSON stores, five-command parser, Ogg stream hub, HTTP + WebSocket server, overlay | done |
| 2 | Audio engine: decode (Opus, AAC-LC), resample, in-process Opus/Ogg encode, gapless handoff | done |
| 3 | HTTPS client + seekable range source, yt-dlp lookups (on demand), tool installer/updater, resolve cache, radio mix | done |
| 4 | Twitch: OAuth, EventSub chat, send-message, the five commands wired to the queue | next |
| 5 | Queue, player loop, queue persistence, `/thumb-proxy`, `/settings` page | |
| 6 | Tray icon, packaging, CI | |

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

## Footprint (steps 1-3, Linux x86-64, measured)

| | |
|---|---|
| `pryxea` binary today (server only) | 0.76 MB |
| Everything but Twitch (HTTPS, resolver, decoder, engine): the `lookup` example | 2.23 MB |
| RSS at idle | 2.5 MB, 1 thread |
| RSS with 20 WebSockets + 5 stream listeners | 5.4 MB |
| Engine streaming 20 s of music, decode + encode, peak RSS | 11 MB |
| CPU doing that | ~1.2 % of one core (Opus), ~1.4 % (AAC + resample) |
| yt-dlp + QuickJS on disk (downloaded, not in the binary) | ~20 MB Windows, ~43 MB Linux, ~39 MB macOS |

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
