//! Plays files through the real engine and saves the Ogg/Opus stream a
//! listener would receive, so any other tool can be pointed at it:
//!   cargo run --release --example stream_dump -- out.ogg a.webm b.m4a
use std::time::Duration;

use pryxea::audio::engine::{Engine, Event};
use pryxea::hub::StreamHub;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let Some(out_path) = args.next() else {
        eprintln!("usage: stream_dump <out.ogg> <input>...");
        std::process::exit(2);
    };
    let inputs: Vec<String> = args.collect();
    let hub = StreamHub::new();
    let (engine, mut events) = Engine::spawn(hub.clone(), 160);
    let mut sub = hub.subscribe();
    tokio::time::sleep(Duration::from_millis(50)).await;
    for (i, path) in inputs.iter().enumerate() {
        let ext = std::path::Path::new(path).extension().and_then(|e| e.to_str());
        let file = std::fs::File::open(path).expect("open input");
        engine.play(i as u64, Box::new(file), ext);
    }
    let mut stream = sub.header.to_vec();
    let mut remaining = inputs.len();
    while remaining > 0 {
        tokio::select! {
            chunk = sub.rx.recv() => stream.extend_from_slice(&chunk.expect("dropped")),
            event = events.recv() => {
                eprintln!("{:?}", event.as_ref().expect("engine stopped"));
                if matches!(event, Some(Event::Ended(..))) { remaining -= 1; }
            }
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(c) = sub.rx.try_recv() {
        stream.extend_from_slice(&c);
    }
    std::fs::write(&out_path, &stream).expect("write output");
    eprintln!("wrote {} bytes to {out_path}", stream.len());
}
