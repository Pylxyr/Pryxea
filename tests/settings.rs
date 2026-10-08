mod common;

use std::sync::Arc;
use std::time::Duration;

use common::rig::*;
use pryxea::http::{self, Ctx};
use pryxea::hub::StreamHub;
use pryxea::settings::SettingsPage;
use pryxea::state::Shared;
use pryxea::store::JsonStore;
use pryxea::toggles::Toggles;
use pryxea::tunables::Tunables;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct App {
    port: u16,
    tunables: Arc<JsonStore>,
    toggles: Arc<JsonStore>,
}

async fn start(tag: &str) -> App {
    let dir = std::env::temp_dir().join(format!("pryxea-settings-http-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (tunables, toggles) = (Arc::new(JsonStore::new(dir.join("tunables.json"))), Arc::new(JsonStore::new(dir.join("toggles.json"))));
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let shared = Arc::new(Shared::new());
    let mut ctx = Ctx::new(shared.clone(), StreamHub::new(), port);
    ctx.settings = Some(Arc::new(SettingsPage::new(tunables.clone(), toggles.clone(), shared, vec![("Version".into(), "test".into())])));
    tokio::spawn(http::serve(listener, Arc::new(ctx)));
    App { port, tunables, toggles }
}

/// Sends a request and returns (status line + headers, body).
async fn request(port: u16, method: &str, path: &str, extra: &[(&str, &str)], body: Option<&str>) -> (String, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        head.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).await.unwrap();
    if let Some(b) = body {
        s.write_all(b.as_bytes()).await.unwrap();
    }
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    (head.to_string(), body.to_string())
}

const FORM: (&str, &str) = ("Content-Type", "application/x-www-form-urlencoded");

fn status(head: &str) -> u16 {
    head.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0)
}

