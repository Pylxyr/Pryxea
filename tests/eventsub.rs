mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::twitch::{self, Mode, creds, endpoints, preauthorize};
use common::{Server, spawn, tls_configs};
use futures_util::{SinkExt, StreamExt};
use pryxea::net::http::Client;
use pryxea::twitch::auth::Auth;
use pryxea::twitch::eventsub::{self, ChatMessage, Config, Link};
use pryxea::twitch::helix::Helix;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

fn welcome(session: &str) -> Message {
    Message::text(json!({"metadata": {"message_id": format!("w-{session}"), "message_type": "session_welcome"}, "payload": {"session": {"id": session, "keepalive_timeout_seconds": 10}}}).to_string())
}

fn chat(id: &str, user: &str, text: &str, badge: &str) -> Message {
    Message::text(
        json!({"metadata": {"message_id": id, "message_type": "notification", "subscription_type": "channel.chat.message"},
               "payload": {"subscription": {"type": "channel.chat.message"},
                           "event": {"broadcaster_user_id": "100", "chatter_user_id": user, "chatter_user_login": "login", "chatter_user_name": "Name",
                                     "message_id": format!("chat-{id}"), "message": {"text": text}, "badges": [{"set_id": badge}]}}})
        .to_string(),
    )
}

fn keepalive() -> Message {
    Message::text(json!({"metadata": {"message_id": "k", "message_type": "session_keepalive"}, "payload": {}}).to_string())
}

fn reconnect(url: &str) -> Message {
    Message::text(json!({"metadata": {"message_id": "r", "message_type": "session_reconnect"}, "payload": {"session": {"id": "sess-0", "reconnect_url": url}}}).to_string())
}

type Script = Arc<dyn Fn(usize, tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> futures_util::future::BoxFuture<'static, ()> + Send + Sync>;

/// A fake EventSub server: each accepted connection runs `script(index, websocket)`.
async fn fake_eventsub(script: Script) -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let mut index = 0;
        while let Ok((tcp, _)) = listener.accept().await {
            if let Ok(ws) = tokio_tungstenite::accept_async(tcp).await {
                tokio::spawn(script(index, ws));
            }
            index += 1;
        }
    });
    (port, task)
}

struct Rig {
    server: Server,
    fake: twitch::Fake,
    rx: UnboundedReceiver<ChatMessage>,
    link: watch::Receiver<Link>,
    task: JoinHandle<()>,
}

fn start(tag: &str, url: String, tls: Arc<rustls::ClientConfig>) -> Rig {
    let server = spawn(false);
    let fake = twitch::install(&server);
    preauthorize(&fake, "42", "botlogin", "bot-access", "bot-refresh");
    let path: PathBuf = std::env::temp_dir().join(format!("pryxea-es-{tag}-{}.json", std::process::id()));
    std::fs::write(&path, r#"{"42": {"token": "bot-access", "refresh": "bot-refresh"}}"#).unwrap();
    let http = Arc::new(Client::new());
    let helix = Arc::new(Helix::new(Arc::new(Auth::new(creds(), endpoints(&server, &url), http, path)), Arc::new(Client::new())));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let (link_tx, link) = watch::channel(Link::Connecting);
    let cfg = Config { url, broadcaster_id: "100".into(), bot_id: "42".into() };
    let task = tokio::spawn(eventsub::run(cfg, helix, tls, tx, link_tx));
    Rig { server, fake, rx, link, task }
}

async fn next_chat(rx: &mut UnboundedReceiver<ChatMessage>) -> ChatMessage {
    tokio::time::timeout(Duration::from_secs(8), rx.recv()).await.expect("a chat message").expect("channel open")
}

async fn wait_link(link: &mut watch::Receiver<Link>, mut pred: impl FnMut(&Link) -> bool) -> Link {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let current = link.borrow_and_update().clone();
            if pred(&current) {
                return current;
            }
            link.changed().await.unwrap();
        }
    })
    .await
    .expect("link state")
}

