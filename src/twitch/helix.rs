//! The two Helix calls Pryxea needs: send a chat message, and subscribe the
//! EventSub session to chat. Both retry once with a refreshed token on 401.

use std::fmt;
use std::sync::Arc;

use serde_json::{Value, json};

use super::auth::{Auth, AuthError, Token};
use crate::net::http::{Client, HttpError, Request};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelixError {
    /// No usable token for the account that must act.
    Auth(AuthError),
    /// Twitch understood but won't allow it (banned, missing scope, not a moderator...).
    Forbidden(String),
    RateLimited,
    /// Any other rejection, with Twitch's own explanation.
    Rejected(u16, String),
    Network(String),
}

impl fmt::Display for HelixError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HelixError::Auth(e) => write!(f, "{e}"),
            HelixError::Forbidden(m) => write!(f, "not allowed: {m}"),
            HelixError::RateLimited => f.write_str("rate limited by Twitch"),
            HelixError::Rejected(code, m) => write!(f, "Twitch said HTTP {code}: {m}"),
            HelixError::Network(m) => write!(f, "cannot reach Twitch: {m}"),
        }
    }
}

impl std::error::Error for HelixError {}

impl From<AuthError> for HelixError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::Network(m) => HelixError::Network(m),
            other => HelixError::Auth(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    Delivered,
    /// Twitch accepted the call but didn't post the message (duplicate, blocked term...).
    Dropped(String),
}

pub struct Helix {
    auth: Arc<Auth>,
    http: Arc<Client>,
}

impl Helix {
    pub fn new(auth: Arc<Auth>, http: Arc<Client>) -> Helix {
        Helix { auth, http }
    }

    pub fn auth(&self) -> &Arc<Auth> {
        &self.auth
    }

    /// Sends a request as `user_id`, refreshing that account's token and retrying once on 401.
    fn call(&self, user_id: &str, build: impl Fn(&Token) -> Request) -> Result<(u16, Value), HelixError> {
        let mut token = self.auth.usable_token(user_id)?;
        for attempt in 0..2 {
            let request = build(&token).header("Authorization", &format!("Bearer {}", token.access)).header("Client-Id", &self.auth.credentials().client_id);
            let resp = self.http.send(&request).map_err(|e: HttpError| HelixError::Network(e.to_string()))?;
            let status = resp.status;
            let body: Value = serde_json::from_slice(&resp.bytes(256 * 1024).map_err(|e| HelixError::Network(e.to_string()))?).unwrap_or(Value::Null);
            if status == 401 && attempt == 0 {
                token = self.auth.force_refresh(user_id)?;
                continue;
            }
            return Ok((status, body));
        }
        Err(HelixError::Auth(AuthError::NeedsAuthorization("Twitch keeps rejecting the saved token".into())))
    }

    fn explain(status: u16, body: &Value) -> HelixError {
        let message = body.get("message").and_then(Value::as_str).unwrap_or("no details").to_string();
        match status {
            401 => HelixError::Auth(AuthError::NeedsAuthorization(message)),
            403 => HelixError::Forbidden(message),
            429 => HelixError::RateLimited,
            _ => HelixError::Rejected(status, message),
        }
    }

    pub fn send_chat_message(&self, broadcaster_id: &str, sender_id: &str, message: &str, reply_to: Option<&str>) -> Result<Sent, HelixError> {
        let mut payload = json!({"broadcaster_id": broadcaster_id, "sender_id": sender_id, "message": message});
        if let Some(id) = reply_to {
            payload["reply_parent_message_id"] = json!(id);
        }
        let url = format!("{}/chat/messages", self.auth.endpoints().api);
        let (status, body) = self.call(sender_id, |_| Request::post(&url, payload.to_string().into_bytes()).header("Content-Type", "application/json"))?;
        if status != 200 {
            return Err(Self::explain(status, &body));
        }
        let item = body.pointer("/data/0");
        if item.and_then(|d| d.get("is_sent")).and_then(Value::as_bool) == Some(true) {
            return Ok(Sent::Delivered);
        }
        let reason = item.and_then(|d| d.pointer("/drop_reason/message")).and_then(Value::as_str).unwrap_or("no reason given");
        Ok(Sent::Dropped(reason.to_string()))
    }

    /// Asks Twitch to deliver the channel's chat to this EventSub WebSocket session.
    /// An already-existing subscription (HTTP 409) counts as success.
    pub fn subscribe_chat(&self, session_id: &str, broadcaster_id: &str, bot_id: &str) -> Result<(), HelixError> {
        let payload = json!({
            "type": "channel.chat.message", "version": "1",
            "condition": {"broadcaster_user_id": broadcaster_id, "user_id": bot_id},
            "transport": {"method": "websocket", "session_id": session_id},
        });
        let url = format!("{}/eventsub/subscriptions", self.auth.endpoints().api);
        let (status, body) = self.call(bot_id, |_| Request::post(&url, payload.to_string().into_bytes()).header("Content-Type", "application/json"))?;
        match status {
            200..=299 | 409 => Ok(()),
            _ => Err(Self::explain(status, &body)),
        }
    }
}
