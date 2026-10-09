//! Pryxea's own updates. Once a day it asks GitHub whether a newer release
//! exists; if so the settings page offers it, and only clicking "Update now"
//! installs it (a download checked against the release's `SHA256SUMS`, swapped
//! in for the program file). Nothing restarts by itself: a stream in progress
//! is never cut off by an update.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::net::http::Client;
use crate::setup::esc;
use crate::tools::{self, ToolError};

pub const DEFAULT_REPO: &str = "Pylxyr/Pryxea";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where releases are published. Tests point this at a local server.
#[derive(Debug, Clone)]
pub struct Repo {
    /// `<download_base>/<asset>` and `<download_base>/SHA256SUMS`.
    pub download_base: String,
    /// Redirects to `.../tag/<version>`.
    pub latest_url: String,
}

impl Repo {
    pub fn github(owner_and_name: &str) -> Repo {
        Repo { download_base: format!("https://github.com/{owner_and_name}/releases/latest/download"), latest_url: format!("https://github.com/{owner_and_name}/releases/latest") }
    }
}

/// `v1.2.3` / `1.2.3` -> (1, 2, 3). Pre-releases (`1.2.3-beta`) and anything odd give `None`:
/// they are never offered.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let mut parts = s.split('.');
    let mut next = || parts.next()?.parse::<u64>().ok();
    let v = (next()?, next()?, next()?);
    parts.next().is_none().then_some(v)
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse_version(candidate), parse_version(current)), (Some(a), Some(b)) if a > b)
}

/// Name of the release file for a platform, e.g. `pryxea-linux-x86_64`.
pub fn asset_name(os: &str, arch: &str) -> Option<String> {
    let supported = matches!((os, arch), ("windows", "x86_64") | ("linux", "x86_64") | ("linux", "aarch64") | ("macos", "aarch64") | ("macos", "x86_64"));
    supported.then(|| format!("pryxea-{os}-{arch}{}", if os == "windows" { ".exe" } else { "" }))
}

fn this_asset() -> Result<String, ToolError> {
    asset_name(std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| ToolError::Unsupported(format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)))
}

/// The newest published version, if it is newer than `current`.
pub fn check(client: &Client, repo: &Repo, current: &str) -> Result<Option<String>, ToolError> {
    let release = tools::Release { download_base: repo.download_base.clone(), latest_url: repo.latest_url.clone() };
    let latest = tools::latest_version(client, &release)?;
    Ok(is_newer(&latest, current).then(|| latest.trim_start_matches(['v', 'V']).to_string()))
}

/// Puts `new_file` where `exe` is. The running program keeps working: on Unix the old file is
/// replaced by name; on Windows (which refuses to overwrite a running program but allows
/// renaming it) the old one is moved aside to `.old` and removed at the next start.
pub fn replace_exe(exe: &Path, new_file: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::rename(new_file, exe)
    }
    #[cfg(not(unix))]
    {
        let aside = old_path(exe);
        let _ = std::fs::remove_file(&aside);
        std::fs::rename(exe, &aside)?;
        if let Err(e) = std::fs::rename(new_file, exe) {
            let _ = std::fs::rename(&aside, exe); // put the working copy back
            return Err(e);
        }
        Ok(())
    }
}

fn old_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".old");
    exe.with_file_name(name)
}

/// Removes the leftover from a previous update. Call once at start-up.
pub fn clean_up_old(exe: &Path) {
    let _ = std::fs::remove_file(old_path(exe));
}

/// Downloads, verifies and installs `asset` over `exe`.
pub fn install(client: &Client, repo: &Repo, asset: &str, exe: &Path) -> Result<(), ToolError> {
    let sha = tools::expected_sha(client, &repo.download_base, "SHA256SUMS", asset)?;
    let staged = exe.with_file_name(format!(".{}.new", exe.file_name().and_then(|n| n.to_str()).unwrap_or("pryxea")));
    tools::download_verified(client, &format!("{}/{asset}", repo.download_base), &sha, &staged)?;
    replace_exe(exe, &staged).map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        ToolError::Io(format!("{}: {e}", exe.display()))
    })
}

// ------------------------------------------------------------------ the app side

/// Remembers what the daily check found and what an install did, for the pages to show.
pub struct Updater {
    client: std::sync::Arc<Client>,
    repo: Repo,
    exe: PathBuf,
    current: String,
    available: Mutex<Option<String>>,
    installed: Mutex<Option<String>>,
    busy: AtomicBool,
}

