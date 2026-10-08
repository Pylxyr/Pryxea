//! The whole bot: the real `pryxea` binary against a fake Twitch (OAuth, Helix and
//! EventSub), a fake yt-dlp, and a local media server. Chat commands go in; chat
//! replies and a decodable Opus stream come out.

#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::twitch::{self, preauthorize};
use common::{Server, spawn};
use futures_util::SinkExt;
use pryxea::audio::opus::Decoder;
use pryxea::audio::ogg;
use pryxea::hub::page_len;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

struct Guard(Child, PathBuf);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        if std::thread::panicking() {
            eprintln!("---- pryxea log ----\n{}", std::fs::read_to_string(&self.1).unwrap_or_default());
        }
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn chat_event(id: &str, user: &str, name: &str, text: &str, badge: &str) -> Message {
    Message::text(
        json!({"metadata": {"message_id": format!("n-{id}"), "message_type": "notification"},
               "payload": {"subscription": {"type": "channel.chat.message"},
                           "event": {"broadcaster_user_id": "100", "chatter_user_id": user, "chatter_user_login": name.to_lowercase(), "chatter_user_name": name,
                                     "message_id": id, "message": {"text": text}, "badges": [{"set_id": badge}]}}})
        .to_string(),
    )
}

async fn http_get(port: u16, path: &str) -> (String, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    (head.to_string(), body.to_string())
}

/// Removes HTTP/1.1 chunk framing from a body.
fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(eol) = body.windows(2).position(|w| w == b"\r\n") {
        let Ok(size) = usize::from_str_radix(std::str::from_utf8(&body[..eol]).unwrap_or("").trim(), 16) else { break };
        let start = eol + 2;
        if size == 0 || body.len() < start + size {
            break; // end, or the last chunk was cut off by closing the connection
        }
        out.extend_from_slice(&body[start..start + size]);
        body = &body[(start + size + 2).min(body.len())..];
    }
    out
}

