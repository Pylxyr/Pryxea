//! Looking songs up with yt-dlp.
//!
//! yt-dlp runs as a short-lived child process per lookup, so it costs no
//! memory at all between requests. This module builds the command line,
//! runs it with a timeout, and turns its JSON into either a playable track
//! (a direct media URL our decoder can read) or a clear error. Results are
//! cached for a while; duplicate lookups in flight share one process.

use std::collections::HashMap;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::Semaphore;

use crate::config::Ytdlp;
use crate::state::Shared;
use crate::youtube;

/// Audio-only formats first, Opus preferred; formats our decoder can't read are never selected.
pub const FORMAT_SELECTOR: &str = "bestaudio[acodec=opus]/bestaudio[acodec^=mp4a.40.2]/bestaudio/best";
/// yt-dlp's own JS-less client: skips the slow signature solve.
const FAST_CLIENT: &str = "visionos";
const FAST_TIMEOUT: Duration = Duration::from_secs(15);
const MIX_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_QUERY_CHARS: usize = 300;
const MAX_JSON_BYTES: usize = 32 * 1024 * 1024;
const STDERR_TAIL: usize = 8 * 1024;

// ------------------------------------------------------------------- types

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// Direct URL of the audio stream.
    pub stream_url: String,
    /// Headers the media server expects (User-Agent and friends).
    pub headers: Vec<(String, String)>,
    /// "webm" or "m4a": a hint for the decoder.
    pub extension: &'static str,
    pub title: String,
    pub uploader: String,
    pub thumbnail_url: Option<String>,
    pub webpage_url: String,
    pub video_id: Option<String>,
    pub duration_secs: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixEntry {
    pub id: String,
    pub url: String,
    pub title: String,
    pub uploader: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// A link that isn't YouTube.
    UnsupportedSource,
    /// Nothing matched the search.
    NotFound,
    /// Private, removed, region-locked, age-gated...
    Unavailable(String),
    /// A live stream (no fixed length, can't be queued).
    Live,
    /// A delivery format or codec we can't play.
    Unplayable(String),
    TimedOut,
    /// yt-dlp isn't installed or couldn't start.
    ToolMissing(String),
    Failed(String),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::UnsupportedSource => f.write_str("only YouTube links are supported"),
            ResolveError::NotFound => f.write_str("nothing found"),
            ResolveError::Unavailable(m) => write!(f, "unavailable: {m}"),
            ResolveError::Live => f.write_str("live streams are not supported"),
            ResolveError::Unplayable(m) => write!(f, "can't be played: {m}"),
            ResolveError::TimedOut => f.write_str("the lookup timed out"),
            ResolveError::ToolMissing(m) => write!(f, "yt-dlp is not available: {m}"),
            ResolveError::Failed(m) => write!(f, "lookup failed: {m}"),
        }
    }
}

impl std::error::Error for ResolveError {}

// ----------------------------------------------------------- command line

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// One video (or the first search hit), with a media format chosen.
    Track { fast: bool },
    /// A Mix playlist, flat (no per-entry format work).
    Mix,
}

struct Plan<'a> {
    cfg: &'a Ytdlp,
    cache_dir: &'a Path,
    /// (runtime name, executable) when a JavaScript runtime is available.
    js: Option<(String, PathBuf)>,
}

