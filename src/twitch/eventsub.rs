//! Receiving chat through Twitch's EventSub WebSocket.
//!
//! One connection, kept alive for as long as the app runs: connect, wait for
//! the welcome, subscribe to the channel's chat, then forward every message.
//! Twitch's own protocol rules are followed: keepalive deadlines, "reconnect"
//! hand-overs (which keep the subscription), duplicate suppression, and
//! revocations. Any trouble becomes a retry with growing back-off, never a crash.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{mpsc::UnboundedSender, watch};
use tokio::time::{sleep, timeout};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{WebSocketStream, client_async};

use super::auth::AuthError;
use super::helix::{Helix, HelixError};
use crate::net::url::Url;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Twitch closes a session that doesn't subscribe within 10 s of the welcome.
const WELCOME_TIMEOUT: Duration = Duration::from_secs(15);
/// Slack on top of the keepalive interval Twitch announces.
const KEEPALIVE_SLACK: Duration = Duration::from_secs(8);
const MIN_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(120);
/// Waiting for a person to fix authorization is slower than a network retry.
const AUTH_RETRY: Duration = Duration::from_secs(20);
const SEEN_IDS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub message_id: String,
    pub chatter_id: String,
    pub login: String,
    pub display_name: String,
    pub text: String,
    /// Moderator or broadcaster badge.
    pub is_mod: bool,
}

/// State of the chat link, for the status page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Connecting,
    /// Subscribed: chat commands will arrive.
    Live,
    /// Not working yet, with the reason (and, usually, what to do about it).
    Waiting(String),
}

#[derive(Debug, Clone)]
pub struct Config {
    pub url: String,
    pub broadcaster_id: String,
    pub bot_id: String,
}

/// Reads a `channel.chat.message` notification's `event`.
pub fn parse_chat(event: &Value, broadcaster_id: &str) -> Option<ChatMessage> {
    // Shared chat: messages from the other channel's viewers carry that channel's id.
    if let Some(source) = event.get("source_broadcaster_user_id").and_then(Value::as_str) {
        if source != broadcaster_id {
            return None;
        }
    }
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let is_mod = event
        .get("badges")
        .and_then(Value::as_array)
        .is_some_and(|badges| badges.iter().any(|b| matches!(b.get("set_id").and_then(Value::as_str), Some("moderator" | "broadcaster"))));
    Some(ChatMessage {
        message_id: text(event, "message_id")?,
        chatter_id: text(event, "chatter_user_id")?,
        login: text(event, "chatter_user_login").unwrap_or_default(),
        display_name: text(event, "chatter_user_name").unwrap_or_default(),
        text: event.pointer("/message/text").and_then(Value::as_str)?.to_string(),
        is_mod,
    })
}

enum Outcome {
    /// Twitch asked us to move to this URL; the subscription moves with us.
    Reconnect(String),
    /// The session ended (or never worked) for this reason; retry after a back-off.
    Ended { reason: String, was_healthy: bool },
    /// Authorization is missing or insufficient; waiting for a person.
    NeedsPerson(String),
}