fn plain_tls() -> Arc<rustls::ClientConfig> {
    tls_configs().1
}

#[tokio::test]
async fn chat_flows_through_subscribe_dedupe_and_a_reconnect_handover() {
    let port_cell = Arc::new(std::sync::OnceLock::<u16>::new());
    let cell = port_cell.clone();
    let script: Script = Arc::new(move |index, mut ws| {
        let cell = cell.clone();
        Box::pin(async move {
            match index {
                0 => {
                    ws.send(welcome("sess-0")).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(300)).await; // let the client subscribe
                    ws.send(chat("n1", "7", "!sr never gonna give you up", "subscriber")).await.unwrap();
                    ws.send(chat("n1", "7", "!sr never gonna give you up", "subscriber")).await.unwrap(); // duplicate delivery
                    ws.send(keepalive()).await.unwrap();
                    ws.send(chat("n2", "8", "!skip", "moderator")).await.unwrap();
                    ws.send(reconnect(&format!("ws://127.0.0.1:{}/reconnect", cell.get().unwrap()))).await.unwrap();
                    let _ = ws.next().await; // wait for the client to leave
                }
                _ => {
                    ws.send(welcome("sess-1")).await.unwrap();
                    ws.send(chat("n3", "9", "!sq", "vip")).await.unwrap();
                    let _ = ws.next().await;
                }
            }
        })
    });
    let (port, server_task) = fake_eventsub(script).await;
    port_cell.set(port).unwrap();
    let mut rig = start("flow", format!("ws://127.0.0.1:{port}/ws"), plain_tls());

    let (a, b, c) = (next_chat(&mut rig.rx).await, next_chat(&mut rig.rx).await, next_chat(&mut rig.rx).await);
    assert_eq!((a.text.as_str(), a.chatter_id.as_str(), a.is_mod), ("!sr never gonna give you up", "7", false));
    assert_eq!((b.text.as_str(), b.is_mod), ("!skip", true), "the duplicate n1 must have been dropped, so n2 is next");
    assert_eq!((c.text.as_str(), c.chatter_id.as_str(), c.is_mod), ("!sq", "9", false));
    wait_link(&mut rig.link, |l| *l == Link::Live).await;

    // Exactly one subscription: the reconnect hand-over keeps it, so the client must not subscribe again.
    let st = rig.fake.lock().unwrap();
    assert_eq!(st.subs.len(), 1, "{:?}", st.subs.iter().map(|s| &s.0).collect::<Vec<_>>());
    assert_eq!(st.subs[0].0["transport"]["session_id"], "sess-0");
    drop(st);
    rig.task.abort();
    server_task.abort();
    let _ = &rig.server;
}

#[tokio::test]
async fn a_dropped_connection_is_re_established_and_re_subscribed() {
    let script: Script = Arc::new(|index, mut ws| {
        Box::pin(async move {
            ws.send(welcome(&format!("sess-{index}"))).await.unwrap();
            if index == 0 {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let _ = ws.close(None).await; // Twitch hangs up
            } else {
                ws.send(chat("n1", "7", "!sq", "subscriber")).await.unwrap();
                let _ = ws.next().await;
            }
        })
    });
    let (port, server_task) = fake_eventsub(script).await;
    let mut rig = start("drop", format!("ws://127.0.0.1:{port}/ws"), plain_tls());
    let m = next_chat(&mut rig.rx).await;
    assert_eq!(m.text, "!sq");
    let st = rig.fake.lock().unwrap();
    assert_eq!(st.subs.len(), 2, "a fresh connection is a fresh session and needs its own subscription");
    assert_eq!((st.subs[0].0["transport"]["session_id"].as_str(), st.subs[1].0["transport"]["session_id"].as_str()), (Some("sess-0"), Some("sess-1")));
    drop(st);
    rig.task.abort();
    server_task.abort();
}