impl Plan<'_> {
    fn fast_allowed(&self) -> bool {
        self.cfg.cookies_file.is_none() && self.cfg.player_clients.is_empty()
    }

    fn args(&self, mode: Mode, target: &str) -> Vec<String> {
        let mut a: Vec<String> = ["--ignore-config", "--no-warnings", "--no-progress", "--socket-timeout", "15", "-J"].map(String::from).into();
        a.extend(["--cache-dir".into(), self.cache_dir.display().to_string()]);
        // Whatever the link, only YouTube extractors may run (never the scrape-any-page fallback).
        a.extend(["--use-extractors".into(), "youtube(:.*)?".into()]);
        // The JS challenge solver is bundled in the official executables; this lets other builds fetch it.
        a.extend(["--remote-components".into(), "ejs:github".into()]);
        match mode {
            Mode::Track { .. } => a.extend(["--no-playlist".into(), "-f".into(), FORMAT_SELECTOR.into()]),
            Mode::Mix => a.extend(["--flat-playlist".into(), "--playlist-items".into(), "1-15".into()]),
        }
        let clients: Vec<&str> = match mode {
            Mode::Track { fast: true } => vec![FAST_CLIENT],
            _ => self.cfg.player_clients.iter().map(String::as_str).collect(),
        };
        if !clients.is_empty() {
            a.extend(["--extractor-args".into(), format!("youtube:player_client={}", clients.join(","))]);
        }
        if let Some(url) = &self.cfg.pot_provider_url {
            a.extend(["--extractor-args".into(), format!("youtubepot-bgutilhttp:base_url={url}")]);
        }
        if let Some(cookies) = &self.cfg.cookies_file {
            a.extend(["--cookies".into(), cookies.display().to_string()]);
        }
        if let Some((name, path)) = &self.js {
            let runtime = format!("{name}:{}", path.display());
            match mode {
                // The fast attempt uses one quick runtime only, as the original did.
                Mode::Track { fast: true } => a.extend(["--no-js-runtimes".into(), "--js-runtimes".into(), runtime]),
                _ => a.extend(["--js-runtimes".into(), runtime]),
            }
        }
        // `--` so a search that starts with a dash can never be read as an option.
        a.push("--".into());
        a.push(target.to_string());
        a
    }
}

// ------------------------------------------------------------ run a process

pub(crate) enum RunError {
    Spawn(String),
    Timeout,
}

pub(crate) struct Output {
    pub(crate) status_ok: bool,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr_tail: String,
}

fn read_capped(src: Option<impl Read>, cap: usize, keep_tail: bool) -> Vec<u8> {
    let Some(mut src) = src else { return Vec::new() };
    let mut out = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    while let Ok(n) = src.read(&mut buf) {
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        if out.len() > cap {
            if keep_tail {
                out.drain(..out.len() - cap);
            } else {
                out.truncate(cap + 1); // one byte over = "too large"; keep draining so the child never blocks
            }
        }
    }
    out
}

/// Kills a process and everything it started (yt-dlp spawns its JS runtime as a child; a
/// survivor would keep our output pipes open). Best effort: errors just mean it was already gone.
fn kill_tree(pid: u32) {
    #[cfg(unix)]
    {
        // The child leads its own process group (see `run_process`), so `-pid` addresses the lot.
        let _ = Command::new("kill").args(["-KILL", "--", &format!("-{pid}")]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).creation_flags(0x0800_0000).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
}

/// Joins a reader thread, but never waits more than `grace` for one stuck on a pipe someone else holds open.
fn join_within(handle: std::thread::JoinHandle<Vec<u8>>, grace: Duration) -> Vec<u8> {
    let deadline = Instant::now() + grace;
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    if handle.is_finished() { handle.join().unwrap_or_default() } else { Vec::new() }
}

pub(crate) fn run_process(exe: &Path, args: &[String], timeout: Duration) -> Result<Output, RunError> {
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.env("PYTHONIOENCODING", "utf-8").env("NO_COLOR", "1");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flash
    }
    let mut child = cmd.spawn().map_err(|e| RunError::Spawn(format!("{}: {e}", exe.display())))?;
    let pid = child.id();
    let out = child.stdout.take();
    let err = child.stderr.take();
    let out_thread = std::thread::spawn(move || read_capped(out, MAX_JSON_BYTES, false));
    let err_thread = std::thread::spawn(move || read_capped(err, STDERR_TAIL, true));
    let deadline = Instant::now() + timeout;
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() >= deadline => break Err(RunError::Timeout),
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => break Err(RunError::Spawn(e.to_string())),
        }
    };
    // Whether it finished or not, nothing it started may outlive this call.
    kill_tree(pid);
    if outcome.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    let stdout = join_within(out_thread, Duration::from_secs(2));
    let stderr = join_within(err_thread, Duration::from_secs(2));
    let status = outcome?;
    Ok(Output { status_ok: status.success(), stdout, stderr_tail: String::from_utf8_lossy(&stderr).to_string() })
}

