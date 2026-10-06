//! Just enough URL handling for http(s) requests and redirects.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub https: bool,
    pub host: String,
    pub port: u16,
    /// Path plus query, always starting with '/'.
    pub target: String,
}

impl Url {
    pub fn parse(s: &str) -> Option<Url> {
        let s = s.trim();
        let (https, rest) = if let Some(r) = strip_prefix_ci(s, "https://") {
            (true, r)
        } else if let Some(r) = strip_prefix_ci(s, "http://") {
            (false, r)
        } else {
            return None;
        };
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        if authority.is_empty() || authority.contains('@') || authority.contains(char::is_whitespace) {
            return None;
        }
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (addr, after) = v6.split_once(']')?;
            let port = match after.strip_prefix(':') {
                Some(p) => p.parse().ok()?,
                None if after.is_empty() => default_port(https),
                None => return None,
            };
            (addr.to_string(), port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), p.parse().ok()?),
                None => (authority.to_string(), default_port(https)),
            }
        };
        if host.is_empty() || port == 0 {
            return None;
        }
        let tail = tail.split('#').next().unwrap_or("");
        let target = match tail {
            "" => "/".to_string(),
            t if t.starts_with('/') => t.to_string(),
            t => format!("/{t}"), // "?query" with no path
        };
        if target.bytes().any(|b| b <= b' ' || b == 0x7f) {
            return None;
        }
        Some(Url { https, host: host.to_ascii_lowercase(), port, target })
    }

    /// The value of the Host header.
    pub fn host_header(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if self.port == default_port(self.https) { host } else { format!("{host}:{}", self.port) }
    }

    pub fn same_origin(&self, other: &Url) -> bool {
        self.https == other.https && self.host == other.host && self.port == other.port
    }

    /// Resolves a redirect `Location` against this URL.
    pub fn join(&self, location: &str) -> Option<Url> {
        let location = location.trim();
        if location.contains("://") {
            return Url::parse(location);
        }
        let scheme = if self.https { "https" } else { "http" };
        let origin = format!("{scheme}://{}", self.host_header());
        if location.starts_with("//") {
            return Url::parse(&format!("{scheme}:{location}"));
        }
        if location.starts_with('/') {
            return Url::parse(&format!("{origin}{location}"));
        }
        let dir = self.target.split('?').next().unwrap_or("/");
        let dir = &dir[..dir.rfind('/').map_or(0, |i| i + 1)];
        Url::parse(&format!("{origin}{dir}{location}"))
    }
}

fn default_port(https: bool) -> u16 {
    if https { 443 } else { 80 }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    s.get(..prefix.len()).filter(|p| p.eq_ignore_ascii_case(prefix)).map(|_| &s[prefix.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_shapes() {
        let u = Url::parse("https://Example.com/a/b?x=1#frag").unwrap();
        assert_eq!((u.https, u.host.as_str(), u.port, u.target.as_str()), (true, "example.com", 443, "/a/b?x=1"));
        let u = Url::parse("http://127.0.0.1:8098").unwrap();
        assert_eq!((u.port, u.target.as_str(), u.host_header().as_str()), (8098, "/", "127.0.0.1:8098"));
        let u = Url::parse("http://[::1]:9/x").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.host_header().as_str()), ("::1", 9, "[::1]:9"));
        assert_eq!(Url::parse("https://h?q=1").unwrap().target, "/?q=1");
        assert_eq!(Url::parse("HTTPS://h/").unwrap().host_header(), "h");
    }

    #[test]
    fn rejects_what_it_cannot_handle_safely() {
        for bad in ["ftp://h/", "h/x", "https://", "https://user@h/", "https://h:0/", "https://h:99999/", "https://h/a b", "https://h/\r\nX: y", "javascript:alert(1)"] {
            assert!(Url::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn redirects_resolve_like_a_browser() {
        let base = Url::parse("https://a.test:8443/dir/file?x=1").unwrap();
        assert_eq!(base.join("https://b.test/z").unwrap().host, "b.test");
        assert_eq!(base.join("/abs?q").unwrap().target, "/abs?q");
        assert_eq!(base.join("rel.bin").unwrap().target, "/dir/rel.bin");
        assert_eq!(base.join("//c.test/p").unwrap().host, "c.test");
        assert!(base.join("/abs").unwrap().same_origin(&base));
        assert!(!base.join("https://a.test/abs").unwrap().same_origin(&base), "different port");
    }
}
