//! How build.rs decides between a release and a dev build.

#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;

use build_script::stamp;
use std::fs;
use std::path::PathBuf;

fn dir(name: &str, with_git: bool) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    if with_git {
        fs::write(dir.join(".git"), "gitdir: elsewhere").unwrap();
    }
    dir
}

#[test]
fn a_release_build_reports_its_version() {
    let checkout = dir("stamp-checkout-ci", true);
    assert_eq!(
        stamp(Some("0.6.0".into()), "0.6.0", &checkout).as_deref(),
        Some("0.6.0")
    );
}

#[test]
fn a_registry_install_reports_its_version() {
    let tarball = dir("stamp-tarball", false);
    assert_eq!(stamp(None, "0.6.0", &tarball).as_deref(), Some("0.6.0"));
}

#[test]
fn an_unstamped_build_reports_dev() {
    let checkout = dir("stamp-checkout", true);
    assert_eq!(stamp(None, "0.6.0", &checkout), None);
    assert_eq!(stamp(Some(String::new()), "0.6.0", &checkout), None);
}
