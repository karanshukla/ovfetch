//! Decides whether this build is a release, for `stamp::VERSION`.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=OVFETCH_VERSION");
    println!("cargo:rerun-if-changed=build.rs");
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let stamp = stamp(
        std::env::var("OVFETCH_VERSION").ok(),
        &std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION"),
        Path::new(&manifest_dir),
    );
    if let Some(v) = stamp {
        println!("cargo:rustc-env=OVFETCH_STAMP={v}");
    }
}

/// `OVFETCH_VERSION` when release CI sets it. Otherwise the crate sources
/// outside a git checkout are a `cargo install ovfetch` from the registry (the
/// published tarball has no `.git`), which is a release too. A checkout is dev.
pub fn stamp(env: Option<String>, pkg_version: &str, manifest_dir: &Path) -> Option<String> {
    match env.filter(|v| !v.is_empty()) {
        Some(v) => Some(v),
        None if manifest_dir.join(".git").exists() => None,
        None => Some(pkg_version.to_string()),
    }
}
