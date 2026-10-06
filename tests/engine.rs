//! End to end: fixture file -> engine -> Opus/Ogg -> hub -> decode again.
//! These run in real time (the engine paces itself at 1x), so they take a few seconds.

use std::f64::consts::PI;
use std::time::{Duration, Instant};

use pryxea::audio::engine::{Engine, Event, Outcome, memory_source};
use pryxea::audio::ogg;
use pryxea::audio::opus::Decoder;
use pryxea::hub::{StreamHub, Subscription};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

async fn next_event(rx: &mut UnboundedReceiver<Event>, within: Duration) -> Event {
    timeout(within, rx.recv()).await.expect("timed out waiting for an engine event").expect("engine stopped")
}

/// Collects everything a listener receives until `until` returns true for an event.
async fn collect(sub: &mut Subscription, events: &mut UnboundedReceiver<Event>, mut until: impl FnMut(&Event) -> bool) -> (Vec<u8>, Vec<(Event, Instant)>) {
    let mut stream = sub.header.to_vec();
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < deadline, "collect timed out; events so far: {seen:?}");
        tokio::select! {
            chunk = sub.rx.recv() => match chunk {
                Some(c) => stream.extend_from_slice(&c),
                None => panic!("listener was dropped by the hub"),
            },
            event = events.recv() => {
                let event = event.expect("engine stopped");
                let done = until(&event);
                seen.push((event, Instant::now()));
                if done {
                    // Let the page containing the end of the track arrive.
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    while let Ok(c) = sub.rx.try_recv() { stream.extend_from_slice(&c); }
                    return (stream, seen);
                }
            }
        }
    }
}

/// Decodes a collected Ogg/Opus stream back to interleaved stereo f32.
fn decode_stream(stream: &[u8]) -> Vec<f32> {
    let packets = ogg::packets(stream);
    assert_eq!(&packets[0].1[..8], b"OpusHead", "stream must start with the Opus header");
    assert_eq!(&packets[1].1[..8], b"OpusTags");
    let pre_skip = usize::from(u16::from_le_bytes([packets[0].1[10], packets[0].1[11]]));
    let mut dec = Decoder::new(0).unwrap();
    let mut pcm = Vec::new();
    for (_, p) in &packets[2..] {
        dec.decode(p, &mut pcm).unwrap();
    }
    pcm.drain(..(pre_skip * 2).min(pcm.len()));
    pcm
}

fn amplitude(pcm: &[f32], channel: usize, freq: f64, from_frame: usize, frames: usize) -> f64 {
    let (mut re, mut im) = (0.0, 0.0);
    for (i, f) in pcm.chunks_exact(2).enumerate().skip(from_frame).take(frames) {
        let a = 2.0 * PI * freq * i as f64 / 48_000.0;
        re += f64::from(f[channel]) * a.cos();
        im += f64::from(f[channel]) * a.sin();
    }
    2.0 * (re * re + im * im).sqrt() / frames as f64
}

#[tokio::test]
async fn a_track_comes_out_as_a_valid_opus_stream_in_real_time() {
    let hub = StreamHub::new();
    let (engine, mut events) = Engine::spawn(hub.clone(), 160);
    let mut sub = hub.subscribe();
    tokio::time::sleep(Duration::from_millis(50)).await; // session start

    let t0 = Instant::now();
    engine.play(1, memory_source(fixture("tone.webm")), Some("webm"));
    let (stream, seen) = collect(&mut sub, &mut events, |e| matches!(e, Event::Ended(1, _))).await;

    assert_eq!(seen[0].0, Event::Started(1));
    assert_eq!(seen[1].0, Event::Ended(1, Outcome::Finished));
    let played = seen[1].1 - t0;
    assert!((1.8..2.8).contains(&played.as_secs_f64()), "track took {played:?}, expected about 2 s of real time");

    let pcm = decode_stream(&stream);
    // Skip the first and last 100 ms: Opus start-up and the track edges.
    assert!((amplitude(&pcm, 0, 440.0, 4_800, 80_000) - 0.5).abs() < 0.03, "left 440 Hz");
    assert!((amplitude(&pcm, 1, 660.0, 4_800, 80_000) - 0.5).abs() < 0.03, "right 660 Hz");
    assert!(amplitude(&pcm, 0, 660.0, 4_800, 80_000) < 0.02, "channels must not be swapped or mixed");
}

