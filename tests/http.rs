//! End-to-end checks of the local server over real sockets.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::StreamExt;
use pryxea::http::{self, Ctx};
use pryxea::hub::StreamHub;
use pryxea::state::{NowPlaying, PlayerState, QueueItem, Shared};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

struct Server {
    port: u16,
    shared: Arc<Shared>,
    hub: Arc<StreamHub>,
}

async fn start() -> Server {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let shared = Arc::new(Shared::new());
    let hub = StreamHub::new();
    let ctx = Arc::new(Ctx { shared: shared.clone(), hub: hub.clone(), port });
    tokio::spawn(http::serve(listener, ctx));
    Server { port, shared, hub }
}

/// Sends one request with `Connection: close` and returns (head, body).
async fn get(port: u16, path: &str, host: &str) -> (String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("response head");
    (String::from_utf8_lossy(&raw[..split]).to_string(), raw[split + 4..].to_vec())
}

fn page(granule: i64, seq: u32, flags: u8, body: &[u8]) -> Vec<u8> {
    let mut out = b"OggS".to_vec();
    out.extend([0, flags]);
    out.extend(granule.to_le_bytes());
    out.extend(1u32.to_le_bytes());
    out.extend(seq.to_le_bytes());
    out.extend([0, 0, 0, 0, 1, body.len() as u8]);
    out.extend(body);
    out
}

fn sample_track() -> NowPlaying {
    NowPlaying {
        title: "Song".into(),
        uploader: "Artist".into(),
        thumbnail_url: Some("https://img.test/a.jpg".into()),
        requester_name: "Ann".into(),
        webpage_url: "https://www.youtube.com/watch?v=abc".into(),
        started_at: Instant::now(),
        duration_secs: 180,
    }
}

#[tokio::test]
async fn healthz_nowplaying_and_overlay_are_served() {
    let srv = start().await;
    let host = format!("127.0.0.1:{}", srv.port);

    let (head, body) = get(srv.port, "/healthz", &host).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(j["player_state"], "idle");
    assert_eq!(j["queue_size"], 0);
    assert_eq!(j["resolves_last_hour"]["success"], 0);

    srv.shared.set_player_state(PlayerState::Playing);
    srv.shared.set_now_playing(Some(sample_track()));
    srv.shared.set_queue(vec![QueueItem { title: "Next".into(), requester_name: "Bo".into() }]);
    let (head, body) = get(srv.port, "/nowplaying.json", &host).await;
    assert!(head.to_ascii_lowercase().contains("content-type: application/json"), "{head}");
    let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!((j["playing"].clone(), j["title"].clone()), (true.into(), "Song".into()));
    assert_eq!(j["queue"][0]["requester_name"], "Bo");

    let (head, body) = get(srv.port, "/overlay", &host).await;
    assert!(head.to_ascii_lowercase().contains("text/html"), "{head}");
    assert!(String::from_utf8_lossy(&body).contains("/ws/nowplaying"));

    let (head, _) = get(srv.port, "/nope", &host).await;
    assert!(head.starts_with("HTTP/1.1 404"), "{head}");
}

#[tokio::test]
async fn foreign_host_headers_get_421_on_every_route() {
    let srv = start().await;
    for path in ["/healthz", "/nowplaying.json", "/overlay", "/stream.opus", "/ws/nowplaying"] {
        let (head, _) = get(srv.port, path, "evil.example").await;
        assert!(head.starts_with("HTTP/1.1 421"), "{path}: {head}");
    }
    assert_eq!(srv.hub.listener_count(), 0, "a rejected request must not subscribe");
}