/// Turns yt-dlp's complaint into something specific.
fn classify_failure(stderr: &str) -> ResolveError {
    let last_error = stderr.lines().rev().find(|l| l.contains("ERROR")).unwrap_or_else(|| stderr.lines().last().unwrap_or("")).trim();
    let message: String = last_error.trim_start_matches("ERROR:").trim().chars().take(200).collect();
    let lower = stderr.to_ascii_lowercase();
    if ["video unavailable", "private video", "has been removed", "not available", "members-only", "members only", "age-restricted", "confirm your age", "blocked it"]
        .iter()
        .any(|p| lower.contains(p))
    {
        return ResolveError::Unavailable(message);
    }
    if lower.contains("sign in to confirm") {
        return ResolveError::Failed("YouTube is asking to sign in (bot check); a cookies file (YTDLP_COOKIES_FILE) is needed".into());
    }
    ResolveError::Failed(if message.is_empty() { "yt-dlp exited with an error".into() } else { message })
}

// ----------------------------------------------------------------- parsing

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// Headers we pass on to the media server. Anything about framing or compression is ours to decide.
fn media_headers(fmt: &Value) -> Vec<(String, String)> {
    const DROP: [&str; 6] = ["host", "connection", "content-length", "accept-encoding", "range", "transfer-encoding"];
    fmt.get("http_headers")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .filter(|(k, v)| !DROP.contains(&k.to_ascii_lowercase().as_str()) && !v.contains(['\r', '\n']))
                .collect()
        })
        .unwrap_or_default()
}

/// Checks the codec/container pair against what the decoder handles. Returns the extension hint.
fn playable_extension(fmt: &Value) -> Result<&'static str, ResolveError> {
    let acodec = str_of(fmt, "acodec").map(str::to_ascii_lowercase);
    let ext = str_of(fmt, "ext").map(str::to_ascii_lowercase).unwrap_or_default();
    if acodec.as_deref() == Some("none") {
        return Err(ResolveError::Unplayable("the chosen format has no audio".into()));
    }
    let hint = match ext.as_str() {
        "webm" => "webm",
        "m4a" | "mp4" => "m4a",
        other => return Err(ResolveError::Unplayable(format!("unsupported container {other:?}"))),
    };
    match acodec.as_deref() {
        None | Some("opus") => Ok(hint),
        Some(c) if c.starts_with("mp4a.40.2") && hint == "m4a" => Ok(hint),
        Some(c) if c.starts_with("mp4a.40.5") || c.starts_with("mp4a.40.29") => Err(ResolveError::Unplayable("HE-AAC audio is not supported".into())),
        Some(c) => Err(ResolveError::Unplayable(format!("unsupported audio codec {c:?}"))),
    }
}

