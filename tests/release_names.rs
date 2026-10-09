//! The release workflow and the built-in updater must agree on file names, or an
//! update would look for a file that was never published.

use pryxea::selfupdate::asset_name;

#[test]
fn the_release_workflow_publishes_exactly_the_files_the_updater_asks_for() {
    let workflow = std::fs::read_to_string(format!("{}/.github/workflows/release.yml", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let mut published: Vec<String> = workflow.lines().filter_map(|l| l.split("asset: ").nth(1)).map(|rest| rest.split([',', ' ', '}']).next().unwrap_or("").to_string()).collect();
    published.sort();
    let mut wanted: Vec<String> = [("windows", "x86_64"), ("linux", "x86_64"), ("linux", "aarch64"), ("macos", "aarch64"), ("macos", "x86_64")].iter().filter_map(|(os, arch)| asset_name(os, arch)).collect();
    wanted.sort();
    assert_eq!(published, wanted);
    assert!(workflow.contains("SHA256SUMS"), "the updater verifies downloads against a SHA256SUMS file");
}
