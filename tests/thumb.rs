mod common;

use std::sync::Arc;

use common::{Resp, spawn};
use pryxea::http::{self, Ctx};
use pryxea::hub::StreamHub;
use pryxea::net::http::Client;
use pryxea::net::url::percent_encode;
use pryxea::state::Shared;
use pryxea::thumb::ThumbProxy;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const JPEG: &[u8] = b"\xff\xd8\xff\xe0 not really a jpeg but close enough";

/// Our server (with a thumbnail proxy allowed to reach the image server) and the image server.
async fn start() -> (u16, common::Server) {
    let images = spawn(false);
    images.on("GET", "/img/ok.jpg", |_| Resp { status: 200, headers: vec![("Content-Type".into(), "image/jpeg".into())], body: JPEG.to_vec() });
    images.on("GET", "/img/page.html", |_| Resp { status: 200, headers: vec![("Content-Type".into(), "text/html".into())], body: b"<script>alert(1)</script>".to_vec() });
    images.on("GET", "/img/missing.jpg", |_| Resp::text(404, "nope"));
    images.on("GET", "/img/big.jpg", |_| Resp { status: 200, headers: vec![("Content-Type".into(), "image/png".into())], body: vec![0u8; 4 * 1024 * 1024] });
    images.on("GET", "/img/redir-ok", |_| Resp { status: 302, headers: vec![("Location".into(), "/img/ok.jpg".into())], body: vec![] });
    images.on("GET", "/img/redir-bad", |_| Resp { status: 302, headers: vec![("Location".into(), "http://evil.example/x.jpg".into())], body: vec![] });
    images.on("GET", "/img/redir-loop", |_| Resp { status: 302, headers: vec![("Location".into(), "/img/redir-loop".into())], body: vec![] });

    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut ctx = Ctx::new(Arc::new(Shared::new()), StreamHub::new(), port);
    ctx.thumbs = Arc::new(ThumbProxy::with(Client::new(), vec!["127.0.0.1".into()], vec![images.addr.port()]));
    tokio::spawn(http::serve(listener, Arc::new(ctx)));
    (port, images)
}

async fn get(port: u16, path: &str) -> (String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    (String::from_utf8_lossy(&raw[..split]).to_string(), raw[split + 4..].to_vec())
}

fn proxied(images: &common::Server, path: &str) -> String {
    format!("/thumb-proxy?url={}", percent_encode(&images.url_ip(path)))
}

#[tokio::test]
async fn an_allowed_image_is_relayed_with_the_headers_the_overlay_needs() {
    let (port, images) = start().await;
    let (head, body) = get(port, &proxied(&images, "/img/ok.jpg")).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let lower = head.to_ascii_lowercase();
    assert!(lower.contains("content-type: image/jpeg") && lower.contains("access-control-allow-origin: *") && lower.contains("max-age=3600"), "{head}");
    assert_eq!(body, JPEG);
    // A redirect to another allowed path is followed.
    let (head, body) = get(port, &proxied(&images, "/img/redir-ok")).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, JPEG);
}

#[tokio::test]
async fn everything_that_is_not_a_small_image_from_an_allowed_host_is_refused() {
    let (port, images) = start().await;
    for (path, status) in [("/img/page.html", 502), ("/img/missing.jpg", 502), ("/img/big.jpg", 502), ("/img/redir-bad", 502), ("/img/redir-loop", 502)] {
        let (head, body) = get(port, &proxied(&images, path)).await;
        assert!(head.starts_with(&format!("HTTP/1.1 {status}")), "{path}: {head}");
        assert!(!String::from_utf8_lossy(&body).contains("alert(1)"), "{path}: page content must never be relayed");
    }
    for url in ["https://evil.example/x.jpg", "http://127.0.0.1:1/x", "file:///etc/passwd", "", "https://i.ytimg.com@evil.example/x"] {
        let (head, _) = get(port, &format!("/thumb-proxy?url={}", percent_encode(url))).await;
        assert!(head.starts_with("HTTP/1.1 400"), "{url:?}: {head}");
    }
    let (head, _) = get(port, "/thumb-proxy").await;
    assert!(head.starts_with("HTTP/1.1 400"), "{head}");
}

#[tokio::test]
async fn the_relay_is_rate_limited() {
    let (port, images) = start().await;
    let mut limited = false;
    for _ in 0..130 {
        let (head, _) = get(port, &proxied(&images, "/img/ok.jpg")).await;
        if head.starts_with("HTTP/1.1 429") {
            limited = true;
            break;
        }
    }
    assert!(limited, "more than 120 requests in a minute must be refused");
}
