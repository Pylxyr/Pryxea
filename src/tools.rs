//! Keeping yt-dlp (and a tiny JavaScript runtime for it) installed and current.
//!
//! Neither ships inside Pryxea: they are downloaded into `bin/` on first use,
//! checked against published SHA-256 sums, and yt-dlp is refreshed daily
//! because YouTube changes break old versions. A yt-dlp the user points to
//! with `YTDLP_PATH` is never touched.

use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::net::http::{Client, Request};
use crate::store::JsonStore;
use crate::ytdlp::run_process;

const MAX_BINARY_BYTES: u64 = 120 * 1024 * 1024;
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
const QJS_VERSION: &str = "v0.17.0";

#[derive(Debug)]
pub enum ToolError {
    /// No official build for this operating system / CPU.
    Unsupported(String),
    Net(String),
    /// The download didn't match its published checksum.
    Verify(String),
    Io(String),
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolError::Unsupported(m) => write!(f, "no official build for this system: {m}"),
            ToolError::Net(m) => write!(f, "download failed: {m}"),
            ToolError::Verify(m) => write!(f, "checksum mismatch: {m}"),
            ToolError::Io(m) => write!(f, "cannot install: {m}"),
        }
    }
}

impl std::error::Error for ToolError {}

/// Where yt-dlp releases are published. Tests point this at a local server.
#[derive(Debug, Clone)]
pub struct Release {
    /// `<download_base>/<asset>` and `<download_base>/SHA2-256SUMS`.
    pub download_base: String,
    /// Redirects to `.../tag/<version>`.
    pub latest_url: String,
}

impl Default for Release {
    fn default() -> Self {
        Release {
            download_base: "https://github.com/yt-dlp/yt-dlp/releases/latest/download".into(),
            latest_url: "https://github.com/yt-dlp/yt-dlp/releases/latest".into(),
        }
    }
}

// ---------------------------------------------------------------- platform

/// (yt-dlp asset name, quickjs-ng asset name and SHA-256) for this machine.
pub struct Assets {
    pub ytdlp: &'static str,
    pub quickjs: Option<(&'static str, &'static str)>,
}

pub fn assets_for(os: &str, arch: &str) -> Option<Assets> {
    let q = |name, sha| Some((name, sha));
    Some(match (os, arch) {
        ("windows", "x86_64") => Assets { ytdlp: "yt-dlp.exe", quickjs: q("qjs-windows-x86_64.exe", "2aeabf0092c3262d6b2609824418f7dd7ed1f1df939f73b2b15645230cac0d77") },
        ("windows", "aarch64") => Assets { ytdlp: "yt-dlp_arm64.exe", quickjs: None },
        ("windows", "x86") => Assets { ytdlp: "yt-dlp_x86.exe", quickjs: None },
        ("linux", "x86_64") => Assets { ytdlp: "yt-dlp_linux", quickjs: q("qjs-linux-x86_64", "0bfc02511a9f549c28b53880d988fc7cd5d361e90c5e8afdfcd7dc6774ceace5") },
        ("linux", "aarch64") => Assets { ytdlp: "yt-dlp_linux_aarch64", quickjs: q("qjs-linux-aarch64", "3372133484edf50a69f3c67903af41206d22a061e930e3cfb63269272ef56d2e") },
        ("macos", "aarch64") => Assets { ytdlp: "yt-dlp_macos", quickjs: q("qjs-darwin-arm64", "8be3ddfe3397d2e692e4e1e8972ee9d032a0a580505d2f8b4ea528cf1b651c11") },
        ("macos", "x86_64") => Assets { ytdlp: "yt-dlp_macos", quickjs: q("qjs-darwin-x86_64", "9e5e101b4fd13cda3204222ca9f8be35412c41dcdef3745829633b7a67245412") },
        _ => return None,
    })
}

fn this_platform() -> Result<Assets, ToolError> {
    assets_for(std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| ToolError::Unsupported(format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)))
}

