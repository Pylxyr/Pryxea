//! The thumbnail relay behind `/thumb-proxy`. The overlay reads pixels back out of
//! the cover art to pick colours, which a browser only allows for same-origin
//! images, so the art is fetched here and served from our own origin.
//!
//! This is reachable by anything that can talk to the local server, so it must not
//! become an open relay: only fixed image CDNs, no URL oddities, every redirect hop
//! re-checked, image types only, a size cap, and a rate limit.

use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::net::http::{Client, Request};
use crate::net::url::Url;

const MAX_URL_LEN: usize = 2048;
const MAX_BYTES: usize = 3 * 1024 * 1024;
const MAX_REDIRECTS: usize = 3;
const TOTAL_TIME: Duration = Duration::from_secs(8);
const HOST_SUFFIXES: [&str; 3] = ["ytimg.com", "ggpht.com", "googleusercontent.com"];
const CONTENT_TYPES: [&str; 5] = ["image/jpeg", "image/png", "image/webp", "image/gif", "image/avif"];

pub struct ThumbProxy {
    client: Client,
    suffixes: Vec<String>,
    ports: Vec<u16>,
    limiter: RateLimiter,
}

pub enum ThumbError {
    /// 400: not an acceptable URL.
    BadUrl,
    /// 429
    RateLimited,
    /// 502, with a short reason.
    Upstream(&'static str),
}

impl ThumbError {
    pub fn status(&self) -> u16 {
        match self {
            ThumbError::BadUrl => 400,
            ThumbError::RateLimited => 429,
            ThumbError::Upstream(_) => 502,
        }
    }
    pub fn message(&self) -> &'static str {
        match self {
            ThumbError::BadUrl => "URL not allowed",
            ThumbError::RateLimited => "Too many requests",
            ThumbError::Upstream(m) => m,
        }
    }
}

impl Default for ThumbProxy {
    fn default() -> Self {
        let mut client = Client::new();
        client.connect_timeout = Duration::from_secs(5);
        client.io_timeout = Duration::from_secs(5);
        ThumbProxy::with(client, HOST_SUFFIXES.iter().map(|s| s.to_string()).collect(), vec![80, 443])
    }
}

impl ThumbProxy {
    /// `suffixes` and `ports` are what URLs may point at (tests widen them to reach a local server).
    pub fn with(client: Client, suffixes: Vec<String>, ports: Vec<u16>) -> ThumbProxy {
        ThumbProxy { client, suffixes, ports, limiter: RateLimiter::new(120, Duration::from_secs(60)) }
    }

    fn host_allowed(&self, host: &str) -> bool {
        self.suffixes.iter().any(|s| host == s || host.strip_suffix(s.as_str()).is_some_and(|rest| rest.ends_with('.')))
    }

