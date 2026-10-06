//! YouTube URL helpers.

use crate::net::url::Url;

const HOSTS: [&str; 4] = ["youtube.com", "www.youtube.com", "m.youtube.com", "music.youtube.com"];

fn is_video_id(s: &str) -> bool {
    s.len() == 11 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The 11-character video ID from a watch / shorts / youtu.be URL, if it is one.
pub fn video_id(url: &str) -> Option<String> {
    let url = Url::parse(url)?;
    let first_segment = |path: &str| path.split('?').next().unwrap_or("").trim_matches('/').split('/').next().unwrap_or("").to_string();
    if url.host == "youtu.be" {
        let id = first_segment(&url.target);
        return is_video_id(&id).then_some(id);
    }
    if !HOSTS.contains(&url.host.as_str()) {
        return None;
    }
    let (path, query) = url.target.split_once('?').unwrap_or((&url.target, ""));
    if let Some(rest) = path.strip_prefix("/shorts/") {
        let id = rest.trim_matches('/').split('/').next().unwrap_or("").to_string();
        return is_video_id(&id).then_some(id);
    }
    query.split('&').find_map(|kv| kv.strip_prefix("v=")).filter(|v| is_video_id(v)).map(str::to_string)
}

/// Only these hosts may be resolved when a chatter sends a link.
pub fn is_allowed_host(url: &str) -> bool {
    Url::parse(url).is_some_and(|u| u.host == "youtu.be" || HOSTS.contains(&u.host.as_str()))
}

pub fn is_url(s: &str) -> bool {
    let s = s.trim_start();
    s.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("https://")) || s.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("http://"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_ids_from_every_common_shape() {
        let id = Some("dQw4w9WgXcQ".to_string());
        for url in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com/watch?feature=share&v=dQw4w9WgXcQ&t=43",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=RDAMVMdQw4w9WgXcQ",
            "https://m.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ?t=5",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "  https://youtu.be/dQw4w9WgXcQ/",
        ] {
            assert_eq!(video_id(url), id, "{url}");
        }
    }

    #[test]
    fn lookalikes_and_other_sites_give_nothing() {
        for url in [
            "https://www.youtube.com/watch?v=short",
            "https://www.youtube.com/playlist?list=PLabc",
            "https://evil.example/watch?v=dQw4w9WgXcQ",
            "https://youtube.com.evil.example/watch?v=dQw4w9WgXcQ",
            "https://notyoutu.be/dQw4w9WgXcQ",
            "dQw4w9WgXcQ",
            "https://www.youtube.com/watch?v=dQw4w9WgXc%20",
        ] {
            assert_eq!(video_id(url), None, "{url}");
        }
    }

    #[test]
    fn allow_list_is_exact_hosts_only() {
        assert!(is_allowed_host("https://youtu.be/x"));
        assert!(is_allowed_host("https://music.youtube.com/anything"));
        assert!(!is_allowed_host("https://youtube.com.evil.example/"));
        assert!(!is_allowed_host("https://vimeo.com/1"));
        assert!(!is_allowed_host("not a url"));
        assert!(is_url("HTTP://x") && is_url(" https://x") && !is_url("ftp://x") && !is_url("never gonna"));
    }
}
