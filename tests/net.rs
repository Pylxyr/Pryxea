mod common;

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use common::{BIG_LEN, SMALL_LEN, Server, pattern, spawn};
use pryxea::audio::decode::TrackDecoder;
use pryxea::net::http::{Client, HttpError, Request};
use pryxea::net::source::HttpSource;
use symphonia::core::io::MediaSource;

fn client_for(server: &Server) -> Arc<Client> {
    Arc::new(Client::with_tls(server.client_tls.clone()))
}

fn open(server: &Server, path: &str) -> HttpSource {
    HttpSource::open(client_for(server), &server.url(path), &[]).expect("open").with_fast_retries()
}

// ------------------------------------------------------------------ client

#[test]
fn plain_and_tls_requests_work_and_untrusted_certs_are_refused() {
    for https in [false, true] {
        let s = spawn(https);
        let resp = client_for(&s).send(&Request::get(s.url("/echo-headers"))).unwrap();
        assert_eq!(resp.status, 200, "https={https}");
        let body = String::from_utf8(resp.bytes(10_000).unwrap()).unwrap();
        assert!(body.contains("Host: localhost:"), "{body}");
        assert!(body.contains("User-Agent: Pryxea/"), "{body}");
        assert!(body.contains("Connection: close"));
    }
    // The platform verifier must not accept a self-signed certificate.
    let s = spawn(true);
    match Client::new().send(&Request::get(s.url("/small"))) {
        Err(HttpError::Tls(_) | HttpError::Io(_)) => {}
        Err(other) => panic!("expected a TLS failure, got {other}"),
        Ok(r) => panic!("self-signed certificate was accepted (HTTP {})", r.status),
    }
}

#[test]
fn chunked_until_close_and_post_bodies_decode_correctly() {
    let s = spawn(true);
    let c = client_for(&s);
    assert_eq!(c.send(&Request::get(s.url("/chunked"))).unwrap().bytes(1_000).unwrap(), b"hello chunked world");
    assert_eq!(c.send(&Request::get(s.url("/until-close"))).unwrap().bytes(1_000).unwrap(), b"no length given");
    let echoed = c.send(&Request::post(s.url("/echo-post"), b"a=1&b=2".to_vec()).header("Content-Type", "application/x-www-form-urlencoded")).unwrap();
    assert_eq!(echoed.header("x-method"), Some("POST"));
    assert_eq!(echoed.bytes(100).unwrap(), b"a=1&b=2");
    assert!(matches!(c.send(&Request::get(s.url("/small"))).unwrap().bytes(1_000), Err(HttpError::TooLarge)));
}

#[test]
fn redirects_are_followed_and_credentials_do_not_cross_origins() {
    let s = spawn(false);
    let c = client_for(&s);
    let r = c.send(&Request::get(s.url("/redir")).header("Authorization", "Bearer secret")).unwrap();
    assert_eq!(r.status, 200);
    assert!(r.url.ends_with("/small"), "{}", r.url);
    assert_eq!(r.bytes(SMALL_LEN).unwrap(), pattern(SMALL_LEN));

    // localhost -> 127.0.0.1 is a different origin: Authorization must be stripped.
    let echoed = c.send(&Request::get(s.url("/redir-cross")).header("Authorization", "Bearer secret").header("X-Keep", "yes")).unwrap();
    let body = String::from_utf8(echoed.bytes(10_000).unwrap()).unwrap();
    assert!(body.contains("X-Keep: yes"), "{body}");
    assert!(!body.to_ascii_lowercase().contains("authorization"), "credentials leaked across origins:\n{body}");

    assert!(matches!(c.send(&Request::get(s.url("/redir-loop"))), Err(HttpError::TooManyRedirects)));
}

#[test]
fn bad_input_is_rejected_before_any_connection() {
    let c = Client::new();
    assert!(matches!(c.send(&Request::get("ftp://x/")), Err(HttpError::Invalid(_))));
    assert!(matches!(c.send(&Request::get("http://localhost:1/").header("X", "a\r\nInjected: 1")), Err(HttpError::Invalid(_))));
    assert!(matches!(c.send(&Request::get("http://127.0.0.1:1/")), Err(HttpError::Connect(_))));
}

