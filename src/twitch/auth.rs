//! OAuth tokens: the authorization-code flow for first-time setup, then
//! validation and refresh so the bot keeps working unattended.
//!
//! Tokens live in one JSON file keyed by Twitch user id. The format is a
//! superset of the one the Python bot (twitchio) wrote, so an existing token
//! file keeps working without re-authorizing.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::Endpoints;
use crate::net::http::{Client, HttpError, Request};
use crate::net::url::{form_encode, percent_encode};
use crate::store::{JsonMap, JsonStore};

/// A token this close to expiry is refreshed ahead of time.
const REFRESH_MARGIN_SECS: u64 = 600;
/// Twitch asks apps to validate hourly; checking more often than this is wasted effort.
const VALIDATE_EVERY: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub user_id: String,
    pub login: String,
    pub access: String,
    pub refresh: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Validation {
    pub user_id: String,
    pub login: String,
    pub scopes: Vec<String>,
    pub expires_in: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// No usable token: the account has to be authorized (again) in a browser.
    NeedsAuthorization(String),
    /// Twitch refused something for a reason a retry won't fix.
    Rejected(String),
    /// Couldn't reach Twitch; try again later.
    Network(String),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::NeedsAuthorization(m) => write!(f, "authorization needed: {m}"),
            AuthError::Rejected(m) => write!(f, "Twitch rejected the request: {m}"),
            AuthError::Network(m) => write!(f, "cannot reach Twitch: {m}"),
        }
    }
}

impl std::error::Error for AuthError {}

fn net(e: HttpError) -> AuthError {
    AuthError::Network(e.to_string())
}

pub struct Auth {
    creds: Credentials,
    ep: Endpoints,
    http: Arc<Client>,
    store: JsonStore,
    /// When each user's token was last confirmed valid.
    validated: Mutex<HashMap<String, Instant>>,
    /// One refresh at a time: refresh tokens rotate, so two at once would lose one.
    refresh_lock: Mutex<()>,
}

impl Auth {
    pub fn new(creds: Credentials, ep: Endpoints, http: Arc<Client>, token_file: impl Into<std::path::PathBuf>) -> Auth {
        Auth { creds, ep, http, store: JsonStore::new(token_file).private(), validated: Mutex::default(), refresh_lock: Mutex::default() }
    }

    pub fn credentials(&self) -> &Credentials {
        &self.creds
    }

    pub fn endpoints(&self) -> &Endpoints {
        &self.ep
    }

    // ------------------------------------------------------------ storage

    fn parse_entry(user_id: &str, v: &Value) -> Option<Token> {
        let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Some(Token {
            user_id: user_id.to_string(),
            login: text("login").unwrap_or_default(),
            access: text("token").filter(|t| !t.is_empty())?,
            refresh: text("refresh").unwrap_or_default(),
            scopes: v.get("scopes").and_then(Value::as_array).map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        })
    }

    pub fn saved(&self, user_id: &str) -> Option<Token> {
        self.store.read().get(user_id).and_then(|v| Self::parse_entry(user_id, v))
    }

    /// Deletes an account's saved token (e.g. someone authorized the wrong account).
    pub fn forget(&self, user_id: &str) {
        let _ = self.store.update(|mut m| m.remove(user_id).map(|_| m));
        self.validated.lock().unwrap_or_else(|e| e.into_inner()).remove(user_id);
    }

    /// Every account with a saved token.
    pub fn saved_accounts(&self) -> Vec<Token> {
        let mut all: Vec<Token> = self.store.read().iter().filter_map(|(id, v)| Self::parse_entry(id, v)).collect();
        all.sort_by(|a, b| a.user_id.cmp(&b.user_id));
        all
    }

    fn save(&self, t: &Token) -> Result<(), AuthError> {
        let entry = json!({"token": t.access, "refresh": t.refresh, "login": t.login, "scopes": t.scopes});
        self.store
            .update(|mut m: JsonMap| {
                m.insert(t.user_id.clone(), entry);
                Some(m)
            })
            .map(|_| ())
            .map_err(|e| AuthError::Rejected(format!("cannot save the token file: {e}")))
    }

    // -------------------------------------------------------- first-time flow

    pub fn authorize_url(&self, redirect_uri: &str, scopes: &[&str], state: &str, force_verify: bool) -> String {
        format!(
            "{}/oauth2/authorize?client_id={}&redirect_uri={}&response_type=code&scope={}&state={}{}",
            self.ep.id,
            percent_encode(&self.creds.client_id),
            percent_encode(redirect_uri),
            percent_encode(&scopes.join(" ")),
            percent_encode(state),
            if force_verify { "&force_verify=true" } else { "" }
        )
    }