#[tokio::test]
async fn the_page_is_served_with_its_protections_and_current_values() {
    let app = start("get").await;
    let (head, body) = request(app.port, "GET", "/settings", &[], None).await;
    assert_eq!(status(&head), 200, "{head}");
    let lower = head.to_ascii_lowercase();
    assert!(lower.contains("cache-control: no-store") && lower.contains("x-frame-options: deny") && lower.contains("frame-ancestors 'none'"), "{head}");
    assert!(body.contains("<title>Pryxea") && body.contains("name=\"queue_cap\" value=\"50\"") && body.contains("Request limits"), "page body missing pieces");
    assert!(!body.contains("${"));
    // A foreign Host header (DNS rebinding) never reaches the page.
    let mut s = TcpStream::connect(("127.0.0.1", app.port)).await.unwrap();
    s.write_all(b"GET /settings HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).await.unwrap();
    assert!(raw.starts_with("HTTP/1.1 421"), "{raw}");
}

#[tokio::test]
async fn saving_from_the_page_itself_works_and_is_reported() {
    let app = start("save").await;
    let origin = format!("http://127.0.0.1:{}", app.port);
    let form = "_settings_form=1&max_pending_per_chatter=4&request_cooldown_seconds=45&queue_cap=120&max_request_duration_seconds=900";
    let (head, body) = request(app.port, "POST", "/settings", &[FORM, ("Origin", &origin), ("Sec-Fetch-Site", "same-origin")], Some(form)).await;
    assert_eq!(status(&head), 200, "{head}\n{body}");
    assert!(body.contains("banner-ok") && body.contains("Saved."), "{body}");
    assert!(body.contains("name=\"queue_cap\" value=\"120\""), "the re-rendered page shows the new values");
    let t = Tunables::from_map(&app.tunables.read());
    assert_eq!((t.max_pending_per_chatter, t.request_cooldown_seconds, t.queue_cap, t.max_request_duration_seconds), (4, 45, 120, 900));
    // The full form had no radio checkbox ticked, so radio autoplay is now off.
    assert!(!Toggles::from_map(&app.toggles.read()).radio_autoplay_enabled);
    assert!(body.contains("name=\"radio_autoplay_enabled\" >"));
}

#[tokio::test]
async fn cross_site_forms_are_refused_and_change_nothing() {
    let app = start("csrf").await;
    let form = "_settings_form=1&queue_cap=1&request_cooldown_seconds=3600";
    for (label, headers) in [
        ("foreign Origin", vec![FORM, ("Origin", "https://evil.example")]),
        ("null Origin", vec![FORM, ("Origin", "null")]),
        ("foreign Referer", vec![FORM, ("Referer", "https://evil.example/attack.html")]),
        ("cross-site fetch", vec![FORM, ("Origin", "http://127.0.0.1"), ("Sec-Fetch-Site", "cross-site")]),
    ] {
        let (head, body) = request(app.port, "POST", "/settings", &headers, Some(form)).await;
        assert_eq!(status(&head), 403, "{label}: {head}");
        assert!(body.contains("Origin check failed"), "{label}: {body}");
    }
    assert!(app.tunables.read().is_empty(), "a refused save must not write anything");
    assert!(app.toggles.read().is_empty());
}

#[tokio::test]
async fn bad_values_save_nothing_and_say_why() {
    let app = start("invalid").await;
    let (head, body) = request(app.port, "POST", "/settings", &[FORM], Some("_settings_form=1&queue_cap=5&request_cooldown_seconds=99999&max_pending_per_chatter=abc")).await;
    assert_eq!(status(&head), 400, "{head}");
    assert!(body.contains("banner-error") && body.contains("Nothing was saved"), "{body}");
    assert!(body.contains("request_cooldown_seconds: must be between 0 and 3600") && body.contains("max_pending_per_chatter: not a number"), "{body}");
    assert!(app.tunables.read().is_empty(), "even the valid queue_cap must not have been saved");
}

#[tokio::test]
async fn scripts_can_post_partial_updates_without_wiping_the_switches() {
    let app = start("partial").await;
    // curl-style: no Origin, no marker, one field.
    let (head, _) = request(app.port, "POST", "/settings", &[FORM], Some("queue_cap=33")).await;
    assert_eq!(status(&head), 200);
    assert_eq!(Tunables::from_map(&app.tunables.read()).queue_cap, 33);
    assert!(Toggles::from_map(&app.toggles.read()).radio_autoplay_enabled, "radio stays on: the post didn't mention it");
    let (head, _) = request(app.port, "POST", "/settings", &[FORM], Some("radio_autoplay_enabled=false")).await;
    assert_eq!(status(&head), 200);
    assert!(!Toggles::from_map(&app.toggles.read()).radio_autoplay_enabled);
}

#[tokio::test]
async fn malformed_submissions_are_rejected_before_parsing() {
    let app = start("malformed").await;
    let (head, _) = request(app.port, "POST", "/settings", &[("Content-Type", "application/json")], Some("{\"queue_cap\": 1}")).await;
    assert_eq!(status(&head), 415, "{head}");
    let big = format!("queue_cap=1&junk={}", "x".repeat(40_000));
    let (head, _) = request(app.port, "POST", "/settings", &[FORM], Some(&big)).await;
    assert_eq!(status(&head), 413, "{head}");
    let (head, _) = request(app.port, "POST", "/healthz", &[FORM], Some("a=b")).await;
    assert_eq!(status(&head), 405, "{head}");
    let (head, _) = request(app.port, "PUT", "/settings", &[FORM], Some("a=b")).await;
    assert_eq!(status(&head), 405, "{head}");
    assert!(app.tunables.read().is_empty());
}

#[tokio::test]
async fn a_saved_limit_takes_effect_immediately_in_the_running_station() {
    let mut r = rig("settings-live", true);
    r.lookup.add("a", track("Song A", A, 100));
    r.lookup.add("b", track("Song B", B, 100));
    let page = SettingsPage::new(r.tunables.clone(), r.toggles.clone(), r.shared.clone(), vec![]);
    let ann = who(7, "Ann");
    // Default settings: no cooldown, so two requests in a row are fine.
    assert!(r.station.request(&ann, "a", r.replier()).starts_with("Looking up"));
    r.out().await;
    // Now a mod sets a one-minute cooldown on the settings page.
    page.apply(&[("request_cooldown_seconds".to_string(), "60".to_string())]).unwrap();
    let reply = r.station.request(&ann, "b", r.replier());
    assert!(reply.starts_with("Slow down \u{2014} try again in "), "{reply}");
    // And turning radio off is seen by the station too.
    assert!(r.station.radio_status());
    page.apply(&[("radio_autoplay_enabled".to_string(), "off".to_string())]).unwrap();
    assert!(!r.station.radio_status());
    tokio::time::sleep(Duration::from_millis(10)).await;
}
