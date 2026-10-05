//! The local HTTP server (loopback only): the Opus stream for OBS, the
//! overlay page, live now-playing data over JSON and WebSocket, and a health
//! check. Built directly on hyper - no web framework - and every response
//! body is either a small in-memory buffer or the live stream, so there is
//! no boxing or per-request buffering.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::header::{self, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Message, Role};

use crate::hub::{StreamHub, Subscription};
use crate::state::Shared;

/// The overlay is a single static page, compiled into the binary.
const OVERLAY_HTML: &str = include_str!("../assets/overlay.html");

const WS_HEARTBEAT: Duration = Duration::from_secs(30);
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Far above anything a single streamer needs (OBS, a browser source, a
/// settings tab); it only bounds memory if something local misbehaves.
const MAX_CONNECTIONS: usize = 64;

pub struct Ctx {
    pub shared: Arc<Shared>,
    pub hub: Arc<StreamHub>,
    pub port: u16,
}

// -------------------------------------------------------------------- guard

/// Every Host header a legitimate local client can send. Anything else is
/// what a DNS-rebinding page looks like (its own hostname resolving to 127.0.0.1).
pub fn host_allowed(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else { return false };
    let host = host.trim().to_ascii_lowercase();
    ["127.0.0.1", "localhost", "[::1]"].iter().any(|name| host == format!("{name}:{port}"))
}

// --------------------------------------------------------------------- body

pub enum RespBody {
    Full(Option<Bytes>),
    Stream { first: Option<Bytes>, sub: Subscription },
}

impl Body for RespBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match self.get_mut() {
            RespBody::Full(bytes) => Poll::Ready(bytes.take().map(|b| Ok(Frame::data(b)))),
            RespBody::Stream { first, sub } => {
                if let Some(head) = first.take().filter(|h| !h.is_empty()) {
                    return Poll::Ready(Some(Ok(Frame::data(head))));
                }
                loop {
                    return match sub.rx.poll_recv(cx) {
                        Poll::Ready(Some(chunk)) if chunk.is_empty() => continue,
                        Poll::Ready(Some(chunk)) => Poll::Ready(Some(Ok(Frame::data(chunk)))),
                        // Dropped for falling behind (or the hub shut down): end the response.
                        Poll::Ready(None) => Poll::Ready(None),
                        Poll::Pending => Poll::Pending,
                    };
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self, RespBody::Full(None))
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            RespBody::Full(Some(b)) => SizeHint::with_exact(b.len() as u64),
            RespBody::Full(None) => SizeHint::with_exact(0),
            RespBody::Stream { .. } => SizeHint::default(),
        }
    }
}

fn respond(status: StatusCode, content_type: &'static str, body: impl Into<Bytes>) -> Response<RespBody> {
    let mut resp = Response::new(RespBody::Full(Some(body.into())));
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    resp
}

fn json_response(value: &serde_json::Value) -> Response<RespBody> {
    respond(StatusCode::OK, "application/json; charset=utf-8", value.to_string())
}

fn plain(status: StatusCode, text: &'static str) -> Response<RespBody> {
    respond(status, "text/plain; charset=utf-8", text)
}

// ------------------------------------------------------------------- routes

async fn route(req: Request<Incoming>, ctx: Arc<Ctx>) -> Result<Response<RespBody>, Infallible> {
    let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok());
    if !host_allowed(host, ctx.port) {
        // 421 Misdirected Request: reached us under a name we don't serve.
        return Ok(plain(StatusCode::MISDIRECTED_REQUEST, "This server only answers on 127.0.0.1 / localhost."));
    }
    let head_only = match *req.method() {
        Method::GET => false,
        Method::HEAD => true,
        _ => return Ok(plain(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed.")),
    };
    Ok(match req.uri().path() {
        "/healthz" => json_response(&ctx.shared.health_json()),
        "/nowplaying.json" => json_response(&ctx.shared.nowplaying_json()),
        "/overlay" => respond(StatusCode::OK, "text/html; charset=utf-8", OVERLAY_HTML),
        "/stream.opus" => stream_response(&ctx, head_only),
        "/ws/nowplaying" => websocket(req, ctx),
        _ => plain(StatusCode::NOT_FOUND, "Not found."),
    })
}

fn stream_response(ctx: &Ctx, head_only: bool) -> Response<RespBody> {
    let body = if head_only {
        RespBody::Full(None)
    } else {
        // Subscribe first, then replay the current Ogg header pages so this
        // listener's stream is decodable from byte one.
        let sub = ctx.hub.subscribe();
        RespBody::Stream { first: Some(sub.header.clone()), sub }
    };
    let mut resp = Response::new(body);
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/ogg"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    resp
}

fn header_has_token(req: &Request<Incoming>, name: header::HeaderName, token: &str) -> bool {
    req.headers()
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|t| t.trim().eq_ignore_ascii_case(token))
}

