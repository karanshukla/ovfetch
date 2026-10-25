//! The binary, run as a user would, for the commands that work offline.
#![cfg(feature = "cli")]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

fn ovfetch(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ovfetch"))
        .args(args)
        .output()
        .unwrap()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// A fresh directory under the target dir, removed first if a past run left it.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn prints_its_version() {
    let o = ovfetch(&["--version"]);
    assert!(o.status.success());
    assert_eq!(
        String::from_utf8(o.stdout).unwrap().trim(),
        format!("ovfetch {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn detect_prints_json() {
    let o = ovfetch(&["detect"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(v["devices"].is_array());
    assert!(v["warnings"].is_array());
}

#[test]
fn verify_checks_a_prefix_against_its_sums() {
    let dir = scratch("verify");
    let p = dir.to_str().unwrap();
    let o = ovfetch(&["verify", "--prefix", p]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("was it installed by ovfetch"),
        "{}",
        stderr(&o)
    );

    const OV_SHA256: &str = "5e1c26b2f7c5b8ae6e0b3da7fcc99b841697e85d65febbb57f24b2b7a199b037";
    fs::write(dir.join("libopenvino.so.1"), b"ov").unwrap();
    fs::write(
        dir.join("SHA256SUMS"),
        format!("{OV_SHA256}  libopenvino.so.1\n"),
    )
    .unwrap();
    let o = ovfetch(&["verify", "--prefix", p]);
    assert!(o.status.success(), "{}", stderr(&o));

    fs::write(dir.join("libopenvino.so.1"), b"tampered").unwrap();
    let o = ovfetch(&["verify", "--prefix", p]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("libopenvino.so.1: modified"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn a_bad_data_dir_is_an_error() {
    let dir = scratch("data-dir");
    let o = ovfetch(&["--data-dir", dir.to_str().unwrap(), "detect"]);
    assert!(!o.status.success());
}

#[test]
fn build_and_openvino_only_conflict() {
    let o = ovfetch(&["resolve", "--build", "--openvino-only"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("cannot be used with"), "{}", stderr(&o));
}
