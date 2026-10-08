mod common;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::rig::*;
use pryxea::station::{QueueEntry, RADIO_NAME, SkipOutcome};
use pryxea::ytdlp::{MixEntry, ResolveError};

// ----------------------------------------------------------------- the flow

#[tokio::test]
async fn a_request_is_acknowledged_queued_played_and_announced() {
    let mut r = rig("flow", false);
    r.lookup.add("never gonna", track("Song A", A, 2));
    let ack = r.station.request(&who(7, "Ann"), "never gonna", r.replier());
    assert_eq!(ack, "Looking up \"never gonna\"\u{2026}");
    let queued = r.out().await;
    assert_eq!((queued.text.as_str(), queued.reply_to.as_deref()), ("Queued: Song A (#1 in queue)", Some("msg-1")));
    let now = r.out().await;
    assert_eq!((now.text.as_str(), now.reply_to), ("Ann's song request is Now Playing: Song A", None), "now-playing goes to the channel, not as a reply");
    assert_eq!(r.title().as_deref(), Some("Song A"));
    let j = r.shared.nowplaying_json();
    assert_eq!((j["playing"].clone(), j["state"].clone(), j["requester_name"].clone()), (true.into(), "playing".into(), "Ann".into()));
    assert_eq!(r.station.active_requester_id(), Some(7));
    r.wait_until("the song to end", |r| r.title().is_none() && r.station.active_requester_id().is_none()).await;
    assert_eq!(r.shared.nowplaying_json()["state"], "idle");
    assert_eq!(r.shared.counters.count_last_hour("tracks_played"), 1);
}

#[tokio::test]
async fn limits_and_refusals_give_the_right_replies() {
    let mut r = rig("limits", true); // nothing starts playing, so the queue just fills
    r.set_tunables(serde_json::json!({"max_pending_per_chatter": 2, "request_cooldown_seconds": 0, "queue_cap": 3, "max_request_duration_seconds": 300}));
    r.lookup.add("a", track("Song A", A, 100));
    r.lookup.add("b", track("Song B", B, 100));
    r.lookup.add("long", track("Long Song", C, 900));
    r.lookup.add_err("live", ResolveError::Live);
    r.lookup.add_err("boom", ResolveError::Failed("x".into()));
    r.lookup.add_err("https://vimeo.com/1", ResolveError::UnsupportedSource);

    let ann = who(7, "Ann");
    assert!(r.station.request(&ann, "a", r.replier()).starts_with("Looking up"));
    assert_eq!(r.out().await.text, "Queued: Song A (#1 in queue)");
    assert_eq!(r.station.request(&ann, "a", r.replier()).trim(), "Looking up \"a\"\u{2026}"); // a re-request is allowed to start...
    assert_eq!(r.out().await.text, "Song A is already queued.", "...and is refused as a duplicate once resolved");
    assert!(r.station.request(&ann, "b", r.replier()).starts_with("Looking up"));
    assert_eq!(r.out().await.text, "Queued: Song B (#2 in queue)");
    // Ann now has two queued: the per-chatter cap applies before any lookup.
    assert_eq!(r.station.request(&ann, "long", r.replier()), "You already have 2 request(s) queued \u{2014} wait for one to play first.");

    let bo = who(8, "Bo");
    assert!(r.station.request(&bo, "long", r.replier()).starts_with("Looking up"));
    assert_eq!(r.out().await.text, "That's too long to queue \u{2014} max is 5 minute(s).");
    for (query, expect) in [("live", "Can't queue a livestream \u{2014} sorry!"), ("boom", "Couldn't fetch that \u{2014} try a different search or link."), ("https://vimeo.com/1", "Only YouTube links and searches are supported."), ("nothing", "No results for that.")] {
        r.station.request(&bo, query, r.replier());
        assert_eq!(r.out().await.text, expect, "{query}");
    }
    // Refused lookups don't use up Bo's slots.
    r.lookup.add("c", track("Song C", C, 100));
    r.station.request(&bo, "c", r.replier());
    assert_eq!(r.out().await.text, "Queued: Song C (#3 in queue)");
    assert_eq!(r.station.request(&who(9, "Cy"), "anything", r.replier()), "Queue's full right now \u{2014} try again in a bit.");
    assert_eq!(r.station.request(&who(9, "Cy"), "   ", r.replier()), "Usage: !sr <song name or URL>");
}