// ------------------------------------------------------------------ source

#[test]
fn a_big_file_is_read_through_several_range_requests_over_http_and_https() {
    for https in [false, true] {
        let s = spawn(https);
        let mut src = open(&s, "/file");
        assert_eq!(src.byte_len(), Some(BIG_LEN as u64));
        assert!(src.is_seekable());
        let mut all = Vec::new();
        src.read_to_end(&mut all).unwrap();
        assert!(all == pattern(BIG_LEN), "content differs (https={https})");
        // 10 MiB in 4 MiB ranges: bytes 0-, 4194304-, 8388608-.
        let ranges: Vec<String> = s.requests().into_iter().filter(|r| r.starts_with("GET /file")).collect();
        assert_eq!(ranges.len(), 3, "{ranges:?}");
        assert!(ranges[0].ends_with("bytes=0-4194303"), "{ranges:?}");
        assert!(ranges[2].ends_with("bytes=8388608-10485759"), "{ranges:?}");
    }
}

#[test]
fn seeking_reuses_the_connection_for_small_hops_and_reconnects_for_big_ones() {
    let s = spawn(false);
    let data = pattern(BIG_LEN);
    let mut src = open(&s, "/file");
    let mut buf = vec![0u8; 1000];

    src.read_exact(&mut buf).unwrap();
    assert_eq!(buf, data[..1000]);
    // Small hop forward: discarded from the open response, no new request.
    src.seek(SeekFrom::Start(50_000)).unwrap();
    src.read_exact(&mut buf).unwrap();
    assert_eq!(buf, data[50_000..51_000]);
    assert_eq!(s.count("GET /file"), 1);
    // Far jump and a jump backwards each need a new range request.
    assert_eq!(src.seek(SeekFrom::Start(9_000_000)).unwrap(), 9_000_000);
    src.read_exact(&mut buf).unwrap();
    assert_eq!(buf, data[9_000_000..9_001_000]);
    src.seek(SeekFrom::Start(10)).unwrap();
    src.read_exact(&mut buf).unwrap();
    assert_eq!(buf, data[10..1010]);
    assert_eq!(s.count("GET /file"), 3);
    // From the end, and reading past it.
    assert_eq!(src.seek(SeekFrom::End(-5)).unwrap(), BIG_LEN as u64 - 5);
    let mut tail = Vec::new();
    src.read_to_end(&mut tail).unwrap();
    assert_eq!(tail, data[BIG_LEN - 5..]);
    assert_eq!(src.read(&mut buf).unwrap(), 0);
    assert!(src.seek(SeekFrom::Current(-(BIG_LEN as i64) * 2)).is_err());
}

#[test]
fn a_server_that_ignores_range_still_plays_but_cannot_seek() {
    let s = spawn(false);
    let mut src = open(&s, "/norange");
    assert!(!src.is_seekable());
    assert_eq!(src.byte_len(), Some(SMALL_LEN as u64));
    let mut all = Vec::new();
    src.read_to_end(&mut all).unwrap();
    assert_eq!(all, pattern(SMALL_LEN));
    // Even a backwards hop works (by re-requesting and skipping).
    src.seek(SeekFrom::Start(1_000)).unwrap();
    let mut buf = [0u8; 100];
    src.read_exact(&mut buf).unwrap();
    assert_eq!(buf, pattern(SMALL_LEN)[1_000..1_100]);
}

#[test]
fn dropped_connections_are_resumed_from_the_right_offset() {
    let s = spawn(true);
    let mut src = open(&s, "/flaky");
    let mut all = Vec::new();
    src.read_to_end(&mut all).unwrap();
    assert!(all == pattern(BIG_LEN), "data was corrupted or duplicated across reconnects");
    let flaky: Vec<String> = s.requests().into_iter().filter(|r| r.starts_with("GET /flaky")).collect();
    assert!(flaky.len() >= 5, "expected reconnects, got {flaky:?}");
    assert!(flaky[1].contains("bytes=100000-"), "second request must resume after the 100000 bytes already received: {flaky:?}");
}

