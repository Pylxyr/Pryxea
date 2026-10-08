//! Twitch: OAuth and token upkeep, the Helix API (sending chat), and the
//! EventSub WebSocket (receiving chat). Everything talks to endpoints given
//! by [`Endpoints`], so tests can point it at local fakes.

pub mod auth;
pub mod chat;
pub mod eventsub;
pub mod helix;

/// Where Twitch's services live.
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// OAuth: `<id>/oauth2/authorize`, `/token`, `/validate`.
    pub id: String,
    /// Helix REST API base, without a trailing slash.
    pub api: String,
    /// EventSub WebSocket URL.
    pub eventsub: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints { id: "https://id.twitch.tv".into(), api: "https://api.twitch.tv/helix".into(), eventsub: "wss://eventsub.wss.twitch.tv/ws".into() }
    }
}

/// What Pryxea needs from Twitch, per account.
pub const BOT_SCOPES: [&str; 3] = ["user:read:chat", "user:write:chat", "user:bot"];
/// Only needed when the bot account is not a moderator of the channel.
pub const BROADCASTER_SCOPES: [&str; 1] = ["channel:bot"];
