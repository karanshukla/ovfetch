//! A private scratch directory that removes itself.

use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) struct TempDir(pub(crate) PathBuf);

impl TempDir {
    /// A new directory only the current user can enter. Install runs as root
    /// and copies what lands here into the prefix, so a path someone else
    /// created first in /tmp is an error, not something to reuse.
    pub(crate) fn new() -> Result<Self> {
        Self::new_in(&std::env::temp_dir(), "ovfetch")
    }

    pub(crate) fn new_in(dir: &Path, stem: &str) -> Result<Self> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.subsec_nanos();
        let p = dir.join(format!("{stem}-{}-{nanos}", std::process::id()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&p)
            .with_context(|| format!("creating {}", p.display()))?;
        Ok(Self(p))
    }

    /// The path, no longer removed on drop.
    pub(crate) fn keep(self) -> PathBuf {
        let p = self.0.clone();
        std::mem::forget(self);
        p
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn temp_dir_is_private() {
        let dir = TempDir::new().unwrap();
        let mode = fs::metadata(&dir.0).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
