mod common;

use std::path::PathBuf;
use std::sync::Arc;

use common::{TOOL_PAYLOAD, spawn, tool_sha};
use pryxea::net::http::Client;
use pryxea::store::JsonStore;
use pryxea::tools::{self, Assets, Maintained, Release, ToolError};

fn temp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pryxea-tools-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn release(server: &common::Server, base: &str) -> Release {
    Release { download_base: server.url(&format!("/tools{base}")), latest_url: server.url("/tools/latest") }
}

fn client(server: &common::Server) -> Arc<Client> {
    Arc::new(Client::with_tls(server.client_tls.clone()))
}

fn leftovers(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect()
}

#[test]
fn yt_dlp_is_downloaded_verified_and_made_executable() {
    let s = spawn(true);
    let bin = temp("install").join("bin");
    let path = tools::ensure_ytdlp(&client(&s), &release(&s, ""), &bin).unwrap();
    assert_eq!(path, tools::ytdlp_path(&bin));
    assert_eq!(std::fs::read(&path).unwrap(), TOOL_PAYLOAD);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o111, 0o111, "must be executable");
    }
    assert_eq!(leftovers(&bin).len(), 1, "no temporary files may remain: {:?}", leftovers(&bin));
    // Already present: nothing is fetched again.
    let before = s.requests().len();
    tools::ensure_ytdlp(&client(&s), &release(&s, ""), &bin).unwrap();
    assert_eq!(s.requests().len(), before);
}

#[test]
fn a_download_that_fails_its_checksum_is_refused_and_leaves_nothing_behind() {
    let s = spawn(false);
    let bin = temp("tamper").join("bin");
    match tools::ensure_ytdlp(&client(&s), &release(&s, "/bad"), &bin) {
        Err(ToolError::Verify(m)) => assert!(m.contains("expected"), "{m}"),
        other => panic!("expected a checksum failure, got {other:?}"),
    }
    assert!(!tools::ytdlp_path(&bin).exists());
    assert!(leftovers(&bin).is_empty(), "{:?}", leftovers(&bin));
}

#[test]
fn a_missing_or_unreachable_release_is_a_clear_network_error() {
    let s = spawn(false);
    let bin = temp("net").join("bin");
    let nowhere = Release { download_base: s.url("/nothing-here"), latest_url: s.url("/nothing-here/latest") };
    assert!(matches!(tools::ensure_ytdlp(&client(&s), &nowhere, &bin), Err(ToolError::Net(_))));
    assert!(!tools::ytdlp_path(&bin).exists());
}

#[test]
fn quickjs_is_pinned_by_hash_and_skipped_where_none_exists() {
    let s = spawn(true);
    let bin = temp("qjs").join("bin");
    let base = s.url("/qjs");
    let good = Assets { ytdlp: "yt-dlp", quickjs: Some(("qjs-test", Box::leak(tool_sha().into_boxed_str()))) };
    let path = tools::ensure_quickjs_from(&client(&s), &base, &bin, &good).unwrap().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), TOOL_PAYLOAD);

    let wrong = Assets { ytdlp: "yt-dlp", quickjs: Some(("qjs-test", "0000000000000000000000000000000000000000000000000000000000000000")) };
    let bin2 = temp("qjs-bad").join("bin");
    assert!(matches!(tools::ensure_quickjs_from(&client(&s), &base, &bin2, &wrong), Err(ToolError::Verify(_))));
    let none = Assets { ytdlp: "yt-dlp", quickjs: None };
    assert!(tools::ensure_quickjs_from(&client(&s), &base, &bin2, &none).unwrap().is_none());
}

#[test]
fn the_latest_version_is_read_from_the_release_redirect() {
    let s = spawn(true);
    assert_eq!(tools::latest_version(&client(&s), &release(&s, "")).unwrap(), "2026.09.01");
    let bad = Release { download_base: String::new(), latest_url: s.url("/small") };
    assert!(tools::latest_version(&client(&s), &bad).is_err(), "a URL that doesn't redirect to a tag has no version");
}

#[cfg(unix)]
mod lifecycle {
    use super::*;

    fn store(dir: &std::path::Path) -> JsonStore {
        JsonStore::new(dir.join("tools.json"))
    }

    fn write_old_ytdlp(bin: &std::path::Path, version: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(bin).unwrap();
        let path = tools::ytdlp_path(bin);
        std::fs::write(&path, format!("#!/bin/sh\necho {version}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    const DAY: u64 = 24 * 60 * 60;

    #[test]
    fn first_run_installs_then_stays_quiet_for_a_day_then_checks_again() {
        let s = spawn(true);
        let dir = temp("life");
        let (bin, state, rel, c) = (dir.join("bin"), store(&dir), release(&s, ""), client(&s));
        let t0 = 1_800_000_000;
        assert_eq!(tools::maintain_ytdlp(&c, &rel, &bin, &state, t0, false).unwrap(), Maintained::Installed("2026.09.01".into()));
        assert_eq!(tools::maintain_ytdlp(&c, &rel, &bin, &state, t0 + 3_600, false).unwrap(), Maintained::NotDue);
        assert_eq!(tools::maintain_ytdlp(&c, &rel, &bin, &state, t0 + DAY + 1, false).unwrap(), Maintained::UpToDate("2026.09.01".into()));
        // The check time was refreshed, so an hour later nothing happens again.
        assert_eq!(tools::maintain_ytdlp(&c, &rel, &bin, &state, t0 + DAY + 3_600, false).unwrap(), Maintained::NotDue);
    }

    #[test]
    fn an_old_version_is_replaced_by_the_latest() {
        let s = spawn(false);
        let dir = temp("old");
        let bin = dir.join("bin");
        write_old_ytdlp(&bin, "2025.01.01");
        let outcome = tools::maintain_ytdlp(&client(&s), &release(&s, ""), &bin, &store(&dir), 1_800_000_000, false).unwrap();
        assert_eq!(outcome, Maintained::Updated { from: "2025.01.01".into(), to: "2026.09.01".into() });
        assert_eq!(tools::installed_version(&tools::ytdlp_path(&bin)).as_deref(), Some("2026.09.01"));
    }

    #[test]
    fn a_failed_update_keeps_the_working_copy() {
        let s = spawn(false);
        let dir = temp("keep");
        let bin = dir.join("bin");
        write_old_ytdlp(&bin, "2025.01.01");
        let result = tools::maintain_ytdlp(&client(&s), &release(&s, "/bad"), &bin, &store(&dir), 1_800_000_000, false);
        assert!(matches!(result, Err(ToolError::Verify(_))));
        assert_eq!(tools::installed_version(&tools::ytdlp_path(&bin)).as_deref(), Some("2025.01.01"), "the old, working yt-dlp must survive");
    }

    #[test]
    fn a_user_supplied_yt_dlp_is_never_touched() {
        let s = spawn(false);
        let dir = temp("user");
        let before = s.requests().len();
        let out = tools::maintain_ytdlp(&client(&s), &release(&s, ""), &dir.join("bin"), &store(&dir), 1_800_000_000, true).unwrap();
        assert_eq!(out, Maintained::Skipped);
        assert_eq!(s.requests().len(), before);
        assert!(!dir.join("bin").exists());
    }
}