#[tokio::test]
async fn cooldown_and_double_taps_are_handled_before_any_lookup() {
    let mut r = rig("cooldown", true);
    r.set_tunables(serde_json::json!({"request_cooldown_seconds": 60, "max_pending_per_chatter": 5}));
    *r.lookup.delay.lock().unwrap() = Duration::from_millis(300);
    r.lookup.add("a", track("Song A", A, 100));
    let ann = who(7, "Ann");
    assert!(r.station.request(&ann, "a", r.replier()).starts_with("Looking up"));
    // Same query while the first is still resolving.
    assert_eq!(r.station.request(&ann, "A", r.replier()), "Still looking that up \u{2014} hang tight!");
    let _ = r.out().await;
    // A different query right after: inside the cooldown.
    let reply = r.station.request(&ann, "something else", r.replier());
    assert!(reply.starts_with("Slow down \u{2014} try again in "), "{reply}");
    assert_eq!(r.lookup.resolves.lock().unwrap().len(), 1, "refused requests must not reach the lookup");
}

#[tokio::test]
async fn real_requests_jump_ahead_of_radio_filler_but_not_each_other_and_only_real_ones_are_saved() {
    let r = rig("order", true);
    let entry = |id: u64, title: &str| QueueEntry { seq: 0, webpage_url: format!("https://www.youtube.com/watch?v={title:_<11}"), requester_id: id, requester_name: if id == 0 { RADIO_NAME.into() } else { format!("u{id}") }, title: title.into(), uploader: String::new() };
    r.station.enqueue(entry(0, "radio1"));
    r.station.enqueue(entry(1, "real1"));
    r.station.enqueue(entry(0, "radio2"));
    r.station.enqueue(entry(2, "real2"));
    assert_eq!(r.station.queued_titles(), ["real1", "real2", "radio1", "radio2"]);
    assert_eq!(r.shared.nowplaying_json()["queue"].as_array().unwrap().len(), 4, "the overlay sees the queue too");
    let saved = r.queue_file.read();
    let items = saved["items"].as_array().unwrap();
    assert_eq!(items.iter().map(|i| i["title"].as_str().unwrap()).collect::<Vec<_>>(), ["real1", "real2"], "radio filler is never saved");
    assert_eq!(items[0]["requester_id"], 1);
}

#[tokio::test]
async fn the_saved_queue_comes_back_after_a_restart() {
    let first = rig("persist", true);
    let entry = |id: u64, title: &str, url: &str| QueueEntry { seq: 0, webpage_url: url.into(), requester_id: id, requester_name: format!("u{id}"), title: title.into(), uploader: "Up".into() };
    first.station.enqueue(entry(1, "One", A));
    first.station.enqueue(entry(2, "Two", B));
    let saved = first.queue_file.read();
    assert_eq!(saved["items"].as_array().unwrap().len(), 2);

    // A "new run": a station that finds that file.
    let second = rig("persist-restart", true);
    second.queue_file.write(saved).unwrap();
    assert_eq!(second.station.restore(), 2);
    assert_eq!(second.station.queued_titles(), ["One", "Two"]);

    // A Python-era queue file (same shape, one junk item) is understood too.
    let legacy = serde_json::json!({"items": [{"webpage_url": C, "requester_id": 5, "requester_name": "Old", "title": "Legacy", "uploader": ""}, {"junk": true}]});
    let third = rig("persist-legacy", true);
    third.queue_file.write(legacy.as_object().unwrap().clone()).unwrap();
    assert_eq!(third.station.restore(), 1);
    assert_eq!(third.station.queued_titles(), ["Legacy"]);
}

// --------------------------------------------------------------------- skip

