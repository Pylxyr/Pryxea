//! Chat commands. Exactly five exist: `!sr`, `!skip`, `!nowplaying`, `!sq`
//! and `!radio`. Parsing is a plain function (no framework); everything the
//! bot says back is built by a pure function here so it can be tested
//! without a Twitch connection.

/// Twitch rejects chat messages over 500 characters.
pub const MAX_REPLY_CHARS: usize = 500;
/// How much of a chatter's query the "Looking up ..." acknowledgement repeats.
pub const MAX_ECHO_CHARS: usize = 80;

pub const USAGE_SR: &str = "Usage: !sr <song name or URL>";
pub const USAGE_RADIO: &str = "Usage: !radio [on|off]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `!sr <query>`. An empty query is kept so the caller can reply with the usage line.
    Sr(String),
    Skip,
    NowPlaying,
    /// `!sq` - the song queue.
    Sq,
    /// `!radio` reports status; `!radio on|off` (mods only) changes it.
    Radio(RadioArg),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadioArg {
    Status,
    On,
    Off,
    Invalid,
}

/// Characters that show up at the end of chat lines without being typed:
/// 7TV appends U+E0000 to dodge Twitch's duplicate-message filter.
fn is_noise(c: char) -> bool {
    c.is_whitespace() || c == '\u{e0000}' || c == '\u{200b}' || c == '\u{feff}'
}

pub fn parse(message: &str) -> Option<Command> {
    let message = message.trim_matches(is_noise);
    let rest = message.strip_prefix('!')?;
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim_matches(is_noise)),
        None => (rest, ""),
    };
    match name.to_ascii_lowercase().as_str() {
        "sr" => Some(Command::Sr(args.to_string())),
        "skip" => Some(Command::Skip),
        "nowplaying" => Some(Command::NowPlaying),
        "sq" => Some(Command::Sq),
        "radio" => Some(Command::Radio(match args.to_ascii_lowercase().as_str() {
            "" => RadioArg::Status,
            "on" => RadioArg::On,
            "off" => RadioArg::Off,
            _ => RadioArg::Invalid,
        })),
        _ => None,
    }
}

/// Cuts to `max` characters, ending in an ellipsis when something was removed.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

pub fn clamp_reply(s: &str) -> String {
    truncate_chars(s, MAX_REPLY_CHARS)
}

/// `Looking up 'query'...`, echoing only a short prefix so nobody can make the
/// bot post up to 500 characters of their choosing.
pub fn looking_up(query: &str) -> String {
    format!("Looking up {:?}\u{2026}", truncate_chars(query, MAX_ECHO_CHARS))
}

pub fn queued(title: &str, position: usize) -> String {
    clamp_reply(&format!("Queued: {title} (#{position} in queue)"))
}

pub fn now_playing(title: &str, requester: &str, elapsed_secs: u64) -> String {
    clamp_reply(&format!("Now playing: {title} \u{2014} requested by {requester} ({elapsed_secs}s in)"))
}

pub const NOTHING_PLAYING: &str = "Nothing's playing right now.";

/// `!sq`: the first three titles and a count of the rest.
pub fn queue_summary<'a>(titles: impl ExactSizeIterator<Item = &'a str>) -> String {
    let total = titles.len();
    if total == 0 {
        return "Queue is empty.".to_string();
    }
    let upcoming: Vec<&str> = titles.take(3).map(|t| if t.is_empty() { "an unnamed track" } else { t }).collect();
    let more = if total > 3 { format!(" (+{} more)", total - 3) } else { String::new() };
    clamp_reply(&format!("{total} queued: {}{more}", upcoming.join(", ")))
}

pub fn radio_status(on: bool) -> String {
    format!("Radio autoplay is {}.", if on { "on" } else { "off" })
}

pub fn radio_changed(on: bool) -> String {
    format!("Radio autoplay is now {}.", if on { "on" } else { "off" })
}

