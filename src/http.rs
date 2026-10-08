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
use http_body_util::{BodyExt, Limited};
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
use crate::settings::{self, SettingsPage};
use crate::setup::Setup;
use crate::state::Shared;
use crate::thumb::ThumbProxy;

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
    /// The main port (what OBS is pointed at).
    pub port: u16,
    /// Other ports this app also listens on (the OAuth redirect port), accepted in Host headers.
    pub extra_ports: Vec<u16>,
    pub setup: Option<Arc<Setup>>,
    pub settings: Option<Arc<SettingsPage>>,
    pub thumbs: Arc<ThumbProxy>,
}

impl Ctx {
    pub fn new(shared: Arc<Shared>, hub: Arc<StreamHub>, port: u16) -> Ctx {
        Ctx { shared, hub, port, extra_ports: Vec::new(), setup: None, settings: None, thumbs: Arc::new(ThumbProxy::default()) }
    }

    fn ports(&self) -> Vec<u16> {
        std::iter::once(self.port).chain(self.extra_ports.iter().copied()).collect()
    }
}

// -------------------------------------------------------------------- guard

/// Every Host header a legitimate local client can send. Anything else is
/// what a DNS-rebinding page looks like (its own hostname resolving to 127.0.0.1).
pub fn host_allowed(host: Option<&str>, ports: &[u16]) -> bool {
    let Some(host) = host else { return false };
    let host = host.trim().to_ascii_lowercase();
    ["127.0.0.1", "localhost", "[::1]"].iter().any(|name| ports.iter().any(|port| host == format!("{name}:{port}")))
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
    if !host_allowed(host, &ctx.ports()) {
        // 421 Misdirected Request: reached us under a name we don't serve.
        return Ok(plain(StatusCode::MISDIRECTED_REQUEST, "This server only answers on 127.0.0.1 / localhost."));
    }
    if *req.method() == Method::POST && req.uri().path() == "/settings" {
        return Ok(settings_post(req, &ctx).await);
    }
    let head_only = match *req.method() {
        Method::GET => false,
        Method::HEAD => true,
        _ => return Ok(plain(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed.")),
    };
    let query = req.uri().query().unwrap_or("").to_string();
    Ok(match req.uri().path() {
        "/" => redirect("/setup"),
        "/setup" => match &ctx.setup {
            Some(setup) => respond(StatusCode::OK, "text/html; charset=utf-8", setup.status_page()),
            None => plain(StatusCode::NOT_FOUND, "Not found."),
        },
        "/settings" => match &ctx.settings {
            Some(page) => protect(respond(StatusCode::OK, "text/html; charset=utf-8", page.render(None))),
            None => plain(StatusCode::NOT_FOUND, "Not found."),
        },
        "/oauth/start" => match &ctx.setup {
            Some(setup) => {
                let who = crate::net::url::parse_query(&query).into_iter().find(|(k, _)| k == "who").map(|(_, v)| v).unwrap_or_default();
                match setup.start(&who) {
                    Ok(url) => redirect(&url),
                    Err((status, html)) => respond(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST), "text/html; charset=utf-8", html),
                }
            }
            None => plain(StatusCode::NOT_FOUND, "Not found."),
        },
        "/oauth/callback" => match ctx.setup.clone() {
            Some(setup) => {
                let (status, html) = tokio::task::spawn_blocking(move || setup.callback(&query)).await.unwrap_or((500, "Something went wrong.".to_string()));
                respond(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST), "text/html; charset=utf-8", html)
            }
            None => plain(StatusCode::NOT_FOUND, "Not found."),
        },
        "/thumb-proxy" => thumb_response(&ctx, &query).await,
        "/healthz" => json_response(&ctx.shared.health_json()),
        "/nowplaying.json" => json_response(&ctx.shared.nowplaying_json()),
        "/overlay" => respond(StatusCode::OK, "text/html; charset=utf-8", OVERLAY_HTML),
        "/stream.opus" => stream_response(&ctx, head_only),
        "/ws/nowplaying" => websocket(req, ctx),
        _ => plain(StatusCode::NOT_FOUND, "Not found."),
    })
}

/// Keeps a page out of caches and frames (clickjacking).
fn protect(mut resp: Response<RespBody>) -> Response<RespBody> {
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("frame-ancestors 'none'"));
    resp
}

