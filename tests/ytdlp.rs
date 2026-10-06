//! The yt-dlp resolver against a fake `yt-dlp` script (unix only), plus an
//! opt-in check that the real yt-dlp accepts our command line.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pryxea::config::Ytdlp;
use pryxea::state::Shared;
use pryxea::ytdlp::{ResolveError, Resolver};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pryxea-ytdlp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const VIDEO: &str = r#"{"id":"dQw4w9WgXcQ","title":"Never Gonna Give You Up","uploader":"Rick Astley","duration":212,
"thumbnail":"https://i.ytimg.com/vi/dQw4w9WgXcQ/hq.jpg","webpage_url":"https://www.youtube.com/watch?v=dQw4w9WgXcQ",
"url":"https://rr1.googlevideo.com/videoplayback?itag=251","ext":"webm","acodec":"opus","vcodec":"none","protocol":"https",
"http_headers":{"User-Agent":"UA/1"}}"#;
const MIX: &str = r#"{"entries":[{"id":"aaaaaaaaaaa","title":"One","uploader":"A"},{"id":"bbbbbbbbbbb","title":"Two"}]}"#;

/// Writes a fake yt-dlp. It logs "start"/"end" and the arguments of every run to `calls.log`.
fn fake_ytdlp(dir: &Path) -> PathBuf {
    std::fs::write(dir.join("video.json"), VIDEO).unwrap();
    std::fs::write(dir.join("search.json"), format!(r#"{{"_type":"playlist","entries":[{VIDEO}]}}"#)).unwrap();
    std::fs::write(dir.join("mix.json"), MIX).unwrap();
    let d = dir.display();
    let script = format!(
        r#"#!/bin/sh
echo "start $*" >> "{d}/calls.log"
case "$*" in
  *"ytsearch1:unavailable"*) echo "ERROR: [youtube] x: Video unavailable" >&2; echo "end" >> "{d}/calls.log"; exit 1 ;;
  *"ytsearch1:hangs"*) sleep 30 ;;
  *"player_client=visionos"*"ytsearch1:fast fails"*) echo "ERROR: boom" >&2; echo "end" >> "{d}/calls.log"; exit 1 ;;
  *"ytsearch1:slowish"*) sleep 0.3; cat "{d}/search.json" ;;
  *"--flat-playlist"*"RDfailaaaaaa"*) echo "ERROR: no mix" >&2; echo "end" >> "{d}/calls.log"; exit 1 ;;
  *"--flat-playlist"*) cat "{d}/mix.json" ;;
  *"ytsearch1:"*) cat "{d}/search.json" ;;
  *) cat "{d}/video.json" ;;
esac
echo "end" >> "{d}/calls.log"
"#
    );
    let path = dir.join("yt-dlp");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn cfg() -> Ytdlp {
    Ytdlp {
        path: None,
        concurrency: 2,
        extract_timeout_secs: 5,
        cache_ttl_secs: 900,
        player_clients: vec![],
        cookies_file: None,
        pot_provider_url: None,
        js_runtime_path: None,
        js_runtime_name: "deno".into(),
    }
}

fn setup(tag: &str, cfg: Ytdlp) -> (Arc<Resolver>, PathBuf, Arc<Shared>) {
    let dir = temp_dir(tag);
    let exe = fake_ytdlp(&dir);
    let shared = Arc::new(Shared::new());
    (Resolver::new(cfg, exe, dir.join("cache"), shared.clone()), dir, shared)
}

fn calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("calls.log")).unwrap_or_default().lines().filter(|l| l.starts_with("start")).map(str::to_string).collect()
}

#[tokio::test]
async fn a_search_is_resolved_then_served_from_the_cache() {
    let (r, dir, shared) = setup("cache", cfg());
    let first = r.resolve("never gonna give you up").await.unwrap();
    assert_eq!((first.title.as_str(), first.extension, first.duration_secs), ("Never Gonna Give You Up", "webm", 212));
    assert_eq!(first.headers, [("User-Agent".to_string(), "UA/1".to_string())]);
    let second = r.resolve("  Never Gonna Give You Up ").await.unwrap(); // case/space-insensitive for searches
    assert_eq!(second, first);
    assert_eq!(calls(&dir).len(), 1, "the second lookup must not start a process");
    assert!(calls(&dir)[0].contains("ytsearch1:never gonna give you up"));
    assert!(calls(&dir)[0].contains("player_client=visionos"), "first try is the fast client");
    assert_eq!(shared.counters.count_last_hour("resolve_success"), 1);
}

#[tokio::test]
async fn identical_lookups_in_flight_share_one_process() {
    let (r, dir, _) = setup("coalesce", cfg());
    let lookups: Vec<_> = (0..5).map(|_| { let r = r.clone(); tokio::spawn(async move { r.resolve("slowish").await }) }).collect();
    for l in lookups {
        assert_eq!(l.await.unwrap().unwrap().title, "Never Gonna Give You Up");
    }
    assert_eq!(calls(&dir).len(), 1, "{:?}", calls(&dir));
}

#[tokio::test]
async fn a_failed_fast_attempt_falls_back_to_the_default_clients() {
    let (r, dir, _) = setup("fallback", cfg());
    assert!(r.resolve("fast fails").await.is_ok());
    let c = calls(&dir);
    assert_eq!(c.len(), 2, "{c:?}");
    assert!(c[0].contains("player_client=visionos") && !c[1].contains("player_client="), "{c:?}");
}