    fn token_request(&self, pairs: &[(&str, &str)]) -> Result<Value, AuthError> {
        let mut all = vec![("client_id", self.creds.client_id.as_str()), ("client_secret", self.creds.client_secret.as_str())];
        all.extend_from_slice(pairs);
        let req = Request::post(format!("{}/oauth2/token", self.ep.id), form_encode(&all).into_bytes()).header("Content-Type", "application/x-www-form-urlencoded");
        let resp = self.http.send(&req).map_err(net)?;
        let status = resp.status;
        let body: Value = serde_json::from_slice(&resp.bytes(64 * 1024).map_err(net)?).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            return Ok(body);
        }
        let message = body.get("message").and_then(Value::as_str).unwrap_or("no details").to_string();
        match status {
            400 | 401 => Err(AuthError::NeedsAuthorization(message)),
            429 | 500..=599 => Err(AuthError::Network(format!("HTTP {status}: {message}"))),
            _ => Err(AuthError::Rejected(format!("HTTP {status}: {message}"))),
        }
    }

    /// Trades the `code` Twitch redirected back with for tokens, and saves them under the account they belong to.
    pub fn exchange_code(&self, code: &str, redirect_uri: &str) -> Result<Token, AuthError> {
        let body = self.token_request(&[("code", code), ("grant_type", "authorization_code"), ("redirect_uri", redirect_uri)])?;
        let access = body.get("access_token").and_then(Value::as_str).ok_or_else(|| AuthError::Rejected("no access token in the response".into()))?;
        let refresh = body.get("refresh_token").and_then(Value::as_str).unwrap_or_default();
        let v = self.validate(access)?;
        let token = Token { user_id: v.user_id, login: v.login, access: access.to_string(), refresh: refresh.to_string(), scopes: v.scopes };
        self.save(&token)?;
        self.validated.lock().unwrap_or_else(|e| e.into_inner()).insert(token.user_id.clone(), Instant::now());
        Ok(token)
    }

    // -------------------------------------------------------------- upkeep

    pub fn validate(&self, access: &str) -> Result<Validation, AuthError> {
        let req = Request::get(format!("{}/oauth2/validate", self.ep.id)).header("Authorization", &format!("OAuth {access}"));
        let resp = self.http.send(&req).map_err(net)?;
        let status = resp.status;
        let body: Value = serde_json::from_slice(&resp.bytes(64 * 1024).map_err(net)?).unwrap_or(Value::Null);
        match status {
            200 => {}
            401 => return Err(AuthError::NeedsAuthorization("the access token is invalid or expired".into())),
            429 | 500..=599 => return Err(AuthError::Network(format!("HTTP {status}"))),
            _ => return Err(AuthError::Rejected(format!("validate returned HTTP {status}"))),
        }
        if body.get("client_id").and_then(Value::as_str) != Some(self.creds.client_id.as_str()) {
            return Err(AuthError::NeedsAuthorization("this token belongs to a different Twitch application".into()));
        }
        let text = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        Ok(Validation {
            user_id: text("user_id"),
            login: text("login"),
            scopes: body.get("scopes").and_then(Value::as_array).map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default(),
            expires_in: body.get("expires_in").and_then(Value::as_u64).unwrap_or(0),
        })
    }

    /// Gets a new access token with the refresh token. Twitch rotates refresh tokens, so the result is saved at once.
    pub fn refresh(&self, old: &Token) -> Result<Token, AuthError> {
        let _one_at_a_time = self.refresh_lock.lock().unwrap_or_else(|e| e.into_inner());
        // Someone else may have refreshed while we waited for the lock.
        if let Some(current) = self.saved(&old.user_id) {
            if current.access != old.access {
                return Ok(current);
            }
        }
        if old.refresh.is_empty() {
            return Err(AuthError::NeedsAuthorization("no refresh token saved".into()));
        }
        let body = self.token_request(&[("grant_type", "refresh_token"), ("refresh_token", &old.refresh)])?;
        let access = body.get("access_token").and_then(Value::as_str).ok_or_else(|| AuthError::Rejected("no access token in the refresh response".into()))?;
        let refresh = body.get("refresh_token").and_then(Value::as_str).unwrap_or(&old.refresh);
        let mut token = Token { access: access.to_string(), refresh: refresh.to_string(), ..old.clone() };
        if let Ok(v) = self.validate(access) {
            token.login = v.login;
            token.scopes = v.scopes;
        }
        self.save(&token)?;
        self.validated.lock().unwrap_or_else(|e| e.into_inner()).insert(token.user_id.clone(), Instant::now());
        Ok(token)
    }

    /// A token for this account that Twitch currently accepts, refreshing it if need be.
    pub fn usable_token(&self, user_id: &str) -> Result<Token, AuthError> {
        let token = self.saved(user_id).ok_or_else(|| AuthError::NeedsAuthorization(format!("no token saved for Twitch user {user_id}")))?;
        let fresh_enough = self.validated.lock().unwrap_or_else(|e| e.into_inner()).get(user_id).is_some_and(|t| t.elapsed() < VALIDATE_EVERY);
        if fresh_enough {
            return Ok(token);
        }
        match self.validate(&token.access) {
            Ok(v) if v.user_id == user_id && v.expires_in > REFRESH_MARGIN_SECS => {
                self.validated.lock().unwrap_or_else(|e| e.into_inner()).insert(user_id.to_string(), Instant::now());
                Ok(token)
            }
            Ok(v) if v.user_id != user_id => Err(AuthError::NeedsAuthorization(format!("the token saved for {user_id} belongs to {} ({}); authorize the right account", v.login, v.user_id))),
            Ok(_) | Err(AuthError::NeedsAuthorization(_)) => self.refresh(&token),
            Err(e) => Err(e),
        }
    }

    /// Called when Twitch answered 401 to a token we thought was fine.
    pub fn force_refresh(&self, user_id: &str) -> Result<Token, AuthError> {
        self.validated.lock().unwrap_or_else(|e| e.into_inner()).remove(user_id);
        let token = self.saved(user_id).ok_or_else(|| AuthError::NeedsAuthorization(format!("no token saved for Twitch user {user_id}")))?;
        self.refresh(&token)
    }

    /// What a status page can say about the two accounts, without touching the network.
    pub fn account_summary(&self, bot_id: &str, owner_id: &str) -> Vec<AccountStatus> {
        [("bot", bot_id), ("broadcaster", owner_id)]
            .into_iter()
            .map(|(role, id)| {
                let t = self.saved(id);
                AccountStatus { role, user_id: id.to_string(), login: t.as_ref().map(|t| t.login.clone()), authorized: t.is_some() }
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountStatus {
    pub role: &'static str,
    pub user_id: String,
    pub login: Option<String>,
    pub authorized: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(dir: &str) -> Auth {
        let path = std::env::temp_dir().join(format!("pryxea-auth-{dir}-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Auth::new(Credentials { client_id: "cid".into(), client_secret: "sec".into() }, Endpoints::default(), Arc::new(Client::new()), path)
    }

    #[test]
    fn the_authorize_url_carries_everything_twitch_needs() {
        let url = auth("url").authorize_url("http://localhost:4343/oauth/callback", &["user:read:chat", "user:write:chat"], "st4te", true);
        assert!(url.starts_with("https://id.twitch.tv/oauth2/authorize?client_id=cid&"), "{url}");
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A4343%2Foauth%2Fcallback"));
        assert!(url.contains("response_type=code") && url.contains("scope=user%3Aread%3Achat%20user%3Awrite%3Achat"));
        assert!(url.contains("state=st4te") && url.ends_with("&force_verify=true"));
        assert!(!auth("url2").authorize_url("http://x/", &["a"], "s", false).contains("force_verify"));
    }

    #[test]
    fn a_token_file_written_by_the_python_bot_is_readable() {
        let a = auth("compat");
        std::fs::write(a.store.path(), r#"{"111": {"token": "abc", "refresh": "def"}, "222": {"token": "", "refresh": "x"}, "333": "junk"}"#).unwrap();
        let t = a.saved("111").unwrap();
        assert_eq!((t.access.as_str(), t.refresh.as_str(), t.login.as_str()), ("abc", "def", ""));
        assert!(a.saved("222").is_none(), "an empty token is no token");
        assert!(a.saved("333").is_none() && a.saved("444").is_none());
        assert_eq!(a.saved_accounts().len(), 1);
    }

    #[test]
    fn saving_keeps_the_other_accounts_and_the_python_compatible_keys() {
        let a = auth("save");
        a.save(&Token { user_id: "1".into(), login: "bot".into(), access: "a1".into(), refresh: "r1".into(), scopes: vec!["user:read:chat".into()] }).unwrap();
        a.save(&Token { user_id: "2".into(), login: "streamer".into(), access: "a2".into(), refresh: "r2".into(), scopes: vec![] }).unwrap();
        let raw: Value = serde_json::from_slice(&std::fs::read(a.store.path()).unwrap()).unwrap();
        assert_eq!(raw["1"]["token"], "a1");
        assert_eq!(raw["2"]["refresh"], "r2");
        assert_eq!(a.account_summary("1", "2").iter().map(|s| s.authorized).collect::<Vec<_>>(), [true, true]);
        assert_eq!(a.account_summary("1", "9")[1], AccountStatus { role: "broadcaster", user_id: "9".into(), login: None, authorized: false });
    }
}
