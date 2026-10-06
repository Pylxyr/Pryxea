//! Checks against the real internet (real certificates, real CDN redirects).
//! Ignored by default; run with: cargo test --test internet -- --ignored

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use pryxea::net::http::{Client, Request};
use pryxea::net::source::HttpSource;
use symphonia::core::io::MediaSource;

const SUMS: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/SHA2-256SUMS";
const EXE: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";

#[test]
#[ignore = "needs internet access"]
fn real_https_with_the_os_trust_store_and_cdn_redirects() {
    let client = Arc::new(Client::new());
    let resp = client.send(&Request::get(SUMS)).unwrap().error_for_status().unwrap();
    assert!(resp.url.contains("githubusercontent.com") || resp.url.contains("github.com"), "{}", resp.url);
    let text = String::from_utf8(resp.bytes(100_000).unwrap()).unwrap();
    assert!(text.lines().any(|l| l.ends_with("yt-dlp.exe")), "{text}");
}

#[test]
#[ignore = "needs internet access"]
fn range_reads_and_seeks_work_through_a_real_cdn() {
    let mut src = HttpSource::open(Arc::new(Client::new()), EXE, &[]).unwrap();
    let len = src.byte_len().expect("length");
    assert!((10_000_000..100_000_000).contains(&len), "{len}");
    assert!(src.is_seekable());
    let mut head = [0u8; 2];
    src.read_exact(&mut head).unwrap();
    assert_eq!(&head, b"MZ", "a Windows executable starts with MZ");
    src.seek(SeekFrom::End(-4)).unwrap();
    let mut tail = Vec::new();
    src.read_to_end(&mut tail).unwrap();
    assert_eq!(tail.len(), 4);
}

#[test]
#[ignore = "needs internet access; downloads ~40 MB"]
fn the_real_yt_dlp_and_quickjs_download_verify_and_run() {
    use pryxea::tools::{self, Release};
    let dir = std::env::temp_dir().join(format!("pryxea-real-tools-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let client = Client::new();

    let ytdlp = tools::ensure_ytdlp(&client, &Release::default(), &dir).expect("yt-dlp must download and match its published SHA-256");
    let version = tools::installed_version(&ytdlp).expect("the downloaded yt-dlp must run");
    let latest = tools::latest_version(&client, &Release::default()).unwrap();
    assert_eq!(version, latest, "a fresh download is the latest release");
    println!("yt-dlp {version} installed at {}", ytdlp.display());

    if let Some(qjs) = tools::ensure_quickjs(&client, &dir).expect("QuickJS must match its pinned SHA-256") {
        let out = std::process::Command::new(&qjs).args(["-e", "console.log(6*7)"]).output().expect("run qjs");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "42");
    }
}
