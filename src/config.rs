//! Settings from environment variables / `.env`. The variable names, defaults
//! and ranges match the Python bot so an existing `.env` carries over as is.
//!
//! Parsing never fails hard: bad optional values fall back to defaults with a
//! warning, and missing or malformed Twitch credentials are collected in
//! `problems` so the app can still start (overlay and stream work) and tell
//! the user what to fix.

use std::path::{Path, PathBuf};

use crate::logging::Level;

pub const DEFAULT_PORT: u16 = 8098;

#[derive(Debug, Clone)]
pub struct Ytdlp {
    /// Explicit yt-dlp executable; otherwise the one in the home folder's `bin/` is used (downloaded on first use).
    pub path: Option<PathBuf>,
    pub concurrency: usize,
    pub extract_timeout_secs: u64,
    pub cache_ttl_secs: u64,
    pub player_clients: Vec<String>,
    pub cookies_file: Option<PathBuf>,
    pub pot_provider_url: Option<String>,
    pub js_runtime_path: Option<PathBuf>,
    pub js_runtime_name: String,
}

/// Overrides for Twitch's service addresses. Only for testing against a local fake; unset in normal use.
#[derive(Debug, Clone, Default)]
pub struct TwitchUrls {
    pub id: Option<String>,
    pub api: Option<String>,
    pub eventsub: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub client_id: String,
    pub client_secret: String,
    pub bot_id: String,
    pub owner_id: String,
    pub port: u16,
    /// Where Twitch sends the browser after authorizing; must match the Twitch application settings.
    pub redirect_uri: String,
    pub twitch_urls: TwitchUrls,
    pub audio_bitrate_kbps: u32,
    pub pause_when_no_listeners: bool,
    pub ytdlp: Ytdlp,
    pub log_level: Level,
    pub log_to_file: bool,
    pub token_file: PathBuf,
    pub tunables_file: PathBuf,
    pub toggles_file: PathBuf,
    pub queue_state_file: PathBuf,
}

#[derive(Debug)]
pub struct Loaded {
    pub settings: Settings,
    pub warnings: Vec<String>,
    /// Reasons the Twitch connection can't start yet. Empty = ready.
    pub problems: Vec<String>,
}

impl Loaded {
    pub fn twitch_ready(&self) -> bool {
        self.problems.is_empty()
    }
}

struct Reader<'a> {
    get: &'a dyn Fn(&str) -> Option<String>,
    warnings: Vec<String>,
}

impl Reader<'_> {
    fn text(&self, name: &str) -> String {
        (self.get)(name).map(|v| v.trim().to_string()).unwrap_or_default()
    }

    fn int(&mut self, name: &str, default: i64) -> i64 {
        let raw = self.text(name);
        if raw.is_empty() {
            return default;
        }
        match raw.parse::<i64>() {
            Ok(v) => v,
            Err(_) => {
                self.warnings.push(format!("{name}={raw:?} is not a valid integer - using {default}."));
                default
            }
        }
    }

    fn clamped(&mut self, name: &str, default: i64, lo: i64, hi: i64) -> i64 {
        let value = self.int(name, default);
        let clamped = value.clamp(lo, hi);
        if clamped != value {
            self.warnings.push(format!("{name}={value} is outside the allowed range {lo}-{hi} - using {clamped}."));
        }
        clamped
    }

    fn bool(&mut self, name: &str, default: bool) -> bool {
        let raw = self.text(name).to_ascii_lowercase();
        match raw.as_str() {
            "" => default,
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => {
                self.warnings.push(format!("{name}={raw:?} is not a recognized boolean - using {default}."));
                default
            }
        }
    }

    /// A file name that lives under `data/`. Directory parts are dropped so a
    /// setting can't point the bot's state files somewhere else.
    fn data_file(&mut self, name: &str, default: &str, data: &Path) -> PathBuf {
        let raw = self.text(name);
        let wanted = if raw.is_empty() { default } else { raw.as_str() };
        match Path::new(wanted).file_name().and_then(|n| n.to_str()) {
            Some(file) if file == wanted => data.join(file),
            Some(file) => {
                self.warnings.push(format!("{name}={wanted:?} must be a plain file name - using {file:?} under data/."));
                data.join(file)
            }
            None => {
                self.warnings.push(format!("{name}={wanted:?} is not a usable file name - using {default}."));
                data.join(default)
            }
        }
    }
}

fn twitch_id(reader: &Reader<'_>, name: &str, problems: &mut Vec<String>) -> String {
    let value = reader.text(name);
    if value.is_empty() {
        problems.push(format!("{name} is not set."));
    } else if !value.chars().all(|c| c.is_ascii_digit()) {
        problems.push(format!("{name} must be digits only (a numeric Twitch user ID, not a username)."));
    }
    value
}