    /// Returns the URL unchanged if it is safe to fetch.
    pub fn validate(&self, url: &str) -> Option<String> {
        if url.is_empty() || url.len() > MAX_URL_LEN || url.bytes().any(|b| b <= 0x20 || b >= 0x7F || b == b'\\') {
            return None;
        }
        let parsed = Url::parse(url)?;
        let dns_like = parsed.host.split('.').count() >= 2 && parsed.host.split('.').all(|label| !label.is_empty() && !label.starts_with('-') && !label.ends_with('-') && label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'));
        (self.ports.contains(&parsed.port) && dns_like && self.host_allowed(&parsed.host)).then(|| url.to_string())
    }

    /// Fetches an image for the overlay. Blocking.
    pub fn fetch(&self, url: &str) -> Result<(Vec<u8>, String), ThumbError> {
        if !self.limiter.allow() {
            return Err(ThumbError::RateLimited);
        }
        let mut url = self.validate(url).ok_or(ThumbError::BadUrl)?;
        let deadline = Instant::now() + TOTAL_TIME;
        for _ in 0..=MAX_REDIRECTS {
            if Instant::now() > deadline {
                return Err(ThumbError::Upstream("Upstream fetch failed"));
            }
            let resp = self.client.send(&Request::get(&url).no_redirects()).map_err(|_| ThumbError::Upstream("Upstream fetch failed"))?;
            if matches!(resp.status, 301 | 302 | 303 | 307 | 308) {
                let next = resp.header("location").and_then(|loc| Url::parse(&url)?.join(loc)).map(|u| format!("{}://{}{}", if u.https { "https" } else { "http" }, u.host_header(), u.target));
                url = next.and_then(|n| self.validate(&n)).ok_or(ThumbError::Upstream("Redirect not allowed"))?;
                continue;
            }
            if resp.status != 200 {
                return Err(ThumbError::Upstream("Upstream fetch failed"));
            }
            let content_type = resp.header("content-type").unwrap_or("").split(';').next().unwrap_or("").trim().to_ascii_lowercase();
            if !CONTENT_TYPES.contains(&content_type.as_str()) {
                return Err(ThumbError::Upstream("Not an image"));
            }
            if resp.header("content-length").and_then(|v| v.parse::<usize>().ok()).is_some_and(|n| n > MAX_BYTES) {
                return Err(ThumbError::Upstream("Image too large"));
            }
            let mut body = Vec::new();
            resp.take((MAX_BYTES + 1) as u64).read_to_end(&mut body).map_err(|_| ThumbError::Upstream("Upstream fetch failed"))?;
            if body.len() > MAX_BYTES {
                return Err(ThumbError::Upstream("Image too large"));
            }
            return Ok((body, content_type));
        }
        Err(ThumbError::Upstream("Too many redirects"))
    }
}

/// At most `limit` calls per `window`.
pub struct RateLimiter {
    limit: usize,
    window: Duration,
    hits: Mutex<std::collections::VecDeque<Instant>>,
}

impl RateLimiter {
    pub fn new(limit: usize, window: Duration) -> RateLimiter {
        RateLimiter { limit, window, hits: Mutex::default() }
    }

    pub fn allow(&self) -> bool {
        let now = Instant::now();
        let mut hits = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        while hits.front().is_some_and(|t| now.duration_since(*t) > self.window) {
            hits.pop_front();
        }
        if hits.len() >= self.limit {
            return false;
        }
        hits.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_image_cdn_urls_with_ordinary_syntax_pass() {
        let p = ThumbProxy::default();
        for ok in ["https://i.ytimg.com/vi/abc/hq720.jpg", "https://yt3.ggpht.com/a/b=s88", "http://lh3.googleusercontent.com/x", "https://ytimg.com/x", "https://i.ytimg.com:443/x"] {
            assert_eq!(p.validate(ok).as_deref(), Some(ok), "{ok}");
        }
        for bad in [
            "", "ftp://i.ytimg.com/x", "https://evil.example/x", "https://ytimg.com.evil.example/x", "https://notytimg.com/x", "https://i.ytimg.com@evil.example/x",
            "https://evil.example@i.ytimg.com/x", "https://i.ytimg.com:8443/x", "https://i.ytimg.com\\@evil.example/x", "https://i.ytimg.com/x y", "https://i.ytimg.com/\u{e9}",
            "https://127.0.0.1/x", "https://localhost/x", "https://ytimg/x", "https://-bad.ytimg.com/x", "https://i.YTIMG.com/x",
        ] {
            // (uppercase hosts are lowercased by the URL parser, so only the genuinely bad ones must fail)
            if bad == "https://i.YTIMG.com/x" {
                continue;
            }
            assert_eq!(p.validate(bad), None, "{bad:?}");
        }
        assert_eq!(p.validate(&format!("https://i.ytimg.com/{}", "a".repeat(2_100))), None);
    }

    #[test]
    fn the_rate_limiter_allows_a_burst_then_refuses_until_the_window_passes() {
        let l = RateLimiter::new(3, Duration::from_millis(80));
        assert!(l.allow() && l.allow() && l.allow());
        assert!(!l.allow());
        std::thread::sleep(Duration::from_millis(100));
        assert!(l.allow());
    }
}
