//! A scriptable fake of Twitch's OAuth and Helix endpoints.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pryxea::twitch::Endpoints;
use pryxea::twitch::auth::Credentials;
use serde_json::{Value, json};

use super::{Resp, Server};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Ok,
    /// 200 but `is_sent: false`.
    Dropped,
    Forbidden,
    RateLimited,
    /// 409 on subscribe (already exists).
    Conflict,
}

pub struct State {
    /// access token -> (user id, login)
    pub valid: HashMap<String, (String, String)>,
    /// refresh token -> (user id, login)
    pub refresh: HashMap<String, (String, String)>,
    pub issued: u32,
    pub validates: u32,
    pub refreshes: u32,
    pub expires_in: u64,
    pub validate_client_id: String,
    pub send_mode: Mode,
    pub sub_mode: Mode,
    pub sent: Vec<(Value, String)>,
    pub subs: Vec<(Value, String)>,
    /// The next user a valid authorization code belongs to.
    pub code_user: (String, String),
}

pub type Fake = Arc<Mutex<State>>;

pub fn creds() -> Credentials {
    Credentials { client_id: "cid".into(), client_secret: "sec".into() }
}

pub fn endpoints(server: &Server, eventsub: &str) -> Endpoints {
    Endpoints { id: server.url("/id"), api: server.url("/api"), eventsub: eventsub.to_string() }
}

fn issue(st: &mut State, user: (String, String)) -> Value {
    st.issued += 1;
    let (access, refresh) = (format!("acc-{}", st.issued), format!("ref-{}", st.issued));
    st.valid.insert(access.clone(), user.clone());
    st.refresh.insert(refresh.clone(), user);
    json!({"access_token": access, "refresh_token": refresh, "expires_in": 14000, "token_type": "bearer", "scope": ["user:read:chat"]})
}

pub fn install(server: &Server) -> Fake {
    let st: Fake = Arc::new(Mutex::new(State {
        valid: HashMap::new(),
        refresh: HashMap::new(),
        issued: 0,
        validates: 0,
        refreshes: 0,
        expires_in: 14_000,
        validate_client_id: "cid".into(),
        send_mode: Mode::Ok,
        sub_mode: Mode::Ok,
        sent: vec![],
        subs: vec![],
        code_user: ("42".into(), "botlogin".into()),
    }));

    let s = st.clone();
    server.on("POST", "/id/oauth2/token", move |req| {
        let mut st = s.lock().unwrap();
        let get = |k: &str| req.form_value(k).unwrap_or_default();
        if get("client_id") != "cid" || get("client_secret") != "sec" {
            return Resp::json(401, json!({"status": 401, "message": "invalid client secret"}));
        }
        match get("grant_type").as_str() {
            "authorization_code" if get("code") == "good-code" => {
                let user = st.code_user.clone();
                Resp::json(200, issue(&mut st, user))
            }
            "authorization_code" => Resp::json(400, json!({"status": 400, "message": "Invalid authorization code"})),
            "refresh_token" => {
                st.refreshes += 1;
                match st.refresh.remove(&get("refresh_token")) {
                    Some(user) => Resp::json(200, issue(&mut st, user)),
                    None => Resp::json(400, json!({"status": 400, "message": "Invalid refresh token"})),
                }
            }
            _ => Resp::json(400, json!({"message": "bad grant"})),
        }
    });

    let s = st.clone();
    server.on("GET", "/id/oauth2/validate", move |req| {
        let mut st = s.lock().unwrap();
        st.validates += 1;
        let token = req.header("authorization").unwrap_or("").trim_start_matches("OAuth ").to_string();
        match st.valid.get(&token) {
            Some((id, login)) => Resp::json(200, json!({"client_id": st.validate_client_id, "login": login, "user_id": id, "scopes": ["user:read:chat", "user:write:chat"], "expires_in": st.expires_in})),
            None => Resp::json(401, json!({"status": 401, "message": "invalid access token"})),
        }
    });

    let s = st.clone();
    server.on("POST", "/api/chat/messages", move |req| {
        let mut st = s.lock().unwrap();
        let token = req.header("authorization").unwrap_or("").trim_start_matches("Bearer ").to_string();
        if !st.valid.contains_key(&token) || req.header("client-id") != Some("cid") {
            return Resp::json(401, json!({"status": 401, "message": "Invalid OAuth token"}));
        }
        st.sent.push((req.json(), token));
        match st.send_mode {
            Mode::Ok => Resp::json(200, json!({"data": [{"message_id": "sent-1", "is_sent": true}]})),
            Mode::Dropped => Resp::json(200, json!({"data": [{"message_id": "", "is_sent": false, "drop_reason": {"code": "msg_duplicate", "message": "Your message was not sent because it is identical to the previous one you sent"}}]})),
            Mode::Forbidden => Resp::json(403, json!({"status": 403, "message": "The user is banned"})),
            _ => Resp::json(429, json!({"status": 429, "message": "slow down"})),
        }
    });

    let s = st.clone();
    server.on("POST", "/api/eventsub/subscriptions", move |req| {
        let mut st = s.lock().unwrap();
        let token = req.header("authorization").unwrap_or("").trim_start_matches("Bearer ").to_string();
        if !st.valid.contains_key(&token) {
            return Resp::json(401, json!({"status": 401, "message": "Invalid OAuth token"}));
        }
        st.subs.push((req.json(), token));
        match st.sub_mode {
            Mode::Ok => Resp::json(202, json!({"data": [{"id": "sub-1"}]})),
            Mode::Conflict => Resp::json(409, json!({"status": 409, "message": "subscription already exists"})),
            Mode::Forbidden => Resp::json(403, json!({"status": 403, "message": "subscription missing proper authorization"})),
            _ => Resp::json(429, json!({})),
        }
    });
    st
}

/// Registers an already-issued token pair as valid, as if the user had authorized earlier.
pub fn preauthorize(fake: &Fake, user_id: &str, login: &str, access: &str, refresh: &str) {
    let mut st = fake.lock().unwrap();
    st.valid.insert(access.into(), (user_id.into(), login.into()));
    st.refresh.insert(refresh.into(), (user_id.into(), login.into()));
}