#[test]
fn transient_statuses_are_retried_but_permanent_ones_are_not() {
    let s = spawn(false);
    let mut src = open(&s, "/transient");
    let mut all = Vec::new();
    src.read_to_end(&mut all).unwrap();
    assert_eq!(all, pattern(SMALL_LEN));
    assert_eq!(s.count("GET /transient"), 2);

    for (path, code) in [("/status/403", 403u16), ("/status/404", 404)] {
        let err = HttpSource::open(client_for(&s), &s.url(path), &[]).err().expect("must fail");
        assert!(matches!(err, HttpError::Status(c) if c == code), "{err}");
        assert_eq!(s.count(&format!("GET {path}")), 1, "{path} must not be retried");
    }
}

#[test]
fn custom_headers_reach_the_media_server() {
    let s = spawn(false);
    let headers = vec![("User-Agent".to_string(), "yt-test/1.0".to_string()), ("Referer".to_string(), "https://example.test/".to_string())];
    let c = client_for(&s);
    let resp = c.send(&Request::get(s.url("/echo-headers")).headers(&headers)).unwrap();
    let body = String::from_utf8(resp.bytes(10_000).unwrap()).unwrap();
    assert!(body.contains("User-Agent: yt-test/1.0") && !body.contains("Pryxea/"), "{body}");
    assert!(body.contains("Referer: https://example.test/"));
}

// ---------------------------------------------------------- with the decoder

fn decode_frames(source: Box<dyn MediaSource>, ext: &str) -> usize {
    let mut dec = TrackDecoder::open(source, Some(ext)).expect("probe");
    let (mut pcm, mut frames) = (Vec::new(), 0);
    while dec.decode_more(&mut pcm).expect("decode") {
        frames += pcm.len() / 2;
        pcm.clear();
    }
    frames + pcm.len() / 2
}

#[test]
fn real_media_decodes_identically_over_http_and_from_a_file() {
    for https in [false, true] {
        let s = spawn(https);
        for (name, ext) in [("tone.webm", "webm"), ("tone44_frag.m4a", "m4a"), ("tone44.m4a", "m4a")] {
            let from_file = decode_frames(Box::new(std::fs::File::open(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()), ext);
            let over_http = decode_frames(Box::new(open(&s, &format!("/media/{name}"))), ext);
            assert_eq!(over_http, from_file, "{name} (https={https})");
            assert!(over_http > 90_000);
        }
    }
}

#[test]
fn a_non_media_url_fails_to_probe_instead_of_hanging() {
    let s = spawn(false);
    assert!(TrackDecoder::open(Box::new(open(&s, "/small")), Some("webm")).is_err());
}

#[tokio::test]
async fn the_engine_plays_a_track_streamed_over_https() {
    use pryxea::audio::engine::{Engine, Event, Outcome};
    use pryxea::hub::StreamHub;
    use std::time::Duration;

    let s = spawn(true);
    let hub = StreamHub::new();
    let (engine, mut events) = Engine::spawn(hub.clone(), 160);
    let source = tokio::task::spawn_blocking({
        let url = s.url("/media/tone.webm");
        let client = client_for(&s);
        move || HttpSource::open(client, &url, &[]).unwrap()
    })
    .await
    .unwrap();
    engine.play(1, Box::new(source), Some("webm"));
    let started = tokio::time::timeout(Duration::from_secs(6), events.recv()).await.expect("event").expect("engine");
    assert_eq!(started, Event::Started(1));
    let ended = tokio::time::timeout(Duration::from_secs(6), events.recv()).await.expect("event").expect("engine");
    assert_eq!(ended, Event::Ended(1, Outcome::Finished));
}
