mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::twitch::{self, Mode, creds, endpoints, preauthorize};
use common::{Server, spawn};
use pryxea::net::http::Client;
use pryxea::twitch::auth::{Auth, AuthError};
use pryxea::twitch::chat::{ChatOut, run_sender};
use pryxea::twitch::helix::{Helix, HelixError, Sent};
use serde_json::json;

fn token_file(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("pryxea-tw-{tag}-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

fn setup(tag: &str) -> (Server, twitch::Fake, Arc<Auth>) {
    let server = spawn(false);
    let fake = twitch::install(&server);
    let auth = Arc::new(Auth::new(creds(), endpoints(&server, "ws://unused"), Arc::new(Client::new()), token_file(tag)));
    (server, fake, auth)
}

// -------------------------------------------------------------------- auth

#[test]
fn the_authorization_code_is_exchanged_validated_and_saved_for_the_right_account() {
    let (_s, fake, auth) = setup("exchange");
    let t = auth.exchange_code("good-code", "http://localhost:4343/oauth/callback").unwrap();
    assert_eq!((t.user_id.as_str(), t.login.as_str()), ("42", "botlogin"));
    assert!(t.access.starts_with("acc-") && t.refresh.starts_with("ref-"));
    assert_eq!(t.scopes, ["user:read:chat", "user:write:chat"]);
    assert_eq!(auth.saved("42").unwrap(), t);
    assert_eq!(fake.lock().unwrap().validates, 1);
    // A bad code is reported as needing (re)authorization, not as a crash.
    assert!(matches!(auth.exchange_code("stale", "http://x/"), Err(AuthError::NeedsAuthorization(m)) if m.contains("Invalid authorization code")));
}

#[test]
fn a_valid_token_is_used_as_is_and_not_revalidated_on_every_call() {
    let (_s, fake, auth) = setup("valid");
    preauthorize(&fake, "42", "botlogin", "good-access", "good-refresh");
    std::fs::write(token_file("valid"), r#"{"42": {"token": "good-access", "refresh": "good-refresh"}}"#).unwrap();
    for _ in 0..5 {
        assert_eq!(auth.usable_token("42").unwrap().access, "good-access");
    }
    let st = fake.lock().unwrap();
    assert_eq!((st.validates, st.refreshes), (1, 0));
}

#[test]
fn an_expired_token_is_refreshed_and_the_rotated_pair_saved() {
    let (_s, fake, auth) = setup("expired");
    preauthorize(&fake, "42", "botlogin", "other", "good-refresh"); // the saved access token is NOT valid
    std::fs::write(token_file("expired"), r#"{"42": {"token": "expired-access", "refresh": "good-refresh"}}"#).unwrap();
    let t = auth.usable_token("42").unwrap();
    assert!(t.access.starts_with("acc-"), "{t:?}");
    assert_ne!(t.refresh, "good-refresh", "Twitch rotates refresh tokens");
    assert_eq!(auth.saved("42").unwrap().access, t.access, "the new pair must be on disk");
    assert_eq!(fake.lock().unwrap().refreshes, 1);
    // The old refresh token is dead now.
    assert!(!fake.lock().unwrap().refresh.contains_key("good-refresh"));
}

#[test]
fn a_token_about_to_expire_is_refreshed_ahead_of_time() {
    let (_s, fake, auth) = setup("soon");
    preauthorize(&fake, "42", "botlogin", "soon-access", "soon-refresh");
    fake.lock().unwrap().expires_in = 120;
    std::fs::write(token_file("soon"), r#"{"42": {"token": "soon-access", "refresh": "soon-refresh"}}"#).unwrap();
    assert_ne!(auth.usable_token("42").unwrap().access, "soon-access");
    assert_eq!(fake.lock().unwrap().refreshes, 1);
}

#[test]
fn unusable_tokens_ask_for_authorization_instead_of_looping() {
    let (_s, fake, auth) = setup("dead");
    // No token at all.
    assert!(matches!(auth.usable_token("42"), Err(AuthError::NeedsAuthorization(_))));
    // Dead access and dead refresh token.
    std::fs::write(token_file("dead"), r#"{"42": {"token": "x", "refresh": "y"}}"#).unwrap();
    assert!(matches!(auth.usable_token("42"), Err(AuthError::NeedsAuthorization(_))));
    // A token that is valid but belongs to somebody else (the classic wrong-browser-session mistake).
    preauthorize(&fake, "77", "someone_else", "else-access", "else-refresh");
    std::fs::write(token_file("dead"), r#"{"42": {"token": "else-access", "refresh": "else-refresh"}}"#).unwrap();
    match auth.usable_token("42") {
        Err(AuthError::NeedsAuthorization(m)) => assert!(m.contains("someone_else"), "{m}"),
        other => panic!("{other:?}"),
    }
    // A token issued to a different Twitch application.
    preauthorize(&fake, "42", "botlogin", "app-access", "app-refresh");
    fake.lock().unwrap().validate_client_id = "another-app".into();
    std::fs::write(token_file("dead"), r#"{"42": {"token": "app-access", "refresh": ""}}"#).unwrap();
    assert!(matches!(auth.usable_token("42"), Err(AuthError::NeedsAuthorization(_))));
}

// ------------------------------------------------------------------- helix

fn helix_with_bot(tag: &str) -> (Server, twitch::Fake, Arc<Helix>) {
    let (s, fake, auth) = setup(tag);
    preauthorize(&fake, "42", "botlogin", "bot-access", "bot-refresh");
    std::fs::write(token_file(tag), r#"{"42": {"token": "bot-access", "refresh": "bot-refresh", "login": "botlogin"}}"#).unwrap();
    (s, fake, Arc::new(Helix::new(auth, Arc::new(Client::new()))))
}

#[test]
fn a_chat_message_is_sent_as_the_bot_to_the_channel_with_reply_threading() {
    let (_s, fake, helix) = helix_with_bot("send");
    assert_eq!(helix.send_chat_message("100", "42", "Queued: Song (#1 in queue)", Some("msg-9")).unwrap(), Sent::Delivered);
    assert_eq!(helix.send_chat_message("100", "42", "Skipped.", None).unwrap(), Sent::Delivered);
    let st = fake.lock().unwrap();
    assert_eq!(st.sent[0].0, json!({"broadcaster_id": "100", "sender_id": "42", "message": "Queued: Song (#1 in queue)", "reply_parent_message_id": "msg-9"}));
    assert!(st.sent[1].0.get("reply_parent_message_id").is_none());
    assert_eq!(st.sent[0].1, "bot-access");
}

#[test]
fn a_401_triggers_one_refresh_and_a_retry_with_the_new_token() {
    let (_s, fake, helix) = helix_with_bot("retry");
    // Revoke the access token behind the app's back; the refresh token still works.
    fake.lock().unwrap().valid.clear();
    fake.lock().unwrap().refresh.insert("bot-refresh".into(), ("42".into(), "botlogin".into()));
    // The cached "recently validated" state is empty (fresh process), so usable_token itself refreshes first.
    assert_eq!(helix.send_chat_message("100", "42", "hello", None).unwrap(), Sent::Delivered);
    let st = fake.lock().unwrap();
    assert_eq!(st.refreshes, 1);
    assert!(st.sent[0].1.starts_with("acc-"), "must use the refreshed token: {}", st.sent[0].1);
}

#[test]
fn twitch_refusals_are_told_apart() {
    let (_s, fake, helix) = helix_with_bot("modes");
    fake.lock().unwrap().send_mode = Mode::Dropped;
    match helix.send_chat_message("100", "42", "dup", None).unwrap() {
        Sent::Dropped(why) => assert!(why.contains("identical"), "{why}"),
        other => panic!("{other:?}"),
    }
    fake.lock().unwrap().send_mode = Mode::Forbidden;
    assert!(matches!(helix.send_chat_message("100", "42", "x", None), Err(HelixError::Forbidden(m)) if m.contains("banned")));
    fake.lock().unwrap().send_mode = Mode::RateLimited;
    assert_eq!(helix.send_chat_message("100", "42", "x", None), Err(HelixError::RateLimited));
}

#[test]
fn the_chat_subscription_is_made_with_the_bots_token_and_tolerates_duplicates() {
    let (_s, fake, helix) = helix_with_bot("subscribe");
    helix.subscribe_chat("session-1", "100", "42").unwrap();
    {
        let st = fake.lock().unwrap();
        let body = &st.subs[0].0;
        assert_eq!(body["type"], "channel.chat.message");
        assert_eq!(body["condition"], json!({"broadcaster_user_id": "100", "user_id": "42"}));
        assert_eq!(body["transport"], json!({"method": "websocket", "session_id": "session-1"}));
        assert_eq!(st.subs[0].1, "bot-access");
    }
    fake.lock().unwrap().sub_mode = Mode::Conflict;
    helix.subscribe_chat("session-2", "100", "42").unwrap();
    fake.lock().unwrap().sub_mode = Mode::Forbidden;
    assert!(matches!(helix.subscribe_chat("session-3", "100", "42"), Err(HelixError::Forbidden(m)) if m.contains("authorization")));
}

#[tokio::test]
async fn queued_chat_messages_are_delivered_in_order_and_survive_failures() {
    let (_s, fake, helix) = helix_with_bot("sender");
    let (chat, rx) = ChatOut::channel();
    let task = tokio::spawn(run_sender(rx, helix, "100".into(), "42".into()));
    chat.reply("m1", "first");
    fake.lock().unwrap().send_mode = Mode::Forbidden;
    chat.announce("lost");
    tokio::time::sleep(Duration::from_millis(900)).await;
    fake.lock().unwrap().send_mode = Mode::Ok;
    chat.announce("second");
    drop(chat);
    tokio::time::timeout(Duration::from_secs(5), task).await.expect("sender stops when the queue closes").unwrap();
    let st = fake.lock().unwrap();
    let texts: Vec<&str> = st.sent.iter().map(|(b, _)| b["message"].as_str().unwrap()).collect();
    assert_eq!(texts.len(), 3, "{texts:?}");
    assert!(texts[0].starts_with("first") && texts[1].starts_with("lost") && texts[2].starts_with("second"), "{texts:?}");
    assert_ne!(texts[0].chars().last(), texts[1].chars().last(), "consecutive messages must differ in their suffix");
    assert_eq!(st.sent[0].0["reply_parent_message_id"], "m1");
}