/// Runs forever, reconnecting as needed. Chat messages go to `tx`; progress to `link`.
pub async fn run(cfg: Config, helix: Arc<Helix>, tls: Arc<ClientConfig>, tx: UnboundedSender<ChatMessage>, link: watch::Sender<Link>) {
    let mut backoff = MIN_BACKOFF;
    let mut url = cfg.url.clone();
    let mut transferred = false;
    let mut seen: VecDeque<String> = VecDeque::with_capacity(SEEN_IDS);
    loop {
        link.send_replace(Link::Connecting);
        match session(&cfg, &url, transferred, &helix, &tls, &tx, &link, &mut seen).await {
            Outcome::Reconnect(next) => {
                url = next;
                transferred = true;
                continue;
            }
            Outcome::Ended { reason, was_healthy } => {
                if was_healthy {
                    backoff = MIN_BACKOFF;
                }
                crate::info!("EventSub chat link ended ({reason}); reconnecting in {}s.", backoff.as_secs());
                link.send_replace(Link::Waiting(format!("reconnecting: {reason}")));
                sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            Outcome::NeedsPerson(why) => {
                crate::warn!("Twitch chat is not connected: {why}");
                link.send_replace(Link::Waiting(why));
                sleep(AUTH_RETRY).await;
            }
        }
        url = cfg.url.clone();
        transferred = false;
        if tx.is_closed() {
            return;
        }
    }
}

async fn connect_ws(url: &str, tls: &Arc<ClientConfig>) -> Result<ConnectedWs, String> {
    let parsed = Url::parse(&url.replacen("ws", "http", 1)).ok_or_else(|| format!("bad EventSub URL {url:?}"))?;
    let tcp = timeout(CONNECT_TIMEOUT, TcpStream::connect((parsed.host.as_str(), parsed.port))).await.map_err(|_| "connect timed out".to_string())?.map_err(|e| format!("cannot connect: {e}"))?;
    let _ = tcp.set_nodelay(true);
    if parsed.https {
        let name = ServerName::try_from(parsed.host.clone()).map_err(|e| e.to_string())?;
        let stream = timeout(CONNECT_TIMEOUT, TlsConnector::from(tls.clone()).connect(name, tcp)).await.map_err(|_| "TLS handshake timed out".to_string())?.map_err(|e| format!("TLS: {e}"))?;
        let (ws, _) = timeout(CONNECT_TIMEOUT, client_async(url, stream)).await.map_err(|_| "WebSocket handshake timed out".to_string())?.map_err(|e| format!("WebSocket: {e}"))?;
        Ok(ConnectedWs::Tls(Box::new(ws)))
    } else {
        let (ws, _) = timeout(CONNECT_TIMEOUT, client_async(url, tcp)).await.map_err(|_| "WebSocket handshake timed out".to_string())?.map_err(|e| format!("WebSocket: {e}"))?;
        Ok(ConnectedWs::Plain(Box::new(ws)))
    }
}

enum ConnectedWs {
    Plain(Box<WebSocketStream<TcpStream>>),
    Tls(Box<WebSocketStream<tokio_rustls::client::TlsStream<TcpStream>>>),
}

#[allow(clippy::too_many_arguments)]
async fn session(cfg: &Config, url: &str, transferred: bool, helix: &Arc<Helix>, tls: &Arc<ClientConfig>, tx: &UnboundedSender<ChatMessage>, link: &watch::Sender<Link>, seen: &mut VecDeque<String>) -> Outcome {
    match connect_ws(url, tls).await {
        Err(reason) => Outcome::Ended { reason, was_healthy: false },
        Ok(ConnectedWs::Plain(ws)) => converse(*ws, cfg, transferred, helix, tx, link, seen).await,
        Ok(ConnectedWs::Tls(ws)) => converse(*ws, cfg, transferred, helix, tx, link, seen).await,
    }
}

async fn converse<S: AsyncRead + AsyncWrite + Unpin>(mut ws: WebSocketStream<S>, cfg: &Config, transferred: bool, helix: &Arc<Helix>, tx: &UnboundedSender<ChatMessage>, link: &watch::Sender<Link>, seen: &mut VecDeque<String>) -> Outcome {
    let mut deadline = WELCOME_TIMEOUT;
    let mut healthy = false;
    loop {
        let frame = match timeout(deadline, ws.next()).await {
            Err(_) => return Outcome::Ended { reason: "no message from Twitch within the keepalive window".into(), was_healthy: healthy },
            Ok(None) => return Outcome::Ended { reason: "Twitch closed the connection".into(), was_healthy: healthy },
            Ok(Some(Err(e))) => return Outcome::Ended { reason: format!("connection error: {e}"), was_healthy: healthy },
            Ok(Some(Ok(frame))) => frame,
        };
        let text = match frame {
            Message::Text(t) => t,
            Message::Ping(p) => {
                let _ = ws.send(Message::Pong(p)).await;
                continue;
            }
            Message::Close(_) => return Outcome::Ended { reason: "Twitch closed the connection".into(), was_healthy: healthy },
            _ => continue,
        };
        let Ok(msg) = serde_json::from_str::<Value>(text.as_str()) else { continue };
        let kind = msg.pointer("/metadata/message_type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "session_welcome" => {
                let session = msg.pointer("/payload/session");
                let keepalive = session.and_then(|s| s.get("keepalive_timeout_seconds")).and_then(Value::as_u64).unwrap_or(10);
                deadline = Duration::from_secs(keepalive) + KEEPALIVE_SLACK;
                let Some(id) = session.and_then(|s| s.get("id")).and_then(Value::as_str).map(str::to_string) else {
                    return Outcome::Ended { reason: "welcome without a session id".into(), was_healthy: false };
                };
                if !transferred {
                    let (h, b, bot) = (helix.clone(), cfg.broadcaster_id.clone(), cfg.bot_id.clone());
                    match tokio::task::spawn_blocking(move || h.subscribe_chat(&id, &b, &bot)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => return subscription_failed(e),
                        Err(_) => return Outcome::Ended { reason: "the subscribe call crashed".into(), was_healthy: false },
                    }
                }
                healthy = true;
                link.send_replace(Link::Live);
                crate::info!("Subscribed to chat for channel {}.", cfg.broadcaster_id);
            }
            "session_keepalive" => {}
            "session_reconnect" => {
                let Some(next) = msg.pointer("/payload/session/reconnect_url").and_then(Value::as_str) else { continue };
                return Outcome::Reconnect(next.to_string());
            }
            "revocation" => {
                let status = msg.pointer("/payload/subscription/status").and_then(Value::as_str).unwrap_or("revoked");
                return if status.contains("authorization") || status.contains("user_removed") {
                    Outcome::NeedsPerson(format!("Twitch revoked the chat subscription ({status}); authorize the accounts again on the setup page"))
                } else {
                    Outcome::Ended { reason: format!("subscription revoked ({status})"), was_healthy: healthy }
                };
            }
            "notification" => {
                if msg.pointer("/payload/subscription/type").and_then(Value::as_str) != Some("channel.chat.message") {
                    continue;
                }
                // Twitch can deliver the same notification twice around a reconnect.
                if let Some(id) = msg.pointer("/metadata/message_id").and_then(Value::as_str) {
                    if seen.iter().any(|s| s == id) {
                        continue;
                    }
                    if seen.len() == SEEN_IDS {
                        seen.pop_front();
                    }
                    seen.push_back(id.to_string());
                }
                if let Some(chat) = msg.pointer("/payload/event").and_then(|e| parse_chat(e, &cfg.broadcaster_id)) {
                    if tx.send(chat).is_err() {
                        return Outcome::Ended { reason: "shutting down".into(), was_healthy: healthy };
                    }
                }
            }
            _ => {}
        }
    }
}