fn websocket(req: Request<Incoming>, ctx: Arc<Ctx>) -> Response<RespBody> {
    let key = req.headers().get(header::SEC_WEBSOCKET_KEY).map(|k| k.as_bytes().to_vec());
    let version_ok = req.headers().get(header::SEC_WEBSOCKET_VERSION).is_some_and(|v| v.as_bytes() == b"13");
    let upgrade_ok = header_has_token(&req, header::UPGRADE, "websocket") && header_has_token(&req, header::CONNECTION, "upgrade");
    let (true, true, Some(key)) = (version_ok, upgrade_ok, key) else {
        return plain(StatusCode::BAD_REQUEST, "WebSocket upgrade required.");
    };
    let accept = derive_accept_key(&key);
    tokio::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                let ws = WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None).await;
                push_nowplaying(ws, &ctx).await;
            }
            Err(e) => crate::debug!("WebSocket upgrade failed: {e}"),
        }
    });
    let mut resp = Response::new(RespBody::Full(None));
    *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let h = resp.headers_mut();
    h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    h.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    if let Ok(v) = HeaderValue::from_str(&accept) {
        h.insert(header::SEC_WEBSOCKET_ACCEPT, v);
    }
    resp
}

/// One snapshot on connect, then another whenever anything changes. The
/// client ticks elapsed time between pushes itself, so nothing is sent every
/// second. A ping every 30 s notices dead connections.
async fn push_nowplaying<S: AsyncRead + AsyncWrite + Unpin>(mut ws: WebSocketStream<S>, ctx: &Ctx) {
    let mut changes = ctx.shared.subscribe_changes();
    let mut heartbeat = tokio::time::interval(WS_HEARTBEAT);
    heartbeat.tick().await; // the first tick is immediate
    let mut awaiting_pong = false;
    let snapshot = |ctx: &Ctx| Message::Text(ctx.shared.nowplaying_json().to_string().into());
    if ws.send(snapshot(ctx)).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            changed = changes.changed() => {
                if changed.is_err() || ws.send(snapshot(ctx)).await.is_err() {
                    break;
                }
            }
            _ = heartbeat.tick() => {
                if awaiting_pong || ws.send(Message::Ping(Bytes::new())).await.is_err() {
                    break;
                }
                awaiting_pong = true;
            }
            incoming = ws.next() => match incoming {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                Some(Ok(_)) => {
                    awaiting_pong = false;
                    // Flushes the pong tungstenite queued in reply to a client ping.
                    if ws.flush().await.is_err() {
                        break;
                    }
                }
            },
        }
    }
    let _ = ws.close(None).await;
}

// ------------------------------------------------------------------- server

pub async fn serve(listener: TcpListener, ctx: Arc<Ctx>) {
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                crate::warn!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
            crate::warn!("too many open connections; refusing one");
            continue;
        };
        let _ = stream.set_nodelay(true);
        let ctx = Arc::clone(&ctx);
        tokio::spawn(async move {
            let service = service_fn(move |req| route(req, Arc::clone(&ctx)));
            let conn = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_READ_TIMEOUT)
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades();
            if let Err(e) = conn.await {
                crate::debug!("connection ended: {e}");
            }
            drop(permit);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_names_on_our_port_are_allowed() {
        assert!(host_allowed(Some("127.0.0.1:8098"), 8098));
        assert!(host_allowed(Some("LOCALHOST:8098"), 8098));
        assert!(host_allowed(Some(" [::1]:8098 "), 8098));
        assert!(!host_allowed(Some("evil.example:8098"), 8098));
        assert!(!host_allowed(Some("127.0.0.1:9999"), 8098));
        assert!(!host_allowed(Some("127.0.0.1"), 8098));
        assert!(!host_allowed(None, 8098));
    }
}
