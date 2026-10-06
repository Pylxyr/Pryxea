//! Real-world check of the whole lookup path, meant to be run on your own machine:
//! installs yt-dlp if needed, resolves a song, streams the first seconds through
//! the HTTP source and decoder, and asks for the radio mix.
//!
//!   cargo run --release --example lookup -- "never gonna give you up"
//!   cargo run --release --example lookup -- https://www.youtube.com/watch?v=dQw4w9WgXcQ
use std::sync::Arc;

use pryxea::audio::decode::TrackDecoder;
use pryxea::config;
use pryxea::net::http::Client;
use pryxea::net::source::HttpSource;
use pryxea::paths::Dirs;
use pryxea::state::Shared;
use pryxea::tools::{self, Release};
use pryxea::ytdlp::Resolver;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let query = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    if query.trim().is_empty() {
        eprintln!("usage: lookup <song name or YouTube URL>");
        std::process::exit(2);
    }
    let dirs = Dirs::resolve();
    dirs.create().expect("create home folder");
    let bin = dirs.home.join("bin");
    let loaded = config::load(&|k| std::env::var(k).ok(), &dirs.home, &dirs.data, &dirs.logs);
    let mut cfg = loaded.settings.ytdlp.clone();

    let client = Arc::new(Client::new());
    let exe = match cfg.path.clone() {
        Some(p) => p,
        None => {
            eprintln!("checking yt-dlp in {} ...", bin.display());
            tools::ensure_ytdlp(&client, &Release::default(), &bin).expect("install yt-dlp")
        }
    };
    if cfg.js_runtime_path.is_none() {
        if let Ok(Some(qjs)) = tools::ensure_quickjs(&client, &bin) {
            cfg.js_runtime_path = Some(qjs);
            cfg.js_runtime_name = "quickjs".into();
        }
    }
    eprintln!("yt-dlp {}", tools::installed_version(&exe).unwrap_or_else(|| "?".into()));

    let resolver = Resolver::new(cfg, exe, dirs.data.join("ytdlp-cache"), Arc::new(Shared::new()));
    let started = std::time::Instant::now();
    let track = match resolver.resolve(&query).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("lookup failed after {:.1}s: {e}", started.elapsed().as_secs_f32());
            std::process::exit(1);
        }
    };
    println!("resolved in {:.1}s: {:?} by {:?} ({} s, .{})", started.elapsed().as_secs_f32(), track.title, track.uploader, track.duration_secs, track.extension);
    println!("  page: {}\n  video id: {:?}\n  stream host: {}", track.webpage_url, track.video_id, track.stream_url.split('/').nth(2).unwrap_or("?"));

    let (url, headers, ext) = (track.stream_url.clone(), track.headers.clone(), track.extension);
    let outcome = tokio::task::spawn_blocking(move || {
        let t = std::time::Instant::now();
        let source = HttpSource::open(client, &url, &headers).map_err(|e| format!("open: {e}"))?;
        let (len, seekable) = (source.len(), symphonia::core::io::MediaSource::is_seekable(&source));
        let mut dec = TrackDecoder::open(Box::new(source), Some(ext)).map_err(|e| format!("probe: {e}"))?;
        let (mut pcm, mut frames) = (Vec::new(), 0usize);
        while frames < 48_000 * 10 && dec.decode_more(&mut pcm).map_err(|e| format!("decode: {e}"))? {
            frames += pcm.len() / 2;
            pcm.clear();
        }
        Ok::<_, String>((len, seekable, frames, t.elapsed()))
    })
    .await
    .unwrap();
    match outcome {
        Ok((len, seekable, frames, took)) => println!("  stream: {len:?} bytes, seekable: {seekable}; decoded {:.1} s of audio in {:.1}s", frames as f64 / 48_000.0, took.as_secs_f32()),
        Err(e) => {
            eprintln!("playing the stream failed: {e}");
            std::process::exit(1);
        }
    }

    if let Some(id) = &track.video_id {
        let mix = resolver.radio_mix(id).await;
        println!("radio mix: {} suggestions{}", mix.len(), mix.iter().take(3).map(|m| format!("\n  - {} ({})", m.title, m.id)).collect::<String>());
    }
}
