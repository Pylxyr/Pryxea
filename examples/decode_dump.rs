//! Decodes a WebM/Opus or MP4/AAC file with Pryxea's decoder and writes raw
//! 48 kHz stereo f32le PCM. Handy for comparing against other decoders:
//!   cargo run --example decode_dump -- song.webm out.raw
use std::io::Write;

use pryxea::audio::decode::TrackDecoder;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(output)) = (args.next(), args.next()) else {
        eprintln!("usage: decode_dump <input> <output.raw>");
        std::process::exit(2);
    };
    let ext = std::path::Path::new(&input).extension().and_then(|e| e.to_str()).map(str::to_string);
    let file = std::fs::File::open(&input).expect("open input");
    let mut dec = TrackDecoder::open(Box::new(file), ext.as_deref()).expect("open decoder");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&output).expect("create output"));
    let mut pcm = Vec::new();
    let mut frames = 0usize;
    while dec.decode_more(&mut pcm).expect("decode") {
        for s in &pcm {
            out.write_all(&s.to_le_bytes()).unwrap();
        }
        frames += pcm.len() / 2;
        pcm.clear();
    }
    eprintln!("{frames} frames ({:.3} s)", frames as f64 / 48_000.0);
}