#[tokio::test]
async fn skip_is_for_mods_and_for_the_requester_only() {
    let mut r = rig("skip", false);
    assert_eq!(r.station.skip(1, true), SkipOutcome::NothingPlaying);
    r.lookup.add("a", track("Song A", A, 100)); // long fake duration: it would play for 2 s of audio anyway
    r.lookup.add("b", track("Song B", B, 2));
    r.station.request(&who(7, "Ann"), "a", r.replier());
    r.station.request(&who(8, "Bo"), "b", r.replier());
    r.out_containing("Ann's song request is Now Playing").await;
    assert_eq!(r.station.skip(8, false), SkipOutcome::NotYours, "Bo can't skip Ann's song");
    assert_eq!(r.title().as_deref(), Some("Song A"));
    assert_eq!(r.station.skip(7, false), SkipOutcome::Skipped, "the requester can skip their own song");
    r.out_containing("Bo's song request is Now Playing").await;
    assert_eq!(r.station.skip(99, true), SkipOutcome::Skipped, "a mod can skip anything");
    r.wait_until("idle", |r| r.title().is_none() && r.station.active_requester_id().is_none()).await;
    assert_eq!(r.station.skip(99, true), SkipOutcome::NothingPlaying);
}

#[tokio::test]
async fn skipping_while_a_song_is_still_loading_prevents_it_from_ever_playing() {
    let mut r = rig("skip-loading", true);
    r.lookup.add("a", track("Song A", A, 2));
    r.station.request(&who(7, "Ann"), "a", r.replier());
    r.out().await; // Queued
    *r.lookup.delay.lock().unwrap() = Duration::from_millis(800); // the play-time lookup is slow
    let _listener = r.hub.subscribe(); // lets the player pick the song up now
    r.wait_until("the song to be picked up", |r| r.station.active_requester_id() == Some(7)).await;
    assert_eq!(r.station.skip(7, false), SkipOutcome::Skipped);
    tokio::time::sleep(Duration::from_millis(1_800)).await;
    assert_eq!(r.title(), None);
    assert_eq!(r.opener.opens.load(Ordering::SeqCst), 0, "a skipped song's stream must never be opened");
    assert_eq!(r.station.active_requester_id(), None);
}

// ------------------------------------------------------------------ playback