impl Updater {
    pub fn new(client: std::sync::Arc<Client>, repo: Repo, exe: PathBuf, current: &str) -> Updater {
        Updater { client, repo, exe, current: current.to_string(), available: Mutex::default(), installed: Mutex::default(), busy: AtomicBool::new(false) }
    }

    /// Asks GitHub. Blocking.
    pub fn check_now(&self) -> Result<Option<String>, ToolError> {
        let found = check(&self.client, &self.repo, &self.current)?;
        *self.available.lock().unwrap_or_else(|e| e.into_inner()) = found.clone();
        Ok(found)
    }

    pub fn available(&self) -> Option<String> {
        self.available.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn installed(&self) -> Option<String> {
        self.installed.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Installs the version the last check found. Blocking. Returns the installed version.
    pub fn install_now(&self) -> Result<String, ToolError> {
        let version = self.available().ok_or_else(|| ToolError::Net("no newer version is known; check again first".into()))?;
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err(ToolError::Io("an update is already being installed".into()));
        }
        let result = this_asset().and_then(|asset| install(&self.client, &self.repo, &asset, &self.exe));
        self.busy.store(false, Ordering::SeqCst);
        result?;
        *self.installed.lock().unwrap_or_else(|e| e.into_inner()) = Some(version.clone());
        *self.available.lock().unwrap_or_else(|e| e.into_inner()) = None;
        crate::info!("Updated Pryxea {} -> {version}; the new version runs after a restart.", self.current);
        Ok(version)
    }

    /// The banner shown at the top of the pages (empty when there is nothing to say).
    pub fn banner_html(&self) -> String {
        let mut html = String::new();
        if let Some(v) = self.installed() {
            let _ = write!(html, "<div class=\"banner banner-ok\" role=\"status\">Updated to Pryxea {}. Close and reopen Pryxea to start using it.</div>", esc(&v));
        } else if let Some(v) = self.available() {
            let _ = write!(
                html,
                "<div class=\"banner banner-ok\" role=\"status\">Pryxea {} is available (you have {}). <form method=\"post\" action=\"/update\" style=\"display:inline\"><button type=\"submit\">Update now</button></form> Nothing restarts by itself; the new version runs the next time you start Pryxea.</div>",
                esc(&v),
                esc(&self.current)
            );
        }
        html
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically_and_prereleases_are_never_offered() {
        assert!(is_newer("v0.2.0", "0.1.0") && is_newer("1.0.0", "0.9.9") && is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0") && !is_newer("0.1.0", "0.2.0") && !is_newer("0.9.0", "0.10.0"));
        for odd in ["1.2.3-beta", "1.2", "1.2.3.4", "latest", "", "v", "1.x.3"] {
            assert_eq!(parse_version(odd), None, "{odd:?}");
            assert!(!is_newer(odd, "0.0.1"), "{odd:?}");
        }
        assert_eq!(parse_version(" V1.20.300 "), Some((1, 20, 300)));
    }

    #[test]
    fn release_files_are_named_per_platform() {
        assert_eq!(asset_name("windows", "x86_64").as_deref(), Some("pryxea-windows-x86_64.exe"));
        assert_eq!(asset_name("linux", "aarch64").as_deref(), Some("pryxea-linux-aarch64"));
        assert_eq!(asset_name("macos", "aarch64").as_deref(), Some("pryxea-macos-aarch64"));
        assert_eq!(asset_name("freebsd", "x86_64"), None);
        assert_eq!(asset_name("windows", "aarch64"), None);
    }

    #[test]
    fn the_old_file_name_keeps_the_original_name_and_the_repo_urls_are_github_releases() {
        assert_eq!(old_path(Path::new("/a/pryxea.exe")), PathBuf::from("/a/pryxea.exe.old"));
        let r = Repo::github("owner/name");
        assert_eq!(r.download_base, "https://github.com/owner/name/releases/latest/download");
        assert_eq!(r.latest_url, "https://github.com/owner/name/releases/latest");
    }

    #[test]
    fn banners_are_empty_when_idle_and_escaped_when_not() {
        let u = Updater::new(std::sync::Arc::new(Client::new()), Repo::github("o/n"), PathBuf::from("/x"), "0.1.0");
        assert_eq!(u.banner_html(), "");
        *u.available.lock().unwrap() = Some("9.9.9<script>".into());
        let html = u.banner_html();
        assert!(html.contains("Update now") && html.contains("action=\"/update\"") && html.contains("9.9.9&lt;script&gt;") && !html.contains("<script>"));
        *u.installed.lock().unwrap() = Some("1.0.0".into());
        assert!(u.banner_html().contains("Close and reopen Pryxea") && !u.banner_html().contains("Update now"));
    }
}