pub fn load(get: &dyn Fn(&str) -> Option<String>, home: &Path, data: &Path, logs_dir: &Path) -> Loaded {
    let mut r = Reader { get, warnings: Vec::new() };
    let mut problems = Vec::new();

    let client_id = r.text("TWITCH_CLIENT_ID");
    if client_id.is_empty() {
        problems.push("TWITCH_CLIENT_ID is not set.".into());
    }
    let client_secret = r.text("TWITCH_CLIENT_SECRET");
    if client_secret.is_empty() {
        problems.push("TWITCH_CLIENT_SECRET is not set.".into());
    }
    let bot_id = twitch_id(&r, "TWITCH_BOT_ID", &mut problems);
    let owner_id = twitch_id(&r, "TWITCH_OWNER_ID", &mut problems);

    let port = r.clamped("TWITCH_NOWPLAYING_PORT", i64::from(DEFAULT_PORT), 1, 65_535) as u16;

    // Songs play at their own volume. The setting is gone; say so instead of silently ignoring it.
    if !r.text("LOUDNESS_MODE").is_empty() {
        r.warnings.push("LOUDNESS_MODE is no longer used: songs always play at their own volume.".into());
    }

    let cookies_raw = r.text("YTDLP_COOKIES_FILE");
    let cookies_file = (!cookies_raw.is_empty()).then(|| home.join(&cookies_raw));
    let player_clients: Vec<String> = r
        .text("YTDLP_PLAYER_CLIENT")
        .split(',')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .collect();
    let js_runtime_name = {
        let n = r.text("YTDLP_JS_RUNTIME_NAME");
        if n.is_empty() { "deno".to_string() } else { n }
    };
    let js_runtime_path = Some(r.text("YTDLP_JS_RUNTIME_PATH")).filter(|p| !p.is_empty()).map(PathBuf::from);
    let pot_provider_url = Some(r.text("YTDLP_POT_PROVIDER_URL")).filter(|u| !u.is_empty());

    let ytdlp = Ytdlp {
        path: Some(r.text("YTDLP_PATH")).filter(|p| !p.is_empty()).map(PathBuf::from),
        concurrency: r.clamped("YTDLP_CONCURRENCY", 2, 1, 8) as usize,
        extract_timeout_secs: r.clamped("YTDLP_EXTRACT_TIMEOUT_SECONDS", 45, 10, 120) as u64,
        cache_ttl_secs: r.clamped("YTDLP_CACHE_TTL_SECONDS", 900, 0, 3600) as u64,
        player_clients,
        cookies_file,
        pot_provider_url,
        js_runtime_path,
        js_runtime_name,
    };

    let log_level = {
        let raw = r.text("LOG_LEVEL");
        if raw.is_empty() {
            Level::Info
        } else {
            Level::parse(&raw).unwrap_or_else(|| {
                r.warnings.push(format!("LOG_LEVEL={raw:?} is not a valid log level - using INFO."));
                Level::Info
            })
        }
    };

    let redirect_uri = {
        let raw = r.text("TWITCH_REDIRECT_URI");
        let default = "http://localhost:4343/oauth/callback".to_string();
        match (raw.is_empty(), crate::net::url::Url::parse(&raw)) {
            (true, _) => default,
            (false, Some(u)) if !u.https && u.host == "localhost" && u.target.ends_with("/oauth/callback") => raw,
            (false, Some(u)) if u.https && u.target.ends_with("/oauth/callback") => raw,
            _ => {
                r.warnings.push(format!("TWITCH_REDIRECT_URI={raw:?} is not a usable http://localhost:<port>/oauth/callback address - using the default."));
                default
            }
        }
    };
    let twitch_urls = {
        let url = |name: &str| Some(r.text(name)).filter(|v| !v.is_empty());
        TwitchUrls { id: url("TWITCH_ID_URL"), api: url("TWITCH_API_URL"), eventsub: url("TWITCH_EVENTSUB_URL") }
    };
    let settings = Settings {
        client_id,
        client_secret,
        bot_id,
        owner_id,
        port,
        redirect_uri,
        twitch_urls,
        audio_bitrate_kbps: r.clamped("AUDIO_BITRATE_KBPS", 160, 64, 256) as u32,
        pause_when_no_listeners: r.bool("PAUSE_QUEUE_WHEN_NO_LISTENERS", false),
        ytdlp,
        log_level,
        log_to_file: r.bool("LOG_TO_FILE", true),
        token_file: r.data_file("TWITCH_TOKEN_FILE", "twitch_tokens.json", data),
        tunables_file: r.data_file("TWITCH_TUNABLES_FILE", "tunables.json", data),
        toggles_file: r.data_file("TWITCH_TOGGLES_FILE", "toggles.json", data),
        queue_state_file: r.data_file("TWITCH_QUEUE_STATE_FILE", "queue_state.json", data),
    };
    let _ = logs_dir;
    Loaded { settings, warnings: r.warnings, problems }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn load_with(pairs: &[(&str, &str)]) -> Loaded {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let get = move |k: &str| map.get(k).cloned();
        load(&get, Path::new("/h"), Path::new("/h/data"), Path::new("/h/logs"))
    }

    const CREDS: [(&str, &str); 4] = [
        ("TWITCH_CLIENT_ID", "cid"),
        ("TWITCH_CLIENT_SECRET", "sec"),
        ("TWITCH_BOT_ID", "123"),
        ("TWITCH_OWNER_ID", "456"),
    ];

    #[test]
    fn defaults_match_the_python_bot() {
        let l = load_with(&CREDS);
        assert!(l.twitch_ready(), "{:?}", l.problems);
        assert!(l.warnings.is_empty(), "{:?}", l.warnings);
        let s = l.settings;
        assert_eq!(s.port, 8098);
        assert_eq!(s.redirect_uri, "http://localhost:4343/oauth/callback");
        assert_eq!(s.audio_bitrate_kbps, 160);
        assert!(!s.pause_when_no_listeners);
        assert_eq!((s.ytdlp.concurrency, s.ytdlp.extract_timeout_secs), (2, 45));
        assert_eq!(s.ytdlp.cache_ttl_secs, 900);
        assert!(s.ytdlp.path.is_none());
        assert_eq!(s.tunables_file, PathBuf::from("/h/data/tunables.json"));
        assert_eq!(s.queue_state_file, PathBuf::from("/h/data/queue_state.json"));
    }

    #[test]
    fn out_of_range_values_clamp_with_a_warning() {
        let mut pairs = CREDS.to_vec();
        pairs.extend([("AUDIO_BITRATE_KBPS", "999"), ("YTDLP_CONCURRENCY", "0"), ("YTDLP_CACHE_TTL_SECONDS", "x")]);
        let l = load_with(&pairs);
        assert_eq!(l.settings.audio_bitrate_kbps, 256);
        assert_eq!(l.settings.ytdlp.concurrency, 1);
        assert_eq!(l.settings.ytdlp.cache_ttl_secs, 900);
        assert_eq!(l.warnings.len(), 3);
    }

    #[test]
    fn missing_and_malformed_credentials_are_reported_not_fatal() {
        let l = load_with(&[("TWITCH_CLIENT_ID", "cid"), ("TWITCH_BOT_ID", "streamer_name")]);
        assert!(!l.twitch_ready());
        assert_eq!(l.problems.len(), 3, "{:?}", l.problems);
        assert!(l.problems.iter().any(|p| p.contains("TWITCH_BOT_ID must be digits")));
    }

    #[test]
    fn the_redirect_uri_must_be_a_local_oauth_callback() {
        let mut ok = CREDS.to_vec();
        ok.push(("TWITCH_REDIRECT_URI", "http://localhost:5000/oauth/callback"));
        let l = load_with(&ok);
        assert_eq!(l.settings.redirect_uri, "http://localhost:5000/oauth/callback");
        assert!(l.warnings.is_empty());
        for bad in ["http://evil.example/oauth/callback", "http://localhost:5000/elsewhere", "not a url"] {
            let mut pairs = CREDS.to_vec();
            pairs.push(("TWITCH_REDIRECT_URI", bad));
            let l = load_with(&pairs);
            assert_eq!(l.settings.redirect_uri, "http://localhost:4343/oauth/callback", "{bad}");
            assert_eq!(l.warnings.len(), 1, "{bad}");
        }
    }

    #[test]
    fn the_old_loudness_setting_is_ignored_with_a_notice() {
        let mut pairs = CREDS.to_vec();
        pairs.push(("LOUDNESS_MODE", "static"));
        let l = load_with(&pairs);
        assert_eq!(l.warnings.len(), 1);
        assert!(l.warnings[0].contains("own volume"), "{:?}", l.warnings);
    }

    #[test]
    fn state_files_cannot_escape_the_data_folder() {
        let mut pairs = CREDS.to_vec();
        pairs.push(("TWITCH_TUNABLES_FILE", "../../etc/tunables.json"));
        let l = load_with(&pairs);
        assert_eq!(l.settings.tunables_file, PathBuf::from("/h/data/tunables.json"));
        assert_eq!(l.warnings.len(), 1);
    }
}