pub fn ytdlp_path(bin_dir: &Path) -> PathBuf {
    bin_dir.join(if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" })
}

pub fn quickjs_path(bin_dir: &Path) -> PathBuf {
    bin_dir.join(if cfg!(windows) { "qjs.exe" } else { "qjs" })
}

// ----------------------------------------------------------------- download

/// `<hash>  <name>` lines, as yt-dlp publishes them.
pub fn parse_sums(text: &str, asset: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (hash, name) = line.split_once(char::is_whitespace)?;
        let name = name.trim().trim_start_matches('*');
        (name == asset && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then(|| hash.to_ascii_lowercase())
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Streams `url` to `dest` (via a temporary file), refusing it unless its SHA-256 matches.
fn download_verified(client: &Client, url: &str, expected_sha256: &str, dest: &Path) -> Result<(), ToolError> {
    let io = |e: std::io::Error| ToolError::Io(format!("{}: {e}", dest.display()));
    let dir = dest.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(io)?;
    let mut resp = client.send(&Request::get(url)).and_then(|r| r.error_for_status()).map_err(|e| ToolError::Net(format!("{url}: {e}")))?;
    let part = dir.join(format!(".{}.{}.part", dest.file_name().and_then(|n| n.to_str()).unwrap_or("tool"), std::process::id()));
    let result = (|| -> Result<(), ToolError> {
        let mut file = std::fs::File::create(&part).map_err(io)?;
        let (mut hasher, mut buf, mut total) = (Sha256::new(), vec![0u8; 64 * 1024], 0u64);
        loop {
            let n = resp.read(&mut buf).map_err(|e| ToolError::Net(format!("{url}: {e}")))?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > MAX_BINARY_BYTES {
                return Err(ToolError::Net(format!("{url}: file is unreasonably large")));
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n]).map_err(io)?;
        }
        file.sync_all().map_err(io)?;
        let actual = hex(&hasher.finalize());
        if !actual.eq_ignore_ascii_case(expected_sha256) {
            return Err(ToolError::Verify(format!("{url}: expected {expected_sha256}, got {actual}")));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755)).map_err(io)?;
        }
        std::fs::rename(&part, dest).map_err(io)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}

fn expected_ytdlp_sha(client: &Client, release: &Release, asset: &str) -> Result<String, ToolError> {
    let url = format!("{}/SHA2-256SUMS", release.download_base);
    let body = client.send(&Request::get(&url)).and_then(|r| r.error_for_status()).and_then(|r| r.bytes(256 * 1024)).map_err(|e| ToolError::Net(format!("{url}: {e}")))?;
    parse_sums(&String::from_utf8_lossy(&body), asset).ok_or_else(|| ToolError::Verify(format!("{asset} is not listed in {url}")))
}

fn install_ytdlp(client: &Client, release: &Release, bin_dir: &Path) -> Result<PathBuf, ToolError> {
    let asset = this_platform()?.ytdlp;
    let sha = expected_ytdlp_sha(client, release, asset)?;
    let dest = ytdlp_path(bin_dir);
    download_verified(client, &format!("{}/{asset}", release.download_base), &sha, &dest)?;
    Ok(dest)
}

/// Downloads yt-dlp if `bin/` has none. Returns its path either way.
pub fn ensure_ytdlp(client: &Client, release: &Release, bin_dir: &Path) -> Result<PathBuf, ToolError> {
    let path = ytdlp_path(bin_dir);
    if path.exists() { Ok(path) } else { install_ytdlp(client, release, bin_dir) }
}

/// Downloads the pinned QuickJS-ng build if missing. `Ok(None)`: none exists for this platform.
pub fn ensure_quickjs(client: &Client, bin_dir: &Path) -> Result<Option<PathBuf>, ToolError> {
    ensure_quickjs_from(client, "https://github.com/quickjs-ng/quickjs/releases/download", bin_dir, &this_platform()?)
}

/// Same as [`ensure_quickjs`] with the download location and assets given (a test hook).
pub fn ensure_quickjs_from(client: &Client, base: &str, bin_dir: &Path, assets: &Assets) -> Result<Option<PathBuf>, ToolError> {
    let Some((asset, sha)) = assets.quickjs else { return Ok(None) };
    let path = quickjs_path(bin_dir);
    if !path.exists() {
        download_verified(client, &format!("{base}/{QJS_VERSION}/{asset}"), sha, &path)?;
    }
    Ok(Some(path))
}

// ------------------------------------------------------------------ updates

/// The version a binary reports (`yt-dlp --version`), e.g. "2026.08.19".
pub fn installed_version(exe: &Path) -> Option<String> {
    let out = run_process(exe, &["--version".to_string()], Duration::from_secs(30)).ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status_ok && !text.is_empty() && text.len() < 40).then_some(text)
}

/// The newest release's version, read from where `/releases/latest` redirects to.
pub fn latest_version(client: &Client, release: &Release) -> Result<String, ToolError> {
    let resp = client.send(&Request::get(&release.latest_url)).map_err(|e| ToolError::Net(format!("{}: {e}", release.latest_url)))?;
    resp.url.rsplit('/').next().filter(|t| !t.is_empty() && resp.url.contains("/tag/")).map(str::to_string).ok_or_else(|| ToolError::Net(format!("no release tag in {}", resp.url)))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Maintained {
    /// The user manages yt-dlp themselves.
    Skipped,
    Installed(String),
    Updated { from: String, to: String },
    UpToDate(String),
    /// Checked recently; nothing done.
    NotDue,
}

/// Installs yt-dlp when missing and refreshes it at most once a day.
/// `now_secs` is Unix time; `user_managed` is true when `YTDLP_PATH` is set.
pub fn maintain_ytdlp(client: &Client, release: &Release, bin_dir: &Path, state: &JsonStore, now_secs: u64, user_managed: bool) -> Result<Maintained, ToolError> {
    if user_managed {
        return Ok(Maintained::Skipped);
    }
    let exe = ytdlp_path(bin_dir);
    let record = |version: &str| {
        let version = version.to_string();
        let _ = state.update(move |mut m| {
            m.insert("ytdlp_checked_at".into(), now_secs.into());
            m.insert("ytdlp_version".into(), version.into());
            Some(m)
        });
    };
    if !exe.exists() {
        install_ytdlp(client, release, bin_dir)?;
        let version = installed_version(&exe).unwrap_or_default();
        record(&version);
        return Ok(Maintained::Installed(version));
    }
    let last = state.read().get("ytdlp_checked_at").and_then(|v| v.as_u64()).unwrap_or(0);
    if now_secs.saturating_sub(last) < CHECK_INTERVAL_SECS {
        return Ok(Maintained::NotDue);
    }
    let latest = latest_version(client, release)?;
    let current = installed_version(&exe);
    if current.as_deref() == Some(latest.as_str()) {
        record(&latest);
        return Ok(Maintained::UpToDate(latest));
    }
    install_ytdlp(client, release, bin_dir)?;
    let now_has = installed_version(&exe).unwrap_or_else(|| latest.clone());
    record(&now_has);
    Ok(Maintained::Updated { from: current.unwrap_or_default(), to: now_has })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_lines_are_matched_by_exact_asset_name() {
        let sums = format!("{}  yt-dlp\n{}  yt-dlp.exe\n{} *yt-dlp_linux\nnot a line\n", "a".repeat(64), "B".repeat(64), "c".repeat(64));
        assert_eq!(parse_sums(&sums, "yt-dlp.exe"), Some("b".repeat(64)));
        assert_eq!(parse_sums(&sums, "yt-dlp"), Some("a".repeat(64)));
        assert_eq!(parse_sums(&sums, "yt-dlp_linux"), Some("c".repeat(64)));
        assert_eq!(parse_sums(&sums, "yt-dlp_macos"), None);
        assert_eq!(parse_sums("deadbeef  yt-dlp", "yt-dlp"), None, "a short hash is not a checksum");
    }

    #[test]
    fn every_supported_platform_has_assets_and_unknown_ones_do_not() {
        for (os, arch) in [("windows", "x86_64"), ("linux", "x86_64"), ("linux", "aarch64"), ("macos", "aarch64"), ("macos", "x86_64")] {
            let a = assets_for(os, arch).unwrap_or_else(|| panic!("{os} {arch}"));
            let (name, sha) = a.quickjs.expect("quickjs build");
            assert!(name.starts_with("qjs-") && sha.len() == 64, "{name}");
        }
        assert!(assets_for("windows", "aarch64").unwrap().quickjs.is_none());
        assert!(assets_for("freebsd", "x86_64").is_none());
    }

    #[test]
    fn paths_follow_the_platform_convention() {
        let p = ytdlp_path(Path::new("/h/bin"));
        assert_eq!(p.file_name().unwrap(), if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" });
    }
}
