mod common;

use std::sync::Arc;

use common::twitch::{self, creds, endpoints};
use common::{Server, spawn};
use pryxea::net::http::Client;
use pryxea::net::url::parse_query;
use pryxea::setup::Setup;
use pryxea::twitch::auth::Auth;
use pryxea::twitch::eventsub::Link;
use tokio::sync::watch;

const REDIRECT: &str = "http://localhost:4343/oauth/callback";

fn setup(tag: &str) -> (Server, twitch::Fake, Arc<Auth>, Setup, watch::Sender<Link>) {
    let server = spawn(false);
    let fake = twitch::install(&server);
    let path = std::env::temp_dir().join(format!("pryxea-setup-{tag}-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let auth = Arc::new(Auth::new(creds(), endpoints(&server, "ws://unused"), Arc::new(Client::new()), path));
    let (tx, rx) = watch::channel(Link::Connecting);
    // The fake hands out tokens for the "bot" account (id 42) unless told otherwise.
    let s = Setup::new(Some(auth.clone()), REDIRECT.into(), "42".into(), "100".into(), 8098, rx, vec![]);
    (server, fake, auth, s, tx)
}

fn state_of(url: &str) -> String {
    let query = url.split_once('?').unwrap().1;
    parse_query(query).into_iter().find(|(k, _)| k == "state").unwrap().1
}

#[test]
fn authorizing_the_bot_account_saves_its_token_and_the_page_reflects_it() {
    let (_s, _fake, auth, setup, link) = setup("bot");
    assert!(setup.status_page().contains("not authorized yet"));
    let url = setup.start("bot").unwrap();
    assert!(url.contains("scope=user%3Aread%3Achat%20user%3Awrite%3Achat%20user%3Abot") && url.contains("force_verify=true"), "{url}");
    assert_eq!(setup.pending(), 1);
    let (status, html) = setup.callback(&format!("code=good-code&state={}&scope=x", state_of(&url)));
    assert_eq!(status, 200, "{html}");
    assert!(html.contains("Authorized botlogin"), "{html}");
    assert_eq!(auth.saved("42").unwrap().login, "botlogin");
    assert_eq!(setup.pending(), 0, "a state is single-use");
    link.send_replace(Link::Live);
    let page = setup.status_page();
    assert!(page.contains("authorized as botlogin") && page.contains("Connected: chat commands are live."), "{page}");
    assert!(page.contains(REDIRECT));
}

#[test]
fn the_broadcaster_flow_asks_only_for_the_channel_bot_scope() {
    let (_s, fake, _auth, setup, _link) = setup("broadcaster");
    fake.lock().unwrap().code_user = ("100".into(), "streamer".into());
    let url = setup.start("broadcaster").unwrap();
    assert!(url.contains("scope=channel%3Abot&") || url.contains("scope=channel%3Abot"), "{url}");
    let (status, html) = setup.callback(&format!("code=good-code&state={}", state_of(&url)));
    assert_eq!(status, 200, "{html}");
}

#[test]
fn a_stale_forged_or_replayed_state_is_refused() {
    let (_s, fake, auth, setup, _link) = setup("states");
    for q in ["code=good-code&state=forged", "code=good-code", "state=abc", ""] {
        let (status, html) = setup.callback(q);
        assert_eq!(status, 400, "{q}: {html}");
        assert_eq!(fake.lock().unwrap().issued, 0, "no tokens may be requested for {q:?}");
    }
    let url = setup.start("bot").unwrap();
    let state = state_of(&url);
    assert_eq!(setup.callback(&format!("code=good-code&state={state}")).0, 200);
    assert_eq!(setup.callback(&format!("code=good-code&state={state}")).0, 400, "replaying a used state must fail");
    assert!(auth.saved("42").is_some());
    assert_eq!(setup.start("nobody").unwrap_err().0, 400);
}

#[test]
fn an_error_from_twitch_is_shown_without_trying_to_exchange_anything() {
    let (_s, fake, _auth, setup, _link) = setup("denied");
    let url = setup.start("bot").unwrap();
    let (status, html) = setup.callback(&format!("error=access_denied&error_description=The+user+denied+you+access&state={}", state_of(&url)));
    assert_eq!(status, 400);
    assert!(html.contains("access_denied") && html.contains("The user denied you access"), "{html}");
    assert_eq!(fake.lock().unwrap().issued, 0);
}

#[test]
fn authorizing_the_wrong_account_is_caught_and_its_token_is_not_kept() {
    let (_s, fake, auth, setup, _link) = setup("wrong");
    fake.lock().unwrap().code_user = ("555".into(), "somebody_else".into());
    let url = setup.start("bot").unwrap();
    let (status, html) = setup.callback(&format!("code=good-code&state={}", state_of(&url)));
    assert_eq!(status, 400);
    assert!(html.contains("somebody_else") && html.contains("42"), "{html}");
    assert!(auth.saved("555").is_none(), "a token for an account we don't use must not be kept");
    assert!(auth.saved("42").is_none());
}

#[test]
fn too_many_pending_authorizations_are_refused() {
    let (_s, _fake, _auth, setup, _link) = setup("flood");
    for _ in 0..20 {
        setup.start("bot").unwrap();
    }
    assert_eq!(setup.start("bot").unwrap_err().0, 429);
}
