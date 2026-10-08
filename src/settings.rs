//! The `/settings` page: view and change the request limits and the radio
//! switch while the bot runs. The page's markup, styling and live-status script
//! are the original's; this module fills in the values and validates saves.
//!
//! Saving is the one thing on this server that changes state from a browser, so
//! it is guarded: the Host check (DNS rebinding) applies to every route, and a
//! save must also come from this very page (Origin / Referer / Sec-Fetch-Site),
//! which stops another website from submitting the form on your behalf.

use std::sync::Arc;
use std::time::Instant;

use crate::net::url::Url;
use crate::setup::esc;
use crate::state::Shared;
use crate::store::JsonStore;
use crate::toggles::{self, Toggles};
use crate::tunables::{self, Tunables};

const TEMPLATE: &str = include_str!("../assets/settings.html");
const CSS: &str = include_str!("../assets/settings.css");
const JS: &str = include_str!("../assets/settings.js");

/// Hidden field only this page's own form carries. It tells a browser submitting the whole form
/// (an unchecked box means "off") from a script posting a few fields (absent means "leave alone").
pub const FORM_MARKER: &str = "_settings_form";
/// A settings form is a few hundred bytes; anything big is not one.
pub const MAX_FORM_BYTES: usize = 16 * 1024;

const LABELS: [(&str, &str, &str); 4] = [
    ("max_pending_per_chatter", "Max pending requests per chatter", "How many queued songs one viewer can have waiting at once."),
    ("request_cooldown_seconds", "Request cooldown", "Seconds a viewer must wait between !sr commands. 0 disables the cooldown."),
    ("queue_cap", "Queue cap", "Total requests allowed in the queue before !sr starts turning people away."),
    ("max_request_duration_seconds", "Max track length", "Seconds. Anything longer is refused at request time and skipped if it grows past this later."),
];
const TOGGLE_HELP: [(&str, &str); 1] = [(toggles::RADIO_KEY, "Auto-queue a similar track when the queue runs dry")];

pub struct SettingsPage {
    tunables: Arc<JsonStore>,
    toggles: Arc<JsonStore>,
    shared: Arc<Shared>,
    /// Label / value rows for the "Endpoints" table.
    info: Vec<(String, String)>,
    started: Instant,
}

/// Replaces every `${name}` in `template` in one pass, so inserted values are never rescanned
/// (a song title containing "${js}" must stay text).
pub fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 2048);
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                match values.iter().find(|(k, _)| *k == name) {
                    Some((_, v)) => out.push_str(v),
                    None => out.push_str(&rest[start..start + 3 + end]),
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// May this request change settings? It must name this server as its origin (or carry no origin at
/// all, as curl does) and not be flagged cross-site by the browser.
pub fn origin_ok(origin: Option<&str>, referer: Option<&str>, host: Option<&str>, sec_fetch_site: Option<&str>) -> bool {
    let source = origin.map(str::to_string).or_else(|| {
        let u = Url::parse(referer?)?;
        Some(format!("{}://{}", if u.https { "https" } else { "http" }, u.host_header()))
    });
    let origin_fine = match (source, host.map(|h| h.trim().to_ascii_lowercase()).filter(|h| !h.is_empty())) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(s), Some(h)) => {
            let s = s.to_ascii_lowercase();
            s == format!("http://{h}") || s == format!("https://{h}")
        }
    };
    let fetch_fine = sec_fetch_site.is_none_or(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "same-origin" | "none"));
    origin_fine && fetch_fine
}