pub fn parse_resolved(root: &Value, fallback_url: &str) -> Result<Resolved, ResolveError> {
    // A search comes back as a playlist wrapper; take the first real entry.
    let info = if str_of(root, "_type") == Some("playlist") {
        root.get("entries").and_then(Value::as_array).and_then(|e| e.iter().find(|x| x.is_object())).ok_or(ResolveError::NotFound)?
    } else {
        root
    };
    if info.get("is_live").and_then(Value::as_bool) == Some(true) || matches!(str_of(info, "live_status"), Some("is_live" | "is_upcoming" | "post_live")) {
        return Err(ResolveError::Live);
    }
    // The selected format's fields sit at the top level; merged downloads list them separately.
    let fmt: &Value = [Some(info), info.pointer("/requested_downloads/0"), info.get("requested_formats").and_then(Value::as_array).and_then(|a| a.iter().find(|f| str_of(f, "vcodec").is_none_or(|v| v == "none")))]
        .into_iter()
        .flatten()
        .find(|v| str_of(v, "url").is_some())
        .ok_or_else(|| ResolveError::Failed("yt-dlp did not return a stream URL".into()))?;
    let stream_url = str_of(fmt, "url").unwrap_or_default().to_string();
    let protocol = str_of(fmt, "protocol").unwrap_or("https");
    let url_path = stream_url.split(['?', '#']).next().unwrap_or("");
    if !protocol.starts_with("http") || protocol.contains("m3u8") || protocol.contains("dash") || url_path.ends_with(".m3u8") || url_path.ends_with(".mpd") {
        return Err(ResolveError::Unplayable(format!("streaming protocol {protocol:?} is not supported")));
    }
    if !(stream_url.starts_with("https://") || stream_url.starts_with("http://")) {
        return Err(ResolveError::Failed("yt-dlp returned a non-http stream URL".into()));
    }
    let extension = playable_extension(fmt)?;
    let webpage_url = str_of(info, "webpage_url").or_else(|| str_of(info, "original_url")).unwrap_or(fallback_url).to_string();
    let video_id = str_of(info, "id").filter(|id| id.len() == 11).map(str::to_string).or_else(|| youtube::video_id(&webpage_url));
    let thumbnail_url = str_of(info, "thumbnail").map(str::to_string).or_else(|| {
        info.get("thumbnails").and_then(Value::as_array).and_then(|t| t.iter().rev().find_map(|x| str_of(x, "url"))).map(str::to_string)
    });
    let duration_secs = info.get("duration").and_then(Value::as_f64).filter(|d| d.is_finite() && *d > 0.0).map_or(0, |d| d.round().min(f64::from(u32::MAX)) as u32);
    Ok(Resolved {
        stream_url,
        headers: media_headers(fmt),
        extension,
        title: str_of(info, "title").unwrap_or("Unknown title").to_string(),
        uploader: str_of(info, "uploader").or_else(|| str_of(info, "channel")).unwrap_or("").to_string(),
        thumbnail_url,
        webpage_url,
        video_id,
        duration_secs,
    })
}

