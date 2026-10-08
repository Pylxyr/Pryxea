//! The browser side of setup: a status page, and the Twitch authorization
//! round trip (start -> Twitch -> callback) that saves a token for the bot
//! account and, if needed, for the broadcaster.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::net::url::{parse_query, percent_encode};
use crate::twitch::auth::Auth;
use crate::twitch::eventsub::Link;
use crate::twitch::{BOT_SCOPES, BROADCASTER_SCOPES};

const STATE_LIFETIME: Duration = Duration::from_secs(10 * 60);
/// More outstanding authorizations than this means somebody is hammering the endpoint.
const MAX_PENDING_STATES: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Bot,
    Broadcaster,
}

pub struct Setup {
    auth: Option<Arc<Auth>>,
    redirect_uri: String,
    bot_id: String,
    owner_id: String,
    main_port: u16,
    link: watch::Receiver<Link>,
    /// Configuration problems found at start-up (shown at the top of the page).
    problems: Vec<String>,
    states: Mutex<HashMap<String, (Instant, Role)>>,
}

pub fn esc(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&#39;".to_string(),
            c => c.to_string(),
        })
        .collect()
}

/// 16 bytes from the OS's secure generator, as hex.
fn random_state() -> String {
    let mut buf = [0u8; 16];
    let _ = rustls::crypto::ring::default_provider().secure_random.fill(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

const STYLE: &str = "body{font:16px/1.5 system-ui,sans-serif;background:#14141c;color:#e8e8f0;max-width:44rem;margin:2rem auto;padding:0 1rem}\
h1{font-size:1.5rem}a.btn{display:inline-block;background:#9146ff;color:#fff;padding:.4rem .9rem;border-radius:.4rem;text-decoration:none}\
.ok{color:#6fdc8c}.bad{color:#ff8a8a}.dim{color:#9a9ab0}code{background:#22222e;padding:.1rem .35rem;border-radius:.25rem}\
table{border-collapse:collapse;width:100%}td{padding:.5rem .4rem;border-top:1px solid #2c2c3a;vertical-align:top}";

fn page(title: &str, body: &str) -> String {
    format!("<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><style>{STYLE}</style></head><body>{body}</body></html>", esc(title))
}

impl Setup {
    pub fn new(auth: Option<Arc<Auth>>, redirect_uri: String, bot_id: String, owner_id: String, main_port: u16, link: watch::Receiver<Link>, problems: Vec<String>) -> Setup {
        Setup { auth, redirect_uri, bot_id, owner_id, main_port, link, problems, states: Mutex::default() }
    }

    pub fn status_page(&self) -> String {
        let mut body = String::from("<h1>Pryxea setup</h1>");
        if !self.problems.is_empty() {
            body.push_str("<p class=\"bad\">Twitch isn't configured yet. Fix these in the <code>.env</code> file, then restart:</p><ul>");
            for p in &self.problems {
                body.push_str(&format!("<li>{}</li>", esc(p)));
            }
            body.push_str("</ul>");
        } else if let Some(auth) = &self.auth {
            let (class, text) = match &*self.link.borrow() {
                Link::Live => ("ok", "Connected: chat commands are live.".to_string()),
                Link::Connecting => ("dim", "Connecting to Twitch\u{2026}".to_string()),
                Link::Waiting(why) => ("bad", format!("Not connected: {why}")),
            };
            body.push_str(&format!("<p class=\"{class}\">{}</p><table>", esc(&text)));
            for acct in auth.account_summary(&self.bot_id, &self.owner_id) {
                let (label, note, who) = if acct.role == "bot" {
                    ("Bot account", "The account that reads and answers chat. Log in to Twitch as this account.", "bot")
                } else {
                    ("Broadcaster account", "Only needed if the bot is not a moderator of your channel. Log in as the channel owner.", "broadcaster")
                };
                let state = match (&acct.login, acct.authorized) {
                    (Some(login), true) if !login.is_empty() => format!("<span class=\"ok\">\u{2713} authorized as {}</span>", esc(login)),
                    (_, true) => "<span class=\"ok\">\u{2713} authorized</span>".to_string(),
                    _ => "<span class=\"bad\">not authorized yet</span>".to_string(),
                };
                body.push_str(&format!(
                    "<tr><td><b>{label}</b><br><span class=\"dim\">user id {}<br>{note}</span></td><td>{state}<br><a class=\"btn\" href=\"/oauth/start?who={who}\">{}</a></td></tr>",
                    esc(&acct.user_id),
                    if acct.authorized { "Authorize again" } else { "Authorize" }
                ));
            }
            body.push_str("</table>");
            body.push_str(&format!("<p class=\"dim\">Your Twitch application must list <code>{}</code> as an OAuth Redirect URL (dev.twitch.tv/console).</p>", esc(&self.redirect_uri)));
        }
        body.push_str(&format!(
            "<h2>OBS</h2><p>Media Source: <code>http://127.0.0.1:{p}/stream.opus</code><br>Browser Source: <code>http://127.0.0.1:{p}/overlay</code></p>",
            p = self.main_port
        ));
        page("Pryxea setup", &body)
    }

    /// Where to send the browser to authorize an account. `who` is "bot" or "broadcaster".
    pub fn start(&self, who: &str) -> Result<String, (u16, String)> {
        let auth = self.auth.as_ref().ok_or((503, page("Pryxea", "<p class=\"bad\">Twitch isn't configured yet. See <a href=\"/setup\">the setup page</a>.</p>")))?;
        let (role, scopes): (Role, &[&str]) = match who {
            "bot" => (Role::Bot, &BOT_SCOPES),
            "broadcaster" => (Role::Broadcaster, &BROADCASTER_SCOPES),
            _ => return Err((400, page("Pryxea", "<p class=\"bad\">Unknown account.</p><p><a href=\"/setup\">Back</a></p>"))),
        };
        let state = random_state();
        {
            let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
            states.retain(|_, (at, _)| at.elapsed() < STATE_LIFETIME);
            if states.len() >= MAX_PENDING_STATES {
                return Err((429, page("Pryxea", "<p class=\"bad\">Too many authorizations in progress. Wait a few minutes.</p>")));
            }
            states.insert(state.clone(), (Instant::now(), role));
        }
        // force_verify so Twitch asks which account to use instead of silently reusing the browser's.
        Ok(auth.authorize_url(&self.redirect_uri, scopes, &state, true))
    }

    /// Finishes the round trip. Blocking (talks to Twitch). Returns (status, html).
    pub fn callback(&self, query: &str) -> (u16, String) {
        let params = parse_query(query);
        let get = |k: &str| params.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        let failure = |msg: &str| (400u16, page("Pryxea", &format!("<h1>Authorization didn't work</h1><p class=\"bad\">{}</p><p><a class=\"btn\" href=\"/setup\">Back to setup</a></p>", esc(msg))));
        let Some(auth) = &self.auth else { return failure("Twitch isn't configured yet.") };
        if let Some(error) = get("error") {
            return failure(&format!("Twitch said: {} {}", error, get("error_description").unwrap_or("")));
        }
        let (Some(code), Some(state)) = (get("code"), get("state")) else { return failure("The response from Twitch was incomplete.") };
        let role = {
            let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
            states.retain(|_, (at, _)| at.elapsed() < STATE_LIFETIME);
            states.remove(state).map(|(_, role)| role)
        };
        let Some(role) = role else { return failure("This authorization link is no longer valid (it expired, or it wasn't started from this page). Start again from the setup page.") };
        let token = match auth.exchange_code(code, &self.redirect_uri) {
            Ok(t) => t,
            Err(e) => return failure(&e.to_string()),
        };
        let (expected, label) = match role {
            Role::Bot => (&self.bot_id, "bot"),
            Role::Broadcaster => (&self.owner_id, "broadcaster"),
        };
        if &token.user_id != expected {
            // Don't keep a token for an account we don't use.
            if token.user_id != self.bot_id && token.user_id != self.owner_id {
                auth.forget(&token.user_id);
            }
            return failure(&format!("You logged in as {} (id {}), but the {label} account in your settings is id {expected}. Log out of Twitch (or use a private window) and authorize the right account.", token.login, token.user_id));
        }
        crate::info!("Authorized the {label} account {} ({}).", token.login, token.user_id);
        (200, page("Pryxea", &format!("<h1 class=\"ok\">\u{2713} Authorized {}</h1><p>The {label} account is set up. You can close this tab.</p><p><a class=\"btn\" href=\"/setup\">Back to setup</a></p>", esc(&token.login))))
    }

    /// For tests: how many authorizations are in flight.
    pub fn pending(&self) -> usize {
        self.states.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// `/oauth/start` target for the address bar, used in messages.
pub fn start_path(who: &str) -> String {
    format!("/oauth/start?who={}", percent_encode(who))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_is_escaped() {
        assert_eq!(esc("<script>\"x\" & 'y'</script>"), "&lt;script&gt;&quot;x&quot; &amp; &#39;y&#39;&lt;/script&gt;");
    }

    #[test]
    fn states_are_random_and_long() {
        let (a, b) = (random_state(), random_state());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn the_unconfigured_page_lists_the_problems_and_still_shows_the_obs_urls() {
        let (_tx, rx) = watch::channel(Link::Connecting);
        let s = Setup::new(None, "http://localhost:4343/oauth/callback".into(), String::new(), String::new(), 8098, rx, vec!["TWITCH_CLIENT_ID is not set.".into(), "<b>evil</b>".into()]);
        let html = s.status_page();
        assert!(html.contains("TWITCH_CLIENT_ID is not set.") && html.contains("&lt;b&gt;evil&lt;/b&gt;") && !html.contains("<b>evil</b>"));
        assert!(html.contains("http://127.0.0.1:8098/stream.opus"));
        assert_eq!(s.start("bot").unwrap_err().0, 503);
        assert_eq!(s.callback("code=x&state=y").0, 400);
    }
}
