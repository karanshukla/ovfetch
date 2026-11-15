//! Private scratch directories that remove themselves, from `tempfile`.

use anyhow::{Context, Result};
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
pub(crate) use tempfile::TempDir;

const OWNER_ONLY: u32 = 0o700;

/// A new directory only the current user can enter. Install runs as root
/// and copies what lands here into the prefix, so a path someone else
/// created first in /tmp is an error, not something to reuse.
pub(crate) fn private() -> Result<TempDir> {
    private_in(&std::env::temp_dir(), "ovfetch")
}

/// The same, created in `dir` with a name that starts with `stem`.
pub(crate) fn private_in(dir: &Path, stem: &str) -> Result<TempDir> {
    tempfile::Builder::new()
        .prefix(stem)
        .permissions(Permissions::from_mode(OWNER_ONLY))
        .tempdir_in(dir)
        .with_context(|| format!("creating {stem} in {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn temp_dir_is_private() {
        let dir = private().unwrap();
        let mode = fs::metadata(dir.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn a_dir_made_beside_a_prefix_is_private_too() {
        let parent = private().unwrap();
        let dir = private_in(parent.path(), ".ort.ovfetch-new").unwrap();
        let mode = fs::metadata(dir.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        assert_eq!(dir.path().parent(), Some(parent.path()));
        let name = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(name.starts_with(".ort.ovfetch-new"), "{name}");
    }

    #[test]
    fn two_dirs_with_one_stem_do_not_collide() {
        let parent = private().unwrap();
        let first = private_in(parent.path(), "stem").unwrap();
        let second = private_in(parent.path(), "stem").unwrap();
        assert_ne!(first.path(), second.path());
    }

    #[test]
    fn a_missing_parent_is_an_error_not_a_fallback() {
        let parent = private().unwrap();
        assert!(private_in(&parent.path().join("absent"), "stem").is_err());
    }

    #[test]
    fn the_dir_and_its_contents_go_when_it_is_dropped() {
        let dir = private().unwrap();
        let path = dir.path().to_owned();
        fs::create_dir(path.join("lib")).unwrap();
        fs::write(path.join("lib/libx.so"), b"x").unwrap();
        drop(dir);
        assert!(!path.exists());
    }

    #[test]
    fn a_kept_dir_outlives_its_guard() {
        let parent = private().unwrap();
        let kept = private_in(parent.path(), "old").unwrap().keep();
        assert!(kept.is_dir());
    }
}