async fn settings_post(req: Request<Incoming>, ctx: &Ctx) -> Response<RespBody> {
    let Some(page) = &ctx.settings else { return plain(StatusCode::NOT_FOUND, "Not found.") };
    let header_of = |name: header::HeaderName| req.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    let site = req.headers().get("sec-fetch-site").and_then(|v| v.to_str().ok()).map(str::to_string);
    let (origin, referer, host) = (header_of(header::ORIGIN), header_of(header::REFERER), header_of(header::HOST));
    if !settings::origin_ok(origin.as_deref(), referer.as_deref(), host.as_deref(), site.as_deref()) {
        crate::warn!("Rejected a settings change: its Origin/Referer/Sec-Fetch-Site didn't match this server (possible cross-site request).");
        return protect(plain(StatusCode::FORBIDDEN, "Origin check failed \u{2014} refusing to save."));
    }
    let is_form = header_of(header::CONTENT_TYPE).is_some_and(|ct| ct.to_ascii_lowercase().starts_with("application/x-www-form-urlencoded"));
    if !is_form {
        return protect(plain(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Expected a form submission."));
    }
    let announced = header_of(header::CONTENT_LENGTH).and_then(|v| v.parse::<usize>().ok());
    if announced.is_some_and(|n| n > settings::MAX_FORM_BYTES) {
        return protect(plain(StatusCode::PAYLOAD_TOO_LARGE, "That's too big for a settings form."));
    }
    let body = match Limited::new(req.into_body(), settings::MAX_FORM_BYTES).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return protect(plain(StatusCode::PAYLOAD_TOO_LARGE, "That's too big for a settings form.")),
    };
    let form = crate::net::url::parse_query(&String::from_utf8_lossy(&body));
    let (status, html) = match page.apply(&form) {
        Ok(()) => (StatusCode::OK, page.render(Some(("Saved.", false)))),
        Err(errors) => (StatusCode::BAD_REQUEST, page.render(Some((&format!("Nothing was saved \u{2014} {}", errors.join("; ")), true)))),
    };
    protect(respond(status, "text/html; charset=utf-8", html))
}

fn redirect(to: &str) -> Response<RespBody> {
    let mut resp = Response::new(RespBody::Full(None));
    *resp.status_mut() = StatusCode::FOUND;
    if let Ok(v) = HeaderValue::from_str(to) {
        resp.headers_mut().insert(header::LOCATION, v);
    }
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

async fn thumb_response(ctx: &Arc<Ctx>, query: &str) -> Response<RespBody> {
    let url = crate::net::url::parse_query(query).into_iter().find(|(k, _)| k == "url").map(|(_, v)| v).unwrap_or_default();
    let thumbs = ctx.thumbs.clone();
    match tokio::task::spawn_blocking(move || thumbs.fetch(&url)).await {
        Ok(Ok((bytes, content_type))) => {
            let mut resp = Response::new(RespBody::Full(Some(Bytes::from(bytes))));
            let h = resp.headers_mut();
            if let Ok(v) = HeaderValue::from_str(&content_type) {
                h.insert(header::CONTENT_TYPE, v);
            }
            h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
            h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=3600"));
            resp
        }
        Ok(Err(e)) => plain_owned(StatusCode::from_u16(e.status()).unwrap_or(StatusCode::BAD_GATEWAY), e.message()),
        Err(_) => plain(StatusCode::BAD_GATEWAY, "Upstream fetch failed"),
    }
}

fn plain_owned(status: StatusCode, text: &'static str) -> Response<RespBody> {
    plain(status, text)
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
        assert!(host_allowed(Some("127.0.0.1:8098"), &[8098]));
        assert!(host_allowed(Some("LOCALHOST:8098"), &[8098]));
        assert!(host_allowed(Some(" [::1]:8098 "), &[8098]));
        assert!(host_allowed(Some("localhost:4343"), &[8098, 4343]));
        assert!(!host_allowed(Some("evil.example:8098"), &[8098]));
        assert!(!host_allowed(Some("127.0.0.1:9999"), &[8098, 4343]));
        assert!(!host_allowed(Some("127.0.0.1"), &[8098]));
        assert!(!host_allowed(None, &[8098]));
    }
}