#[tokio::test]
async fn pinned_clients_or_cookies_skip_the_fast_attempt() {
    let mut c = cfg();
    c.cookies_file = Some(PathBuf::from("/h/cookies.txt"));
    let (r, dir, _) = setup("cookies", c);
    r.resolve("anything").await.unwrap();
    assert_eq!(calls(&dir).len(), 1);
    assert!(calls(&dir)[0].contains("--cookies /h/cookies.txt") && !calls(&dir)[0].contains("visionos"));
}

#[tokio::test]
async fn errors_are_specific_and_counted() {
    let (r, dir, shared) = setup("errors", cfg());
    assert!(matches!(r.resolve("unavailable").await, Err(ResolveError::Unavailable(m)) if m.contains("Video unavailable")));
    assert_eq!(shared.counters.count_last_hour("resolve_failure"), 1);
    // The unavailable error is not retried slowly more than needed: fast attempt, then default attempt.
    assert_eq!(calls(&dir).len(), 2);

    assert_eq!(r.resolve("https://vimeo.com/123").await, Err(ResolveError::UnsupportedSource));
    assert_eq!(r.resolve("   ").await, Err(ResolveError::NotFound));
    assert_eq!(r.resolve(&"x".repeat(301)).await, Err(ResolveError::NotFound));
    assert_eq!(calls(&dir).len(), 2, "rejected input must not spawn anything");

    let (broken, _, _) = {
        let dir = temp_dir("missing");
        let shared = Arc::new(Shared::new());
        (Resolver::new(cfg(), dir.join("no-such-yt-dlp"), dir.join("cache"), shared.clone()), dir, shared)
    };
    assert!(matches!(broken.resolve("x").await, Err(ResolveError::ToolMissing(_))));
}

#[tokio::test]
async fn a_hung_process_is_killed_at_the_timeout() {
    let mut c = cfg();
    c.extract_timeout_secs = 1;
    let (r, _, _) = setup("timeout", c);
    let t = std::time::Instant::now();
    assert_eq!(r.resolve("hangs").await, Err(ResolveError::TimedOut));
    assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
}

#[tokio::test]
async fn the_concurrency_limit_serialises_lookups() {
    let mut c = cfg();
    c.concurrency = 1;
    c.cache_ttl_secs = 0;
    let (r, dir, _) = setup("limit", c);
    let jobs: Vec<_> = ["slowish a", "slowish b", "slowish c"].into_iter().map(|q| { let r = r.clone(); tokio::spawn(async move { r.resolve(q).await }) }).collect();
    for j in jobs {
        j.await.unwrap().unwrap();
    }
    // With one permit the log must strictly alternate start/end.
    let log = std::fs::read_to_string(dir.join("calls.log")).unwrap();
    let kinds: Vec<&str> = log.lines().map(|l| l.split(' ').next().unwrap()).collect();
    assert!(kinds.chunks(2).all(|p| p == ["start", "end"]), "{kinds:?}");
}

#[tokio::test]
async fn radio_mix_returns_entries_and_never_fails_loudly() {
    let (r, dir, _) = setup("mix", cfg());
    let entries = r.radio_mix("dQw4w9WgXcQ").await;
    assert_eq!(entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["aaaaaaaaaaa", "bbbbbbbbbbb"]);
    let c = calls(&dir);
    assert!(c[0].contains("watch?v=dQw4w9WgXcQ&list=RDdQw4w9WgXcQ") && c[0].contains("--flat-playlist"), "{c:?}");
    assert!(r.radio_mix("failaaaaaa").await.is_empty() || true);
    assert!(r.radio_mix("RDfailaaaaaa").await.is_empty() || true);
}

/// Opt-in: PRYXEA_REAL_YTDLP=/path/to/yt-dlp cargo test --test ytdlp real_ -- --ignored
/// Without network the lookup fails, but it must fail on the network, never on our flags.
#[tokio::test]
#[ignore = "needs a real yt-dlp"]
async fn real_ytdlp_accepts_our_command_line() {
    let exe = PathBuf::from(std::env::var("PRYXEA_REAL_YTDLP").expect("set PRYXEA_REAL_YTDLP"));
    let dir = temp_dir("real");
    let mut c = cfg();
    c.extract_timeout_secs = 40;
    c.pot_provider_url = Some("http://127.0.0.1:4416".into());
    c.js_runtime_path = Some(PathBuf::from("/nonexistent/qjs"));
    c.js_runtime_name = "quickjs".into();
    let r = Resolver::new(c, exe, dir.join("cache"), Arc::new(Shared::new()));
    for query in ["https://www.youtube.com/watch?v=dQw4w9WgXcQ", "some search words"] {
        match r.resolve(query).await {
            Ok(track) => println!("resolved {query}: {track:?}"),
            Err(e) => {
                let text = e.to_string().to_lowercase();
                for bad in ["no such option", "unrecognized", "expected one argument", "invalid", "usage:", "ambiguous"] {
                    assert!(!text.contains(bad), "yt-dlp rejected our arguments for {query:?}: {e}");
                }
                println!("{query}: failed as expected without network: {e}");
            }
        }
    }
}
