mod common;

use std::path::PathBuf;
use std::sync::Arc;

use common::{TOOL_PAYLOAD, spawn};
use pryxea::net::http::Client;
use pryxea::selfupdate::{self, Repo, Updater};
use pryxea::tools::ToolError;

fn repo(server: &common::Server, dir: &str) -> Repo {
    Repo { download_base: server.url(&format!("/{dir}")), latest_url: server.url(&format!("/{dir}/latest")) }
}

fn fake_exe(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pryxea-selfupdate-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join(if cfg!(windows) { "pryxea.exe" } else { "pryxea" });
    std::fs::write(&exe, b"OLD PROGRAM").unwrap();
    exe
}

fn client(s: &common::Server) -> Arc<Client> {
    Arc::new(Client::with_tls(s.client_tls.clone()))
}

#[test]
fn a_newer_release_is_noticed_and_an_equal_or_older_one_is_not() {
    let s = spawn(true);
    let c = client(&s);
    assert_eq!(selfupdate::check(&c, &repo(&s, "rel"), "0.1.0").unwrap().as_deref(), Some("9.9.9"));
    assert_eq!(selfupdate::check(&c, &repo(&s, "rel"), "9.9.9").unwrap(), None);
    assert_eq!(selfupdate::check(&c, &repo(&s, "rel"), "10.0.0").unwrap(), None);
    // A repository that doesn't exist (404 page, no redirect to a tag) is an error, not a crash.
    assert!(selfupdate::check(&c, &Repo { download_base: s.url("/nope"), latest_url: s.url("/nope/latest") }, "0.1.0").is_err());
}

#[test]
fn an_update_is_verified_then_swapped_in_without_leaving_files_behind() {
    let s = spawn(true);
    let exe = fake_exe("install");
    let asset = selfupdate::asset_name(std::env::consts::OS, std::env::consts::ARCH).expect("a supported test platform");
    selfupdate::install(&client(&s), &repo(&s, "rel"), &asset, &exe).unwrap();
    assert_eq!(std::fs::read(&exe).unwrap(), TOOL_PAYLOAD);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o111, 0o111, "the new program must be runnable");
    }
    let leftovers: Vec<String> = std::fs::read_dir(exe.parent().unwrap()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n != exe.file_name().unwrap().to_str().unwrap() && !n.ends_with(".old")).collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    selfupdate::clean_up_old(&exe); // harmless when there is nothing to clean
}

#[test]
fn a_download_that_fails_its_checksum_leaves_the_working_program_untouched() {
    let s = spawn(false);
    let exe = fake_exe("tamper");
    let asset = selfupdate::asset_name(std::env::consts::OS, std::env::consts::ARCH).unwrap();
    match selfupdate::install(&client(&s), &repo(&s, "rel-bad"), &asset, &exe) {
        Err(ToolError::Verify(_)) => {}
        other => panic!("expected a checksum failure, got {other:?}"),
    }
    assert_eq!(std::fs::read(&exe).unwrap(), b"OLD PROGRAM");
    assert_eq!(std::fs::read_dir(exe.parent().unwrap()).unwrap().count(), 1, "no staged download may be left");
}

#[test]
fn the_updater_walks_through_check_install_and_the_restart_notice() {
    let s = spawn(true);
    let exe = fake_exe("updater");
    let u = Updater::new(client(&s), repo(&s, "rel"), exe.clone(), "0.1.0");
    assert_eq!(u.banner_html(), "");
    assert!(u.install_now().is_err(), "nothing to install before a check has found something");
    assert_eq!(u.check_now().unwrap().as_deref(), Some("9.9.9"));
    let banner = u.banner_html();
    assert!(banner.contains("Pryxea 9.9.9 is available (you have 0.1.0)") && banner.contains("Update now"), "{banner}");
    assert_eq!(u.install_now().unwrap(), "9.9.9");
    assert_eq!(std::fs::read(&exe).unwrap(), TOOL_PAYLOAD);
    let banner = u.banner_html();
    assert!(banner.contains("Updated to Pryxea 9.9.9") && banner.contains("Close and reopen") && !banner.contains("Update now"), "{banner}");
    assert_eq!(u.available(), None, "the same update must not be offered twice");
}

#[test]
fn a_failed_install_keeps_offering_the_update() {
    let s = spawn(false);
    let exe = fake_exe("failed");
    let u = Updater::new(client(&s), repo(&s, "rel-bad"), exe.clone(), "0.1.0");
    assert_eq!(u.check_now().unwrap().as_deref(), Some("9.9.9"));
    assert!(u.install_now().is_err());
    assert_eq!(u.available().as_deref(), Some("9.9.9"));
    assert_eq!(u.installed(), None);
    assert_eq!(std::fs::read(&exe).unwrap(), b"OLD PROGRAM");
}
