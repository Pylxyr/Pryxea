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

`LOUDNESS_MODE=dynamic` no longer exists (it cost ~120 MB of RAM per use);
`static` and `off` remain.

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
| 2 | Audio engine: decode, resample, loudness gain + limiter, in-process Opus/Ogg encode, gapless handoff | next |
| 3 | yt-dlp helper (on-demand, idle exit), resolve cache, radio mix | |
| 4 | Twitch: OAuth, EventSub chat, send-message, the five commands wired to the queue | |
| 5 | Queue, player loop, queue persistence, `/thumb-proxy`, `/settings` page | |
| 6 | Tray icon, packaging, CI | |

## Footprint (step 1, Linux x86-64, measured)

| | |
|---|---|
| Release binary | 0.76 MB |
| RSS at idle | 2.5 MB, 1 thread |
| RSS with 20 WebSockets + 5 stream listeners | 5.4 MB |

## License

Public domain (Unlicense), like the project it descends from.