fn fake_ytdlp(dir: &std::path::Path, media_port: u16) -> PathBuf {
    let video = format!(
        r#"{{"id":"aaaaaaaaaaa","title":"E2E Song","uploader":"Artist","duration":2,"webpage_url":"https://www.youtube.com/watch?v=aaaaaaaaaaa","url":"http://127.0.0.1:{media_port}/media/tone.webm","ext":"webm","acodec":"opus","vcodec":"none","protocol":"https","http_headers":{{"User-Agent":"e2e"}}}}"#
    );
    let script = format!(
        "#!/bin/sh\ncase \"$*\" in\n  *--flat-playlist*) echo '{{\"entries\":[]}}' ;;\n  *ytsearch1:*) echo '{{\"_type\":\"playlist\",\"entries\":[{video}]}}' ;;\n  *) echo '{video}' ;;\nesac\n"
    );
    let path = dir.join("fake-yt-dlp");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// (resident MB, threads) of a process, from /proc.
fn footprint(pid: u32) -> (f64, u32) {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let field = |name: &str| status.lines().find(|l| l.starts_with(name)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    (field("VmRSS:") / 1024.0, field("Threads:") as u32)
}

async fn wait_for<T>(what: &str, secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn chat_commands_drive_the_whole_station_end_to_end() {
    let site: Server = spawn(false); // plays Twitch's OAuth + Helix endpoints, and serves the media
    let fake = twitch::install(&site);
    preauthorize(&fake, "42", "botlogin", "bot-access", "bot-refresh");

    let home = std::env::temp_dir().join(format!("pryxea-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join("data")).unwrap();
    std::fs::write(home.join("data/twitch_tokens.json"), r#"{"42": {"token": "bot-access", "refresh": "bot-refresh", "login": "botlogin"}}"#).unwrap();
    let ytdlp = fake_ytdlp(&home, site.addr.port());

    // Fake EventSub: welcome, wait to be subscribed, then wait for the go signal and play out a chat script.
    let go = Arc::new(tokio::sync::Notify::new());
    let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_port = ws_listener.local_addr().unwrap().port();
    let (go2, fake2) = (go.clone(), fake.clone());
    tokio::spawn(async move {
        while let Ok((tcp, _)) = ws_listener.accept().await {
            let (go, fake) = (go2.clone(), fake2.clone());
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                ws.send(Message::text(json!({"metadata": {"message_id": "w", "message_type": "session_welcome"}, "payload": {"session": {"id": "e2e-session", "keepalive_timeout_seconds": 30}}}).to_string())).await.unwrap();
                go.notified().await;
                let _ = fake.lock().unwrap().subs.len();
                ws.send(chat_event("m-sr", "7", "Viewer", "!sr some song", "subscriber")).await.unwrap();
                tokio::time::sleep(Duration::from_millis(1_000)).await;
                ws.send(chat_event("m-np", "8", "Other", "!nowplaying", "subscriber")).await.unwrap();
                tokio::time::sleep(Duration::from_millis(250)).await;
                ws.send(chat_event("m-sq", "8", "Other", "!sq", "subscriber")).await.unwrap();
                tokio::time::sleep(Duration::from_millis(400)).await;
                ws.send(chat_event("m-skip", "9", "Mod", "!skip", "moderator")).await.unwrap();
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
        }
    });

    let (main_port, redirect_port) = (free_port(), free_port());
    let log = home.join("child.log");
    let child = Command::new(env!("CARGO_BIN_EXE_pryxea"))
        .env_clear()
        .env("PRYXEA_HOME", &home)
        .env("TWITCH_CLIENT_ID", "cid")
        .env("TWITCH_CLIENT_SECRET", "sec")
        .env("TWITCH_BOT_ID", "42")
        .env("TWITCH_OWNER_ID", "100")
        .env("TWITCH_NOWPLAYING_PORT", main_port.to_string())
        .env("TWITCH_REDIRECT_URI", format!("http://localhost:{redirect_port}/oauth/callback"))
        .env("TWITCH_ID_URL", site.url_ip("/id"))
        .env("TWITCH_API_URL", site.url_ip("/api"))
        .env("TWITCH_EVENTSUB_URL", format!("ws://127.0.0.1:{ws_port}/ws"))
        .env("YTDLP_PATH", &ytdlp)
        .env("YTDLP_JS_RUNTIME_PATH", "/bin/true")
        .env("LOG_LEVEL", "DEBUG")
        .env("LOG_TO_FILE", "false")
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .expect("start the pryxea binary");
    let _guard = Guard(child, log);

    // It comes up and finds its chat link.
    wait_for("the server", 15, || std::net::TcpStream::connect(("127.0.0.1", main_port)).ok()).await;
    wait_for("the chat subscription", 15, || {
        let st = fake.lock().unwrap();
        (!st.subs.is_empty()).then_some(())
    })
    .await;
    let pid = _guard.0.id();
    let (idle_mb, idle_threads) = footprint(pid);
    eprintln!("idle, chat connected: {idle_mb:.1} MB resident, {idle_threads} threads");
    assert!(idle_mb < 30.0, "idle memory regressed: {idle_mb:.1} MB");
    let (head, page) = http_get(main_port, "/setup").await;
    assert!(head.starts_with("HTTP/1.1 200") && page.contains("authorized as botlogin"), "{head}\n{page}");
    assert_eq!(fake.lock().unwrap().subs[0].0["condition"]["broadcaster_user_id"], "100");

    // A listener on the stream, as OBS would be.
    let mut listener = TcpStream::connect(("127.0.0.1", main_port)).await.unwrap();
    listener.write_all(format!("GET /stream.opus HTTP/1.1\r\nHost: 127.0.0.1:{main_port}\r\n\r\n").as_bytes()).await.unwrap();
    let collected = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let sink = collected.clone();
    let reader = tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        while let Ok(n) = listener.read(&mut buf).await {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });

    go.notify_waiters();
    go.notify_one();

    // The bot answers in chat, in order, as replies to the right messages.
    let sent = wait_for("the bot's replies", 25, || {
        let st = fake.lock().unwrap();
        (st.sent.len() >= 6).then(|| st.sent.iter().map(|(b, _)| (b["message"].as_str().unwrap().to_string(), b.get("reply_parent_message_id").and_then(|v| v.as_str()).map(str::to_string))).collect::<Vec<_>>())
    })
    .await;
    let texts: Vec<&str> = sent.iter().map(|(t, _)| t.as_str()).collect();
    assert!(texts[0].starts_with("Looking up \"some song\""), "{texts:?}");
    assert!(texts[1].starts_with("Queued: E2E Song (#1 in queue)"), "{texts:?}");
    assert!(texts[2].starts_with("Viewer's song request is Now Playing: E2E Song"), "{texts:?}");
    assert!(texts[3].starts_with("Now playing: E2E Song \u{2014} requested by Viewer ("), "{texts:?}");
    assert!(texts[4].starts_with("Queue is empty."), "{texts:?}");
    assert!(texts[5].starts_with("Skipped."), "{texts:?}");
    assert_eq!(sent[0].1.as_deref(), Some("m-sr"));
    assert_eq!(sent[2].1, None, "the now-playing announcement is not a reply");
    assert_eq!((sent[3].1.as_deref(), sent[4].1.as_deref(), sent[5].1.as_deref()), (Some("m-np"), Some("m-sq"), Some("m-skip")));

    let (play_mb, play_threads) = footprint(pid);
    eprintln!("after a song was requested, played and skipped (stream listener attached): {play_mb:.1} MB resident, {play_threads} threads");
    assert!(play_mb < 45.0, "playback memory regressed: {play_mb:.1} MB");

    // After the skip the station is idle again.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, body) = http_get(main_port, "/nowplaying.json").await;
        if body.contains("\"playing\":false") {
            break;
        }
        assert!(Instant::now() < deadline, "the station never went idle: {body}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // What a listener received is a real Opus stream containing the song's tone.
    tokio::time::sleep(Duration::from_millis(300)).await;
    reader.abort();
    let raw = collected.lock().unwrap().clone();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("response head");
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    assert!(head.contains("200 ok") && head.contains("audio/ogg"), "{head}");
    let body = dechunk(&raw[split + 4..]);
    let mut at = 0;
    while let Ok(Some(n)) = page_len(&body[at..]) {
        at += n;
    }
    let packets = ogg::packets(&body[..at]);
    assert_eq!(&packets[0].1[..8], b"OpusHead");
    let mut dec = Decoder::new(0).unwrap();
    let mut pcm = Vec::new();
    for (_, p) in &packets[2..] {
        dec.decode(p, &mut pcm).unwrap();
    }
    let frames: Vec<&[f32]> = pcm.chunks_exact(2 * 4_800).collect(); // 100 ms windows
    let loud = frames.iter().filter(|w| (w.iter().step_by(2).map(|s| f64::from(*s).powi(2)).sum::<f64>() / 4_800.0).sqrt() > 0.25).count();
    assert!(loud >= 8, "expected the 440 Hz tone for about a second, found {loud} loud 100 ms windows of {}", frames.len());
    let best = frames.iter().find(|w| (w.iter().step_by(2).map(|s| f64::from(*s).powi(2)).sum::<f64>() / 4_800.0).sqrt() > 0.25).unwrap();
    let (mut re, mut im) = (0.0, 0.0);
    for (i, f) in best.chunks_exact(2).enumerate() {
        let a = 2.0 * std::f64::consts::PI * 440.0 * i as f64 / 48_000.0;
        re += f64::from(f[0]) * a.cos();
        im += f64::from(f[0]) * a.sin();
    }
    let amp = 2.0 * (re * re + im * im).sqrt() / 4_800.0;
    assert!((amp - 0.5).abs() < 0.08, "left channel should carry a 0.5-amplitude 440 Hz tone, got {amp}");
}