#[tokio::test]
async fn consecutive_songs_change_over_without_the_overlay_ever_showing_nothing() {
    let mut r = rig("gapless", false);
    r.lookup.add("a", track("Song A", A, 2));
    r.lookup.add("b", track("Song B", B, 2));
    r.station.request(&who(7, "Ann"), "a", r.replier());
    r.station.request(&who(8, "Bo"), "b", r.replier());
    let (mut seen, mut a_at, mut b_at) = (Vec::<Option<String>>::new(), None, None);
    let end = Instant::now() + Duration::from_secs(8);
    while Instant::now() < end && b_at.is_none_or(|b: Instant| b.elapsed() < Duration::from_millis(300)) {
        let t = r.title();
        if seen.last() != Some(&t) {
            if t.as_deref() == Some("Song A") {
                a_at = Some(Instant::now());
            }
            if t.as_deref() == Some("Song B") {
                b_at = Some(Instant::now());
            }
            seen.push(t);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(seen, [None, Some("Song A".into()), Some("Song B".into())], "no 'nothing playing' flicker between the songs");
    let gap = b_at.unwrap() - a_at.unwrap();
    assert!((1.9..2.6).contains(&gap.as_secs_f64()), "B should follow A's 2 s right away, got {gap:?}");
    // Both were prepared with at most one open each, and the chat saw two announcements.
    r.out_containing("Bo's song request is Now Playing").await;
    assert_eq!(r.opener.opens.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn nothing_starts_without_a_listener_when_that_is_configured_and_starts_when_one_arrives() {
    let mut r = rig("listeners", true);
    r.lookup.add("a", track("Song A", A, 2));
    r.station.request(&who(7, "Ann"), "a", r.replier());
    r.out().await;
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(r.title(), None);
    assert_eq!(r.station.queue_len(), 1);
    let _listener = r.hub.subscribe();
    r.wait_until("the song to start once someone listens", |r| r.title().as_deref() == Some("Song A")).await;
}

// -------------------------------------------------------------------- radio

#[tokio::test]
async fn radio_fills_the_gap_with_a_related_song_and_never_repeats_or_replays_the_seed() {
    let mut r = rig("radio", false);
    r.lookup.add("a", track("Song A", A, 2));
    r.lookup.add(B, track("Radio B", B, 2));
    r.lookup.add(C, track("Radio C", C, 2));
    *r.lookup.mix.lock().unwrap() = vec![
        MixEntry { id: "aaaaaaaaaaa".into(), url: A.into(), title: "Song A".into(), uploader: "x".into() }, // the seed itself
        MixEntry { id: "bbbbbbbbbbb".into(), url: B.into(), title: "Radio B".into(), uploader: "x".into() },
        MixEntry { id: "ccccccccccc".into(), url: C.into(), title: "Radio C".into(), uploader: "x".into() },
    ];
    r.station.request(&who(7, "Ann"), "a", r.replier());
    r.out_containing("Ann's song request is Now Playing").await;
    let b = r.out_containing("Now Playing: Radio B").await;
    assert_eq!(b.reply_to, None);
    assert!(!b.text.contains("song request"), "radio songs aren't attributed to a chatter: {}", b.text);
    r.out_containing("Now Playing: Radio C").await;
    assert_eq!(r.shared.nowplaying_json()["requester_name"], RADIO_NAME);
    let calls = r.lookup.mix_calls.lock().unwrap().clone();
    assert_eq!(calls[0], "aaaaaaaaaaa", "the first suggestion is seeded by the song that played");
    assert!(calls.len() >= 2, "{calls:?}");
}

#[tokio::test]
async fn radio_off_means_silence_after_the_last_request() {
    let mut r = rig("radio-off", false);
    r.toggles.write(serde_json::json!({"radio_autoplay_enabled": false}).as_object().unwrap().clone()).unwrap();
    r.lookup.add("a", track("Song A", A, 2));
    *r.lookup.mix.lock().unwrap() = vec![MixEntry { id: "bbbbbbbbbbb".into(), url: B.into(), title: "Radio B".into(), uploader: "x".into() }];
    r.station.request(&who(7, "Ann"), "a", r.replier());
    r.out_containing("Now Playing").await;
    r.wait_until("the song to end", |r| r.title().is_none()).await;
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert!(r.lookup.mix_calls.lock().unwrap().is_empty(), "radio is off: the mix must not even be asked for");
    assert_eq!(r.station.queue_len(), 0);
    assert!(r.station.radio_status() == false);
    r.station.set_radio(true);
    assert!(r.station.radio_status());
}

// ----------------------------------------------------------------- failures

#[tokio::test]
async fn a_song_that_cannot_be_loaded_is_announced_and_the_queue_moves_on() {
    let mut r = rig("fail", true); // hold the queue until the lookup is rigged
    r.lookup.add("a", track("Song A", A, 2));
    r.lookup.add("b", track("Song B", B, 2));
    r.station.request(&who(7, "Ann"), "a", r.replier());
    r.out().await; // Queued
    r.station.request(&who(8, "Bo"), "b", r.replier());
    r.out().await;
    // By the time Ann's turn comes the video has gone private.
    r.lookup.add_err(A, ResolveError::Unavailable("private".into()));
    let _listener = r.hub.subscribe();
    let msg = r.out_containing("Couldn't load").await;
    assert_eq!((msg.text.as_str(), msg.reply_to), ("Couldn't load Ann's song \u{2014} skipping it.", None));
    r.out_containing("Bo's song request is Now Playing").await;
}

#[tokio::test]
async fn a_stale_stream_url_is_retried_once_with_a_fresh_lookup() {
    let mut r = rig("stale", false);
    r.lookup.add("a", track("Song A", A, 2));
    r.opener.garbage_opens.store(1, Ordering::SeqCst); // first stream turns out to be unplayable
    r.station.request(&who(7, "Ann"), "a", r.replier());
    r.out_containing("Ann's song request is Now Playing").await;
    assert_eq!(r.opener.opens.load(Ordering::SeqCst), 2);
    assert!(r.lookup.forgotten.lock().unwrap().contains(&A.to_string()), "the cached lookup must be dropped before retrying");
    r.wait_until("it to end", |r| r.title().is_none()).await;
    assert_eq!(r.shared.counters.count_last_hour("tracks_played"), 1, "one song, announced once");
}

#[tokio::test]
async fn a_song_that_fails_twice_is_given_up_on_with_one_message() {
    let mut r = rig("fail-twice", false);
    r.lookup.add("a", track("Song A", A, 2));
    r.station.request(&who(7, "Ann"), "a", r.replier());
    r.out().await; // Queued
    r.opener.fail_opens.store(5, Ordering::SeqCst);
    let msg = r.out_containing("Couldn't load").await;
    assert!(msg.text.contains("Ann's song"));
    assert_eq!(r.title(), None);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(r.station.active_requester_id(), None);
}