fn truthy(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

impl SettingsPage {
    pub fn new(tunables: Arc<JsonStore>, toggles: Arc<JsonStore>, shared: Arc<Shared>, info: Vec<(String, String)>) -> SettingsPage {
        SettingsPage { tunables, toggles, shared, info, started: Instant::now() }
    }

    fn tunable_rows(&self, t: &Tunables) -> String {
        let value = |name: &str| match name {
            "max_pending_per_chatter" => i64::from(t.max_pending_per_chatter),
            "request_cooldown_seconds" => i64::from(t.request_cooldown_seconds),
            "queue_cap" => t.queue_cap as i64,
            _ => i64::from(t.max_request_duration_seconds),
        };
        LABELS
            .iter()
            .map(|(name, label, help)| {
                let (_, lo, hi) = tunables::BOUNDS.iter().copied().find(|(k, ..)| k == name).expect("known tunable");
                format!(
                    "<div class=\"field\"><label for=\"f-{name}\">{label}</label><input id=\"f-{name}\" type=\"number\" name=\"{name}\" value=\"{}\" min=\"{lo}\" max=\"{hi}\" step=\"1\" inputmode=\"numeric\"><p class=\"help\">{} <span class=\"range\">{lo}\u{2013}{hi}</span></p></div>",
                    value(name),
                    esc(help)
                )
            })
            .collect()
    }

    fn toggle_rows(&self, t: &Toggles) -> String {
        TOGGLE_HELP
            .iter()
            .map(|(key, help)| {
                let checked = if *key == toggles::RADIO_KEY && t.radio_autoplay_enabled { "checked" } else { "" };
                format!("<label class=\"switch-row\"><input type=\"checkbox\" name=\"{key}\" {checked}><span class=\"switch\" aria-hidden=\"true\"></span><span class=\"switch-text\"><code>{key}</code><span class=\"help\">{}</span></span></label>", esc(help))
            })
            .collect()
    }

    fn status_chips(&self) -> String {
        let secs = self.started.elapsed().as_secs();
        let (hours, rem) = (secs / 3600, secs % 3600);
        let uptime = if hours > 0 { format!("{hours}h {}m", rem / 60) } else { format!("{}m", rem / 60) };
        let state = self.shared.player_state().as_str();
        let (np_style, np_text, np_title) = match self.shared.now_playing() {
            Some(np) => ("", format!("\u{25b6} {}", esc(&np.title)), esc(&np.title)),
            None => ("display:none", String::new(), String::new()),
        };
        format!(
            "<span class=\"chip state-{state}\" id=\"chip-state\">{state}</span><span class=\"chip\" id=\"chip-queue\">{} queued</span><span class=\"chip\" id=\"chip-uptime\" data-uptime-base=\"{secs}\">up {uptime}</span><span class=\"chip chip-np\" id=\"chip-np\" style=\"{np_style}\" title=\"{np_title}\">{np_text}</span>",
            self.shared.queue_len()
        )
    }

    /// The page. `message` is a banner: (text, is_error).
    pub fn render(&self, message: Option<(&str, bool)>) -> String {
        let t = Tunables::from_map(&self.tunables.read());
        let f = Toggles::from_map(&self.toggles.read());
        let banner = message.map(|(text, error)| format!("<div class=\"banner {}\" role=\"status\">{}</div>", if error { "banner-error" } else { "banner-ok" }, esc(text))).unwrap_or_default();
        let info: String = self.info.iter().map(|(k, v)| format!("<tr><td>{}</td><td><code>{}</code></td></tr>", esc(k), esc(v))).collect();
        let marker = format!("{FORM_MARKER}");
        fill(
            TEMPLATE,
            &[
                ("css", CSS),
                ("js", JS),
                ("status_chips", &self.status_chips()),
                ("banner", &banner),
                ("form_marker", &marker),
                ("tunable_rows", &self.tunable_rows(&t)),
                ("toggle_rows", &self.toggle_rows(&f)),
                ("info_rows", &info),
            ],
        )
    }

    /// Validates every submitted value first, then saves. Nothing is written if anything is wrong.
    pub fn apply(&self, form: &[(String, String)]) -> Result<(), Vec<String>> {
        let get = |k: &str| form.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        let mut current = Tunables::from_map(&self.tunables.read());
        let mut errors = Vec::new();
        for (name, lo, hi) in tunables::BOUNDS {
            let Some(raw) = get(name) else { continue };
            match raw.trim().parse::<i64>() {
                Err(_) => errors.push(format!("{name}: not a number")),
                Ok(v) if v < lo || v > hi => errors.push(format!("{name}: must be between {lo} and {hi}")),
                Ok(v) => match name {
                    "max_pending_per_chatter" => current.max_pending_per_chatter = v as u32,
                    "request_cooldown_seconds" => current.request_cooldown_seconds = v as u32,
                    "queue_cap" => current.queue_cap = v as usize,
                    _ => current.max_request_duration_seconds = v as u32,
                },
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        let full_form = get(FORM_MARKER).is_some();
        let mut switches = Toggles::from_map(&self.toggles.read());
        match get(toggles::RADIO_KEY) {
            Some(v) => switches.radio_autoplay_enabled = if full_form { true } else { truthy(v) },
            None if full_form => switches.radio_autoplay_enabled = false,
            None => {}
        }
        self.tunables.write(current.to_map()).map_err(|e| vec![format!("could not save the limits: {e}")])?;
        self.toggles.write(switches.to_map()).map_err(|e| vec![format!("could not save the switches: {e}")])?;
        crate::info!("Settings updated via /settings: {current:?}, radio autoplay {}", switches.radio_autoplay_enabled);
        Ok(())
    }

    /// The current values, as the page would show them (for tests).
    pub fn current(&self) -> (Tunables, Toggles) {
        (Tunables::from_map(&self.tunables.read()), Toggles::from_map(&self.toggles.read()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(tag: &str) -> SettingsPage {
        let dir = std::env::temp_dir().join(format!("pryxea-settings-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        SettingsPage::new(Arc::new(JsonStore::new(dir.join("tunables.json"))), Arc::new(JsonStore::new(dir.join("toggles.json"))), Arc::new(Shared::new()), vec![("OBS Media Source".into(), "http://127.0.0.1:8098/stream.opus".into())])
    }

    fn form(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn fill_replaces_placeholders_once_and_never_rescans_inserted_text() {
        assert_eq!(fill("a ${x} b ${y} c ${x}", &[("x", "1"), ("y", "${x}")]), "a 1 b ${x} c 1");
        assert_eq!(fill("${unknown} and ${", &[]), "${unknown} and ${");
        assert_eq!(fill("no placeholders", &[("x", "1")]), "no placeholders");
    }

    #[test]
    fn the_origin_must_be_this_page_or_absent() {
        let h = Some("127.0.0.1:8098");
        assert!(origin_ok(Some("http://127.0.0.1:8098"), None, h, Some("same-origin")));
        assert!(origin_ok(None, Some("http://127.0.0.1:8098/settings"), h, None), "the Referer stands in for a missing Origin");
        assert!(origin_ok(None, None, h, None), "scripts such as curl send neither");
        assert!(origin_ok(Some("HTTP://LOCALHOST:8098"), None, Some("localhost:8098"), Some("none")));
        assert!(!origin_ok(Some("https://evil.example"), None, h, Some("cross-site")));
        assert!(!origin_ok(Some("https://evil.example"), None, h, None));
        assert!(!origin_ok(Some("null"), None, h, None), "a sandboxed or redirected page sends Origin: null");
        assert!(!origin_ok(None, Some("https://evil.example/page"), h, None));
        assert!(!origin_ok(Some("http://127.0.0.1:8098"), None, h, Some("cross-site")), "the browser's own verdict counts too");
        assert!(!origin_ok(Some("http://127.0.0.1:8098"), None, h, Some("same-site")));
        assert!(!origin_ok(Some("http://127.0.0.1:8098"), None, None, None));
    }

    #[test]
    fn the_page_shows_current_values_ranges_and_switch_state() {
        let p = page("render");
        p.apply(&form(&[(FORM_MARKER, "1"), ("queue_cap", "77")])).unwrap(); // marker without the checkbox: radio goes off
        let html = p.render(None);
        assert!(html.contains("name=\"queue_cap\" value=\"77\" min=\"1\" max=\"200\""), "{html}");
        assert!(html.contains("name=\"max_pending_per_chatter\" value=\"2\" min=\"1\" max=\"10\""));
        assert!(html.contains("name=\"radio_autoplay_enabled\" >") && !html.contains("name=\"radio_autoplay_enabled\" checked"));
        assert!(html.contains("http://127.0.0.1:8098/stream.opus") && html.contains("name=\"_settings_form\""));
        assert!(!html.contains("${"), "every placeholder must be filled");
        assert!(page("render2").render(None).contains("name=\"radio_autoplay_enabled\" checked"));
    }

    #[test]
    fn banners_and_titles_are_escaped() {
        let p = page("escape");
        let html = p.render(Some(("Nothing was saved \u{2014} <b>x</b>", true)));
        assert!(html.contains("banner-error") && html.contains("&lt;b&gt;x&lt;/b&gt;") && !html.contains("<b>x</b>"));
        p.shared.set_now_playing(Some(crate::state::NowPlaying { title: "<script>alert(1)</script> ${js}".into(), uploader: String::new(), thumbnail_url: None, requester_name: String::new(), webpage_url: String::new(), started_at: Instant::now(), duration_secs: 1 }));
        let html = p.render(None);
        assert!(!html.contains("<script>alert(1)"), "{html}");
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt; ${js}"), "the title must survive as text, unexpanded");
    }

    #[test]
    fn valid_values_are_saved_and_one_bad_value_saves_nothing() {
        let p = page("apply");
        p.apply(&form(&[(FORM_MARKER, "1"), ("max_pending_per_chatter", "5"), ("request_cooldown_seconds", " 30 "), ("queue_cap", "100"), ("max_request_duration_seconds", "900"), ("radio_autoplay_enabled", "on")])).unwrap();
        let (t, f) = p.current();
        assert_eq!((t.max_pending_per_chatter, t.request_cooldown_seconds, t.queue_cap, t.max_request_duration_seconds, f.radio_autoplay_enabled), (5, 30, 100, 900, true));

        let err = p.apply(&form(&[(FORM_MARKER, "1"), ("queue_cap", "5"), ("request_cooldown_seconds", "99999"), ("max_pending_per_chatter", "many")])).unwrap_err();
        assert_eq!(err.len(), 2, "{err:?}");
        assert!(err.iter().any(|e| e.contains("request_cooldown_seconds: must be between 0 and 3600")) && err.iter().any(|e| e.contains("not a number")));
        let (t, f) = p.current();
        assert_eq!((t.queue_cap, f.radio_autoplay_enabled), (100, true), "a rejected form must change nothing, not even its valid fields or the switches");
    }

    #[test]
    fn a_partial_post_only_touches_what_it_names() {
        let p = page("partial");
        // No form marker: a script. The radio switch is not mentioned, so it stays as it was (on by default).
        p.apply(&form(&[("queue_cap", "9")])).unwrap();
        let (t, f) = p.current();
        assert_eq!((t.queue_cap, f.radio_autoplay_enabled), (9, true));
        // Naming it explicitly means what it says.
        p.apply(&form(&[("radio_autoplay_enabled", "false")])).unwrap();
        assert!(!p.current().1.radio_autoplay_enabled);
        p.apply(&form(&[("radio_autoplay_enabled", "yes")])).unwrap();
        assert!(p.current().1.radio_autoplay_enabled);
        assert_eq!(p.current().0.queue_cap, 9);
    }
}