#[tokio::test]
async fn a_forbidden_subscription_is_reported_with_what_to_do_instead_of_spinning() {
    let script: Script = Arc::new(|_, mut ws| {
        Box::pin(async move {
            ws.send(welcome("sess-x")).await.unwrap();
            let _ = ws.next().await;
        })
    });
    let (port, server_task) = fake_eventsub(script).await;
    let mut rig = start("forbidden", format!("ws://127.0.0.1:{port}/ws"), plain_tls());
    rig.fake.lock().unwrap().sub_mode = Mode::Forbidden;
    let link = wait_link(&mut rig.link, |l| matches!(l, Link::Waiting(_))).await;
    let Link::Waiting(why) = link else { unreachable!() };
    assert!(why.contains("moderator") && why.contains("broadcaster"), "{why}");
    rig.task.abort();
    server_task.abort();
}

#[tokio::test]
async fn a_missing_bot_token_waits_for_authorization() {
    let script: Script = Arc::new(|_, mut ws| {
        Box::pin(async move {
            ws.send(welcome("sess-y")).await.unwrap();
            let _ = ws.next().await;
        })
    });
    let (port, server_task) = fake_eventsub(script).await;
    let mut rig = start("noauth", format!("ws://127.0.0.1:{port}/ws"), plain_tls());
    rig.fake.lock().unwrap().valid.clear(); // every token is now unknown to Twitch, and refresh fails too
    rig.fake.lock().unwrap().refresh.clear();
    let Link::Waiting(why) = wait_link(&mut rig.link, |l| matches!(l, Link::Waiting(_))).await else { unreachable!() };
    assert!(why.contains("authorized") || why.contains("setup"), "{why}");
    rig.task.abort();
    server_task.abort();
}

#[tokio::test]
async fn the_websocket_works_over_tls_with_the_shared_trust_settings() {
    let (server_cfg, client_cfg) = tls_configs();
    let acceptor = tokio_rustls::TlsAcceptor::from(server_cfg);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server_task = tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let tls = acceptor.accept(tcp).await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(tls).await.unwrap();
                ws.send(welcome("tls-sess")).await.unwrap();
                tokio::time::sleep(Duration::from_millis(300)).await;
                ws.send(chat("n1", "5", "!nowplaying", "subscriber")).await.unwrap();
                let _ = ws.next().await;
            });
        }
    });
    let mut rig = start("tls", format!("wss://localhost:{port}/ws"), client_cfg);
    assert_eq!(next_chat(&mut rig.rx).await.text, "!nowplaying");
    rig.task.abort();
    server_task.abort();
}

#[tokio::test]
async fn an_untrusted_server_certificate_is_never_accepted() {
    let (server_cfg, _) = tls_configs();
    let acceptor = tokio_rustls::TlsAcceptor::from(server_cfg);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server_task = tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(tls) = acceptor.accept(tcp).await {
                    if let Ok(mut ws) = tokio_tungstenite::accept_async(tls).await {
                        let _ = ws.send(welcome("evil")).await;
                        let _ = ws.send(chat("n1", "5", "!skip", "moderator")).await;
                    }
                }
            });
        }
    });
    // Default client settings (OS trust store) do not trust the test CA.
    let system_tls = Client::new().tls_config().unwrap();
    let mut rig = start("untrusted", format!("wss://localhost:{port}/ws"), system_tls);
    let Link::Waiting(why) = wait_link(&mut rig.link, |l| matches!(l, Link::Waiting(_))).await else { unreachable!() };
    assert!(why.to_lowercase().contains("tls") || why.to_lowercase().contains("cert"), "{why}");
    assert!(tokio::time::timeout(Duration::from_millis(300), rig.rx.recv()).await.is_err(), "no chat may be accepted over an unverified connection");
    rig.task.abort();
    server_task.abort();
    let _: Value = json!(null);
}