pub const RADIO_MODS_ONLY: &str = "Only mods can change that \u{2014} try !radio with no argument to check status.";
pub const SKIP_OWN_ONLY: &str = "You can only skip your own song \u{2014} mods can skip anything.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_five_commands_are_recognised() {
        assert_eq!(parse("!sr never gonna give you up"), Some(Command::Sr("never gonna give you up".into())));
        assert_eq!(parse("!skip"), Some(Command::Skip));
        assert_eq!(parse("!nowplaying"), Some(Command::NowPlaying));
        assert_eq!(parse("!sq"), Some(Command::Sq));
        assert_eq!(parse("!radio"), Some(Command::Radio(RadioArg::Status)));
        assert_eq!(parse("!radio ON"), Some(Command::Radio(RadioArg::On)));
        assert_eq!(parse("!radio off"), Some(Command::Radio(RadioArg::Off)));
        assert_eq!(parse("!radio maybe"), Some(Command::Radio(RadioArg::Invalid)));
    }

    #[test]
    fn every_removed_command_and_alias_is_ignored() {
        for gone in [
            "!songrequest x", "!np", "!queue", "!pause", "!resume", "!unpause", "!voteskip", "!vs", "!remove 1",
            "!cancel", "!unqueue", "!position", "!pos", "!block x", "!unblock x", "!blocklist", "!banlist",
            "!srx", "!skipp", "sr hello", "", "!", "hello !sr x",
        ] {
            assert_eq!(parse(gone), None, "{gone:?} should not parse");
        }
    }

    #[test]
    fn names_are_case_insensitive_and_noise_is_trimmed() {
        assert_eq!(parse("!SR  hello  "), Some(Command::Sr("hello".into())));
        assert_eq!(parse("  !Skip\u{e0000}"), Some(Command::Skip));
        assert_eq!(parse("!sr\u{e0000}"), Some(Command::Sr(String::new())));
        // Tabs and newlines separate the name from the arguments too.
        assert_eq!(parse("!sr\tfoo bar"), Some(Command::Sr("foo bar".into())));
    }

    #[test]
    fn sr_keeps_urls_and_inner_spacing_intact() {
        let q = "https://youtu.be/dQw4w9WgXcQ?t=43";
        assert_eq!(parse(&format!("!sr {q}")), Some(Command::Sr(q.into())));
        assert_eq!(parse("!sr a   b"), Some(Command::Sr("a   b".into())));
    }

    #[test]
    fn replies_match_the_python_bot() {
        assert_eq!(queue_summary(Vec::<&str>::new().into_iter()), "Queue is empty.");
        assert_eq!(queue_summary(["A", "B"].into_iter()), "2 queued: A, B");
        assert_eq!(queue_summary(["A", "B", "C", "D", "E"].into_iter()), "5 queued: A, B, C (+2 more)");
        assert_eq!(queue_summary(["A", ""].into_iter()), "2 queued: A, an unnamed track");
        assert_eq!(queued("Song", 3), "Queued: Song (#3 in queue)");
        assert_eq!(now_playing("Song", "Ann", 42), "Now playing: Song \u{2014} requested by Ann (42s in)");
        assert_eq!(radio_status(true), "Radio autoplay is on.");
        assert_eq!(radio_changed(false), "Radio autoplay is now off.");
    }

    #[test]
    fn replies_never_exceed_twitchs_limit_and_echo_is_short() {
        let long = "x".repeat(2000);
        assert_eq!(queued(&long, 1).chars().count(), MAX_REPLY_CHARS);
        let echo = looking_up(&long);
        // `Looking up "<80 chars incl. ellipsis>"…`
        assert_eq!(echo.chars().count(), "Looking up ".len() + 2 + MAX_ECHO_CHARS + 1);
        assert!(echo.ends_with("\u{2026}\"\u{2026}"), "{echo}");
        assert_eq!(looking_up("short one"), "Looking up \"short one\"\u{2026}");
        // Multi-byte characters are cut on character boundaries, not bytes.
        let emoji = "\u{1F3B5}".repeat(100);
        assert_eq!(truncate_chars(&emoji, 10).chars().count(), 10);
    }
}