pub fn parse_mix(root: &Value) -> Vec<MixEntry> {
    root.get("entries")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| {
                    let id = str_of(e, "id").filter(|id| id.len() == 11)?.to_string();
                    Some(MixEntry {
                        url: str_of(e, "url").map_or_else(|| format!("https://www.youtube.com/watch?v={id}"), str::to_string),
                        title: str_of(e, "title").unwrap_or("Unknown title").to_string(),
                        uploader: str_of(e, "uploader").or_else(|| str_of(e, "channel")).unwrap_or("").to_string(),
                        id,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- resolver

pub struct Resolver {
    cfg: Ytdlp,
    exe: PathBuf,
    cache_dir: PathBuf,
    shared: Arc<Shared>,
    /// A JS runtime found after start-up (it is downloaded in the background on first run).
    js_override: Mutex<Option<(String, PathBuf)>>,
    cache: Mutex<HashMap<String, (Instant, Resolved)>>,
    inflight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    permits: Semaphore,
}

impl Resolver {
    pub fn new(cfg: Ytdlp, exe: PathBuf, cache_dir: PathBuf, shared: Arc<Shared>) -> Arc<Resolver> {
        let permits = Semaphore::new(cfg.concurrency);
        Arc::new(Resolver { cfg, exe, cache_dir, shared, js_override: Mutex::default(), cache: Mutex::default(), inflight: Mutex::default(), permits })
    }

    /// Tells the resolver about a JavaScript runtime that became available (e.g. QuickJS just downloaded).
    pub fn set_js_runtime(&self, name: &str, path: PathBuf) {
        *self.js_override.lock().unwrap_or_else(|e| e.into_inner()) = Some((name.to_string(), path));
    }

    fn plan(&self) -> Plan<'_> {
        let js = self.js_override.lock().unwrap_or_else(|e| e.into_inner()).clone().or_else(|| self.cfg.js_runtime_path.clone().map(|p| (self.cfg.js_runtime_name.clone(), p)));
        Plan { cfg: &self.cfg, cache_dir: &self.cache_dir, js }
    }

    fn cached(&self, key: &str) -> Option<Resolved> {
        let ttl = Duration::from_secs(self.cfg.cache_ttl_secs);
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.retain(|_, (at, _)| at.elapsed() < ttl);
        cache.get(key).map(|(_, r)| r.clone())
    }

    /// Drops a cached lookup, e.g. after its stream URL turned out to be stale.
    pub fn forget(&self, target: &str) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.remove(target);
        cache.remove(&format!("ytsearch1:{}", target.to_lowercase()));
    }

    /// Looks up a YouTube link or a search phrase.
    pub async fn resolve(self: &Arc<Self>, query: &str) -> Result<Resolved, ResolveError> {
        let query = query.trim();
        if query.is_empty() || query.chars().count() > MAX_QUERY_CHARS {
            return Err(ResolveError::NotFound);
        }
        let (target, key) = if youtube::is_url(query) {
            if !youtube::is_allowed_host(query) {
                return Err(ResolveError::UnsupportedSource);
            }
            (query.to_string(), query.to_string())
        } else {
            (format!("ytsearch1:{query}"), format!("ytsearch1:{}", query.to_lowercase()))
        };
        if let Some(hit) = self.cached(&key) {
            return Ok(hit);
        }
        // Identical lookups arriving together share one process: later ones wait, then hit the cache.
        let gate = {
            let mut inflight = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
            Arc::clone(inflight.entry(key.clone()).or_default())
        };
        let _turn = gate.lock().await;
        if let Some(hit) = self.cached(&key) {
            return Ok(hit);
        }
        let _permit = self.permits.acquire().await.map_err(|_| ResolveError::Failed("shutting down".into()))?;
        let me = Arc::clone(self);
        let t = target.clone();
        let result = tokio::task::spawn_blocking(move || me.resolve_blocking(&t)).await.unwrap_or_else(|_| Err(ResolveError::Failed("the lookup crashed".into())));
        self.shared.counters.record(if result.is_ok() { "resolve_success" } else { "resolve_failure" });
        if let Ok(r) = &result {
            if self.cfg.cache_ttl_secs > 0 {
                self.cache.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), (Instant::now(), r.clone()));
            }
        }
        let mut inflight = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if Arc::strong_count(&gate) <= 2 {
            inflight.remove(&key); // nobody else is waiting on this key
        }
        result
    }

    fn resolve_blocking(&self, target: &str) -> Result<Resolved, ResolveError> {
        let plan = self.plan();
        let slow = Duration::from_secs(self.cfg.extract_timeout_secs);
        if plan.fast_allowed() {
            match self.run_track(&plan, Mode::Track { fast: true }, target, FAST_TIMEOUT.min(slow)) {
                Ok(r) => return Ok(r),
                Err(e @ (ResolveError::ToolMissing(_) | ResolveError::UnsupportedSource)) => return Err(e),
                Err(e) => crate::debug!("fast lookup failed ({e}); trying yt-dlp's default clients"),
            }
        }
        self.run_track(&plan, Mode::Track { fast: false }, target, slow)
    }

    fn run_track(&self, plan: &Plan<'_>, mode: Mode, target: &str, timeout: Duration) -> Result<Resolved, ResolveError> {
        let json = self.run_json(plan, mode, target, timeout)?;
        parse_resolved(&json, target)
    }

    fn run_json(&self, plan: &Plan<'_>, mode: Mode, target: &str, timeout: Duration) -> Result<Value, ResolveError> {
        let args = plan.args(mode, target);
        let out = match run_process(&self.exe, &args, timeout) {
            Ok(o) => o,
            Err(RunError::Timeout) => return Err(ResolveError::TimedOut),
            Err(RunError::Spawn(m)) => return Err(ResolveError::ToolMissing(m)),
        };
        if !out.status_ok {
            return Err(classify_failure(&out.stderr_tail));
        }
        if out.stdout.len() > MAX_JSON_BYTES {
            return Err(ResolveError::Failed("yt-dlp output was unreasonably large".into()));
        }
        serde_json::from_slice(&out.stdout).map_err(|_| ResolveError::Failed("yt-dlp returned unreadable output".into()))
    }

    /// YouTube's own "more like this" queue for a video. Any failure just means no suggestions.
    pub async fn radio_mix(self: &Arc<Self>, seed_video_id: &str) -> Vec<MixEntry> {
        let url = format!("https://www.youtube.com/watch?v={seed_video_id}&list=RD{seed_video_id}");
        let Ok(_permit) = self.permits.acquire().await else { return Vec::new() };
        let me = Arc::clone(self);
        let json = tokio::task::spawn_blocking(move || {
            let plan = me.plan();
            me.run_json(&plan, Mode::Mix, &url, MIX_TIMEOUT)
        })
        .await;
        match json {
            Ok(Ok(v)) => parse_mix(&v),
            Ok(Err(e)) => {
                crate::debug!("radio mix lookup failed (non-fatal): {e}");
                Vec::new()
            }
            Err(_) => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> Ytdlp {
        Ytdlp {
            path: None,
            concurrency: 2,
            extract_timeout_secs: 45,
            cache_ttl_secs: 900,
            player_clients: vec![],
            cookies_file: None,
            pot_provider_url: None,
            js_runtime_path: None,
            js_runtime_name: "deno".into(),
        }
    }

    fn plan_for(cfg: &Ytdlp) -> Plan<'_> {
        Plan { cfg, cache_dir: Path::new("/c"), js: cfg.js_runtime_path.clone().map(|p| (cfg.js_runtime_name.clone(), p)) }
    }

    fn plan_args(cfg: &Ytdlp, mode: Mode, target: &str) -> Vec<String> {
        plan_for(cfg).args(mode, target)
    }

    fn has_pair(args: &[String], a: &str, b: &str) -> bool {
        args.windows(2).any(|w| w[0] == a && w[1] == b)
    }

    #[test]
    fn the_target_always_comes_last_after_a_double_dash() {
        let a = plan_args(&cfg(), Mode::Track { fast: false }, "-o evil");
        assert_eq!(&a[a.len() - 2..], ["--", "-o evil"]);
        assert!(has_pair(&a, "--use-extractors", "youtube(:.*)?"));
        assert!(has_pair(&a, "-f", FORMAT_SELECTOR));
        assert!(a.contains(&"--no-playlist".to_string()) && a.contains(&"-J".to_string()));
        assert!(a.contains(&"--ignore-config".to_string()));
    }

    #[test]
    fn fast_attempt_uses_one_client_and_one_runtime() {
        let mut c = cfg();
        c.js_runtime_path = Some(PathBuf::from("/bin/qjs"));
        c.js_runtime_name = "quickjs".into();
        let fast = plan_args(&c, Mode::Track { fast: true }, "x");
        assert!(has_pair(&fast, "--extractor-args", "youtube:player_client=visionos"));
        assert!(fast.contains(&"--no-js-runtimes".to_string()) && has_pair(&fast, "--js-runtimes", "quickjs:/bin/qjs"));
        let slow = plan_args(&c, Mode::Track { fast: false }, "x");
        assert!(!slow.iter().any(|s| s.contains("player_client")));
        assert!(!slow.contains(&"--no-js-runtimes".to_string()) && has_pair(&slow, "--js-runtimes", "quickjs:/bin/qjs"));
    }

    #[test]
    fn cookies_or_pinned_clients_disable_the_fast_path_and_options_pass_through() {
        let mut c = cfg();
        assert!(plan_for(&c).fast_allowed());
        c.cookies_file = Some(PathBuf::from("/h/cookies.txt"));
        c.player_clients = vec!["web".into(), "tv".into()];
        c.pot_provider_url = Some("http://127.0.0.1:4416".into());
        assert!(!plan_for(&c).fast_allowed());
        let a = plan_args(&c, Mode::Track { fast: false }, "x");
        assert!(has_pair(&a, "--cookies", "/h/cookies.txt"));
        assert!(has_pair(&a, "--extractor-args", "youtube:player_client=web,tv"));
        assert!(has_pair(&a, "--extractor-args", "youtubepot-bgutilhttp:base_url=http://127.0.0.1:4416"));
    }

    #[test]
    fn the_mix_lookup_is_flat_and_keeps_the_playlist() {
        let a = plan_args(&cfg(), Mode::Mix, "https://www.youtube.com/watch?v=aaaaaaaaaaa&list=RDaaaaaaaaaaa");
        assert!(a.contains(&"--flat-playlist".to_string()) && has_pair(&a, "--playlist-items", "1-15"));
        assert!(!a.contains(&"--no-playlist".to_string()) && !a.contains(&"-f".to_string()));
    }

    fn opus_video() -> Value {
        json!({
            "id": "dQw4w9WgXcQ", "title": " Never Gonna Give You Up ", "uploader": "Rick Astley", "channel": "Rick Astley Official",
            "duration": 212.4, "thumbnail": "https://i.ytimg.com/vi/dQw4w9WgXcQ/maxresdefault.jpg",
            "webpage_url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ", "is_live": false,
            "url": "https://rr1---sn-test.googlevideo.com/videoplayback?expire=1&itag=251", "ext": "webm", "acodec": "opus", "vcodec": "none",
            "protocol": "https", "format_id": "251",
            "http_headers": {"User-Agent": "Mozilla/5.0", "Accept-Encoding": "gzip, deflate", "Accept": "*/*", "Host": "evil", "Sec-Fetch-Mode": "navigate"},
            "formats": [{"format_id": "139", "acodec": "mp4a.40.5", "ext": "m4a"}]
        })
    }

    #[test]
    fn an_opus_video_becomes_a_track() {
        let r = parse_resolved(&opus_video(), "ignored").unwrap();
        assert_eq!((r.extension, r.title.as_str(), r.uploader.as_str(), r.duration_secs), ("webm", "Never Gonna Give You Up", "Rick Astley", 212));
        assert_eq!(r.video_id.as_deref(), Some("dQw4w9WgXcQ"));
        assert!(r.stream_url.contains("googlevideo.com"));
        let names: Vec<&str> = r.headers.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"User-Agent") && names.contains(&"Sec-Fetch-Mode"));
        assert!(!names.contains(&"Accept-Encoding") && !names.contains(&"Host"), "{names:?}");
    }

    #[test]
    fn search_results_unwrap_to_the_first_entry_and_merged_formats_are_found() {
        let search = json!({"_type": "playlist", "entries": [null, opus_video()]});
        assert_eq!(parse_resolved(&search, "x").unwrap().title, "Never Gonna Give You Up");
        assert_eq!(parse_resolved(&json!({"_type": "playlist", "entries": []}), "x"), Err(ResolveError::NotFound));

        let mut merged = opus_video();
        let fmt = merged.as_object_mut().unwrap();
        let audio = json!({"url": "https://h/a", "ext": "m4a", "acodec": "mp4a.40.2", "vcodec": "none", "protocol": "https"});
        fmt.remove("url");
        fmt.insert("requested_formats".into(), json!([{"url": "https://h/v", "ext": "mp4", "vcodec": "avc1", "acodec": "none"}, audio]));
        let r = parse_resolved(&merged, "x").unwrap();
        assert_eq!((r.stream_url.as_str(), r.extension), ("https://h/a", "m4a"));
    }

    #[test]
    fn unplayable_things_are_refused_with_a_reason() {
        let with = |k: &str, v: Value| {
            let mut j = opus_video();
            j[k] = v;
            parse_resolved(&j, "x")
        };
        assert_eq!(with("is_live", json!(true)), Err(ResolveError::Live));
        assert_eq!(with("live_status", json!("is_upcoming")), Err(ResolveError::Live));
        assert!(matches!(with("protocol", json!("m3u8_native")), Err(ResolveError::Unplayable(_))));
        assert!(matches!(with("url", json!("https://h/master.m3u8?x=1")), Err(ResolveError::Unplayable(_))));
        assert!(matches!(with("url", json!("file:///etc/passwd")), Err(ResolveError::Failed(_))));
        assert!(matches!(with("acodec", json!("none")), Err(ResolveError::Unplayable(_))));
        assert!(matches!(with("acodec", json!("vorbis")), Err(ResolveError::Unplayable(_))));
        assert!(matches!(with("ext", json!("ogg")), Err(ResolveError::Unplayable(_))));
        let mut he = opus_video();
        he["ext"] = json!("m4a");
        he["acodec"] = json!("mp4a.40.5");
        assert!(matches!(parse_resolved(&he, "x"), Err(ResolveError::Unplayable(m)) if m.contains("HE-AAC")));
        let mut aac = opus_video();
        aac["ext"] = json!("m4a");
        aac["acodec"] = json!("mp4a.40.2");
        assert_eq!(parse_resolved(&aac, "x").unwrap().extension, "m4a");
        assert!(matches!(parse_resolved(&json!({"title": "no url"}), "x"), Err(ResolveError::Failed(_))));
    }

    #[test]
    fn missing_fields_get_sensible_defaults() {
        let r = parse_resolved(&json!({"url": "https://h/x", "ext": "webm"}), "https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap();
        assert_eq!((r.title.as_str(), r.uploader.as_str(), r.duration_secs), ("Unknown title", "", 0));
        assert_eq!(r.webpage_url, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert_eq!(r.video_id.as_deref(), Some("dQw4w9WgXcQ"));
        let t = parse_resolved(&json!({"url": "https://h/x", "ext": "webm", "thumbnails": [{"url": "https://t/small"}, {"url": "https://t/big"}]}), "x").unwrap();
        assert_eq!(t.thumbnail_url.as_deref(), Some("https://t/big"));
    }

    #[test]
    fn mix_entries_keep_only_real_videos() {
        let mix = json!({"entries": [
            {"id": "aaaaaaaaaaa", "title": "One", "uploader": "A", "url": "https://www.youtube.com/watch?v=aaaaaaaaaaa"},
            {"id": "bbbbbbbbbbb", "title": "Two", "channel": "B"},
            {"id": "tooshort"}, {"title": "no id"}, null
        ]});
        let entries = parse_mix(&mix);
        assert_eq!(entries.len(), 2);
        assert_eq!((entries[1].url.as_str(), entries[1].uploader.as_str()), ("https://www.youtube.com/watch?v=bbbbbbbbbbb", "B"));
        assert!(parse_mix(&json!({})).is_empty());
    }

    #[test]
    fn failures_are_classified_by_what_yt_dlp_said() {
        assert!(matches!(classify_failure("ERROR: [youtube] abc: Video unavailable. This video is private"), ResolveError::Unavailable(_)));
        assert!(matches!(classify_failure("WARNING: x\nERROR: [youtube] abc: Sign in to confirm you're not a bot"), ResolveError::Failed(m) if m.contains("cookies")));
        assert!(matches!(classify_failure("ERROR: unable to download webpage: <urlopen error>"), ResolveError::Failed(m) if m.contains("unable to download")));
        assert!(matches!(classify_failure(""), ResolveError::Failed(_)));
    }
}