fn subscription_failed(e: HelixError) -> Outcome {
    match e {
        HelixError::Auth(AuthError::NeedsAuthorization(m)) => Outcome::NeedsPerson(format!("the bot account needs to be authorized ({m}); open the setup page")),
        HelixError::Forbidden(m) => Outcome::NeedsPerson(format!("Twitch won't let the bot read this chat ({m}); make the bot a moderator of the channel, or authorize the broadcaster account too")),
        HelixError::Auth(e) => Outcome::NeedsPerson(e.to_string()),
        other => Outcome::Ended { reason: format!("could not subscribe: {other}"), was_healthy: false },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event() -> Value {
        json!({
            "broadcaster_user_id": "100", "chatter_user_id": "42", "chatter_user_login": "viewer", "chatter_user_name": "Viewer",
            "message_id": "m-1", "message": {"text": "!sr some song"}, "message_type": "text",
            "badges": [{"set_id": "subscriber", "id": "0", "info": "3"}], "source_broadcaster_user_id": null
        })
    }

    #[test]
    fn a_normal_chat_message_is_parsed() {
        let m = parse_chat(&event(), "100").unwrap();
        assert_eq!((m.chatter_id.as_str(), m.display_name.as_str(), m.text.as_str(), m.is_mod), ("42", "Viewer", "!sr some song", false));
        assert_eq!(m.message_id, "m-1");
    }

    #[test]
    fn moderators_and_the_broadcaster_count_as_mods_but_vips_and_subs_do_not() {
        for (badge, expected) in [("moderator", true), ("broadcaster", true), ("vip", false), ("subscriber", false), ("staff", false)] {
            let mut e = event();
            e["badges"] = json!([{"set_id": "subscriber"}, {"set_id": badge}]);
            assert_eq!(parse_chat(&e, "100").unwrap().is_mod, expected, "{badge}");
        }
    }

    #[test]
    fn messages_from_a_shared_chat_partner_are_ignored() {
        let mut e = event();
        e["source_broadcaster_user_id"] = json!("999");
        assert!(parse_chat(&e, "100").is_none());
        e["source_broadcaster_user_id"] = json!("100");
        assert!(parse_chat(&e, "100").is_some(), "shared chat echo of our own channel is fine");
    }

    #[test]
    fn malformed_events_are_dropped_not_fatal() {
        assert!(parse_chat(&json!({}), "100").is_none());
        assert!(parse_chat(&json!({"message_id": "x", "chatter_user_id": "1"}), "100").is_none());
        let mut e = event();
        e["message"] = json!("not an object");
        assert!(parse_chat(&e, "100").is_none());
    }
}
