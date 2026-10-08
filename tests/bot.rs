mod common;

use std::time::Duration;

use common::rig::*;
use pryxea::bot::Bot;
use pryxea::station::{QueueEntry, RADIO_NAME};
use pryxea::twitch::eventsub::ChatMessage;

const BOT_ID: &str = "42";

fn chat_msg(id: &str, user: &str, name: &str, text: &str, is_mod: bool) -> ChatMessage {
    ChatMessage { message_id: id.into(), chatter_id: user.into(), login: name.to_lowercase(), display_name: name.into(), text: text.into(), is_mod }
}

fn bot_for(r: &Rig) -> Bot {
    Bot::new(r.station.clone(), r.chat.clone(), BOT_ID.into())
}

#[tokio::test]
async fn sr_runs_the_whole_request_flow_through_chat() {
    let mut r = rig("bot-sr", false);
    r.lookup.add("never gonna", track("Song A", A, 2));
    let bot = bot_for(&r);
    bot.handle(&chat_msg("m1", "7", "Ann", "!sr never gonna", false));
    let ack = r.out().await;
    assert_eq!((ack.text.as_str(), ack.reply_to.as_deref()), ("Looking up \"never gonna\"\u{2026}", Some("m1")));
    assert_eq!(r.out().await.text, "Queued: Song A (#1 in queue)");
    assert_eq!(r.out().await.text, "Ann's song request is Now Playing: Song A");
    bot.handle(&chat_msg("m2", "8", "Bo", "!nowplaying", false));
    let np = r.out().await;
    assert!(np.text.starts_with("Now playing: Song A \u{2014} requested by Ann ("), "{}", np.text);
    assert_eq!(np.reply_to.as_deref(), Some("m2"));
}

#[tokio::test]
async fn usage_hints_and_nothing_playing_replies() {
    let mut r = rig("bot-usage", true);
    let bot = bot_for(&r);
    for (text, expect) in [
        ("!sr", "Usage: !sr <song name or URL>"),
        ("!SR   ", "Usage: !sr <song name or URL>"),
        ("!nowplaying", "Nothing's playing right now."),
        ("!skip", "Nothing's playing right now."),
        ("!sq", "Queue is empty."),
        ("!radio", "Radio autoplay is on."),
        ("!radio maybe", "Usage: !radio [on|off]"),
    ] {
        bot.handle(&chat_msg("m", "7", "Ann", text, true));
        assert_eq!(r.out().await.text, expect, "{text}");
    }
}

#[tokio::test]
async fn sq_lists_the_first_three_and_counts_the_rest() {
    let mut r = rig("bot-sq", true);
    let bot = bot_for(&r);
    for (i, title) in ["One", "Two", "Three", "Four", "Five"].iter().enumerate() {
        r.station.enqueue(QueueEntry { seq: 0, webpage_url: format!("https://www.youtube.com/watch?v={i:_>11}"), requester_id: 10 + i as u64, requester_name: "x".into(), title: (*title).into(), uploader: String::new() });
    }
    bot.handle(&chat_msg("m", "7", "Ann", "!sq", false));
    assert_eq!(r.out().await.text, "5 queued: One, Two, Three (+2 more)");
}

#[tokio::test]
async fn skip_rules_and_replies() {
    let mut r = rig("bot-skip", false);
    r.lookup.add("a", track("Song A", A, 100));
    let bot = bot_for(&r);
    bot.handle(&chat_msg("m1", "7", "Ann", "!sr a", false));
    r.out_containing("Ann's song request is Now Playing").await;
    bot.handle(&chat_msg("m2", "8", "Bo", "!skip", false));
    assert_eq!(r.out().await.text, "You can only skip your own song \u{2014} mods can skip anything.");
    bot.handle(&chat_msg("m3", "9", "Cy", "!skip", true));
    assert_eq!(r.out().await.text, "Skipped.");
    r.wait_until("the song to stop", |r| r.title().is_none()).await;
}

#[tokio::test]
async fn radio_can_be_read_by_anyone_but_changed_only_by_mods() {
    let mut r = rig("bot-radio", true);
    let bot = bot_for(&r);
    bot.handle(&chat_msg("m1", "7", "Ann", "!radio off", false));
    assert_eq!(r.out().await.text, "Only mods can change that \u{2014} try !radio with no argument to check status.");
    assert!(r.station.radio_status(), "a refused change must not change anything");
    bot.handle(&chat_msg("m2", "9", "Cy", "!radio off", true));
    assert_eq!(r.out().await.text, "Radio autoplay is now off.");
    bot.handle(&chat_msg("m3", "7", "Ann", "!radio", false));
    assert_eq!(r.out().await.text, "Radio autoplay is off.");
    bot.handle(&chat_msg("m4", "9", "Cy", "!radio ON", true));
    assert_eq!(r.out().await.text, "Radio autoplay is now on.");
}

#[tokio::test]
async fn removed_commands_and_the_bots_own_messages_are_ignored() {
    let mut r = rig("bot-ignored", true);
    let bot = bot_for(&r);
    for text in ["!pause", "!resume", "!voteskip", "!remove", "!position", "!block x", "!unblock x", "!blocklist", "!queue", "!np", "!songrequest x", "hello chat", "sr never gonna"] {
        bot.handle(&chat_msg("m", "7", "Ann", text, true));
    }
    // The bot answering itself must never start a loop.
    bot.handle(&chat_msg("m", BOT_ID, "PryxeaBot", "!sq", true));
    assert!(tokio::time::timeout(Duration::from_millis(300), r.chat_rx.recv()).await.is_err(), "nothing may be said in reply to those");
    // A chatter id that isn't a number is refused politely, not crashed on.
    bot.handle(&chat_msg("m9", "not-a-number", "Weird", "!sq", false));
    assert_eq!(r.out().await.text, "Couldn't identify you \u{2014} try again.");
}

#[tokio::test]
async fn display_names_fall_back_to_the_login() {
    let mut r = rig("bot-names", false);
    r.lookup.add("a", track("Song A", A, 2));
    let bot = bot_for(&r);
    let mut m = chat_msg("m1", "7", "", "!sr a", false);
    m.login = "ann_lower".into();
    bot.handle(&m);
    r.out_containing("ann_lower's song request is Now Playing").await;
    let _ = RADIO_NAME;
}