#[tokio::test]
async fn stream_replays_the_header_then_live_pages_and_frees_the_listener_slot() {
    let srv = start().await;
    let head_pages = [page(0, 0, 2, b"OpusHead"), page(0, 1, 0, b"OpusTags")].concat();
    let audio1 = page(960, 2, 0, b"first-audio");
    let audio2 = page(1920, 3, 0, b"second-audio");
    srv.hub.begin_session();
    srv.hub.publish(Bytes::from([head_pages.clone(), audio1.clone()].concat()));

    let mut s = TcpStream::connect(("127.0.0.1", srv.port)).await.unwrap();
    let host = format!("127.0.0.1:{}", srv.port);
    s.write_all(format!("GET /stream.opus HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes()).await.unwrap();

    // Wait until the server has subscribed this client, then publish live audio.
    let mut listeners = srv.hub.listeners();
    tokio::time::timeout(Duration::from_secs(5), listeners.wait_for(|n| *n == 1)).await.unwrap().unwrap();
    srv.hub.publish(Bytes::from(audio2.clone()));

    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let want = head_pages.len() + audio2.len();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text = String::from_utf8_lossy(&raw);
        if text.contains("second-audio") || Instant::now() > deadline {
            break;
        }
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf)).await.unwrap().unwrap();
        assert!(n > 0, "stream closed early");
        raw.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8_lossy(&raw).to_string();
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(text.to_ascii_lowercase().contains("content-type: audio/ogg"));
    assert!(text.to_ascii_lowercase().contains("transfer-encoding: chunked"));
    // Header pages come first, then only the audio published after joining.
    let at = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("missing {needle}"));
    assert!(at("OpusHead") < at("OpusTags") && at("OpusTags") < at("second-audio"));
    assert!(!text.contains("first-audio"), "audio from before joining must not be replayed");
    assert!(want > 0);

    drop(s);
    tokio::time::timeout(Duration::from_secs(5), listeners.wait_for(|n| *n == 0)).await.expect("listener slot freed").unwrap();
}

#[tokio::test]
async fn websocket_sends_a_snapshot_then_one_message_per_change() {
    let srv = start().await;
    let url = format!("ws://127.0.0.1:{}/ws/nowplaying", srv.port);
    let tcp = TcpStream::connect(("127.0.0.1", srv.port)).await.unwrap();
    let (mut ws, resp) = tokio_tungstenite::client_async(url, tcp).await.unwrap();
    assert_eq!(resp.status(), 101);

    let next_json = |msg: Option<Result<Message, _>>| -> serde_json::Value {
        match msg.expect("socket open").expect("no error") {
            Message::Text(t) => serde_json::from_str(t.as_str()).unwrap(),
            other => panic!("unexpected frame {other:?}"),
        }
    };
    let first = next_json(tokio::time::timeout(Duration::from_secs(5), ws.next()).await.unwrap());
    assert_eq!(first["playing"], false);

    srv.shared.set_player_state(PlayerState::Playing);
    srv.shared.set_now_playing(Some(sample_track()));
    // Changes made back to back may coalesce, but the last state must arrive.
    let mut last = serde_json::Value::Null;
    for _ in 0..3 {
        let Ok(Some(msg)) = tokio::time::timeout(Duration::from_millis(500), ws.next()).await else { break };
        last = next_json(Some(msg));
    }
    assert_eq!(last["title"], "Song");
    assert_eq!(last["state"], "playing");
}

#[tokio::test]
async fn plain_get_to_the_websocket_path_is_rejected_with_400() {
    let srv = start().await;
    let host = format!("127.0.0.1:{}", srv.port);
    let (head, _) = get(srv.port, "/ws/nowplaying", &host).await;
    assert!(head.starts_with("HTTP/1.1 400"), "{head}");
}

#[tokio::test]
async fn post_is_405_and_head_stream_does_not_subscribe() {
    let srv = start().await;
    let host = format!("127.0.0.1:{}", srv.port);
    let mut s = TcpStream::connect(("127.0.0.1", srv.port)).await.unwrap();
    s.write_all(format!("POST /healthz HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).await.unwrap();
    assert!(raw.starts_with("HTTP/1.1 405"), "{raw}");

    let mut s = TcpStream::connect(("127.0.0.1", srv.port)).await.unwrap();
    s.write_all(format!("HEAD /stream.opus HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).await.unwrap();
    assert!(raw.starts_with("HTTP/1.1 200") && raw.to_ascii_lowercase().contains("audio/ogg"), "{raw}");
    assert_eq!(srv.hub.listener_count(), 0);
}