#[tokio::test]
async fn consecutive_tracks_join_without_a_gap() {
    let hub = StreamHub::new();
    let (engine, mut events) = Engine::spawn(hub.clone(), 160);
    let mut sub = hub.subscribe();
    tokio::time::sleep(Duration::from_millis(50)).await;

    engine.play(1, memory_source(fixture("tone.webm")), Some("webm"));
    engine.play(2, memory_source(fixture("tone.webm")), Some("webm"));
    let (stream, seen) = collect(&mut sub, &mut events, |e| matches!(e, Event::Ended(2, _))).await;
    let kinds: Vec<_> = seen.iter().map(|(e, _)| e.clone()).collect();
    assert_eq!(kinds, [Event::Started(1), Event::Ended(1, Outcome::Finished), Event::Started(2), Event::Ended(2, Outcome::Finished)]);

    // Silence inside the stream = both channels near zero for many frames in a row. The window
    // stops 0.5 s before the end so the quiet tail after the last track is not counted.
    let pcm = decode_stream(&stream);
    let (mut run, mut longest, mut longest_end) = (0usize, 0usize, 0usize);
    for (i, f) in pcm.chunks_exact(2).enumerate().skip(4_800).take(pcm.len() / 2 - 24_000) {
        if f[0].abs() < 0.01 && f[1].abs() < 0.01 {
            run += 1;
            if run > longest {
                longest = run;
                longest_end = i;
            }
        } else {
            run = 0;
        }
    }
    assert!(
        longest < 1_500,
        "found a {longest}-frame silent gap ({:.0} ms) ending at {:.3} s of a {:.3} s stream",
        longest as f64 / 48.0,
        longest_end as f64 / 48_000.0,
        pcm.len() as f64 / 96_000.0
    );
    let total = pcm.len() as f64 / 2.0 / 48_000.0;
    assert!((3.8..4.6).contains(&total), "two 2 s tracks should give about 4 s, got {total:.2}");
}

#[tokio::test]
async fn a_listener_who_joins_late_gets_the_header_and_a_decodable_stream() {
    let hub = StreamHub::new();
    let (engine, mut events) = Engine::spawn(hub.clone(), 160);
    let _first = hub.subscribe();
    tokio::time::sleep(Duration::from_millis(50)).await;
    engine.play(1, memory_source(fixture("tone.webm")), Some("webm"));
    tokio::time::sleep(Duration::from_millis(700)).await;

    let mut late = hub.subscribe();
    assert!(!late.header.is_empty(), "a late listener must be handed the Ogg header pages");
    let (stream, _) = collect(&mut late, &mut events, |e| matches!(e, Event::Ended(1, _))).await;
    let pcm = decode_stream(&stream);
    let frames = pcm.len() / 2;
    assert!((55_000..90_000).contains(&frames), "{frames} frames after joining ~0.7 s in");
    // Mid-stream start: skip Opus's decoder warm-up, then the tone must be there.
    assert!((amplitude(&pcm, 0, 440.0, 4_800, 30_000) - 0.5).abs() < 0.04);
}

#[tokio::test]
async fn with_nobody_listening_nothing_is_encoded_but_tracks_still_advance_in_real_time() {
    let hub = StreamHub::new();
    let (engine, mut events) = Engine::spawn(hub.clone(), 160);
    let t0 = Instant::now();
    engine.play(1, memory_source(fixture("tone.webm")), Some("webm"));
    assert_eq!(next_event(&mut events, Duration::from_secs(3)).await, Event::Started(1));
    assert_eq!(next_event(&mut events, Duration::from_secs(4)).await, Event::Ended(1, Outcome::Finished));
    assert!((1.8..2.8).contains(&t0.elapsed().as_secs_f64()), "{:?}", t0.elapsed());
    assert!(hub.header_snapshot().is_none(), "no session may be started without a listener");
}

#[tokio::test]
async fn skip_ends_the_track_at_once_and_the_next_one_starts() {
    let hub = StreamHub::new();
    let (engine, mut events) = Engine::spawn(hub.clone(), 160);
    engine.play(1, memory_source(fixture("tone.webm")), Some("webm"));
    engine.play(2, memory_source(fixture("tone.webm")), Some("webm"));
    assert_eq!(next_event(&mut events, Duration::from_secs(3)).await, Event::Started(1));
    tokio::time::sleep(Duration::from_millis(400)).await;
    let t = Instant::now();
    engine.skip();
    assert_eq!(next_event(&mut events, Duration::from_secs(1)).await, Event::Ended(1, Outcome::Skipped));
    assert_eq!(next_event(&mut events, Duration::from_secs(1)).await, Event::Started(2));
    assert!(t.elapsed() < Duration::from_millis(400), "{:?}", t.elapsed());
    engine.cancel(2);
    assert_eq!(next_event(&mut events, Duration::from_secs(1)).await, Event::Ended(2, Outcome::Skipped));
}

#[tokio::test]
async fn an_unplayable_track_is_reported_and_does_not_block_the_queue() {
    let hub = StreamHub::new();
    let (engine, mut events) = Engine::spawn(hub.clone(), 160);
    engine.play(1, memory_source(vec![0x42; 4_000]), Some("webm"));
    engine.play(2, memory_source(fixture("tone44_frag.m4a")), Some("m4a"));
    match next_event(&mut events, Duration::from_secs(3)).await {
        Event::Ended(1, Outcome::Failed(why)) => assert!(!why.is_empty()),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(next_event(&mut events, Duration::from_secs(3)).await, Event::Started(2));
    assert_eq!(next_event(&mut events, Duration::from_secs(4)).await, Event::Ended(2, Outcome::Finished));
}
