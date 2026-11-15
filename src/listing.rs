//! What a directory holds, one level deep. Unreadable entries are skipped.

use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn file_names(dir: &Path) -> Result<Vec<String>> {
    Ok(fs::read_dir(dir)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect())
}

/// Files that are not symlinks, and not directories.
pub(crate) fn regular_files(dir: &Path) -> Result<Vec<PathBuf>> {
    Ok(fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && !p.is_symlink())
        .collect())
}

#[cfg(test)]
pub(crate) fn sorted_file_names(dir: &Path) -> Vec<String> {
    let mut names = file_names(dir).unwrap();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempdir::TempDir;
    use std::os::unix::fs::symlink;

    #[test]
    fn regular_files_leave_out_symlinks_and_directories() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.0.join("libreal.so.1"), b"x").unwrap();
        symlink("libreal.so.1", dir.0.join("libreal.so")).unwrap();
        fs::create_dir(dir.0.join("nested")).unwrap();
        assert_eq!(regular_files(&dir.0).unwrap(), [dir.0.join("libreal.so.1")]);
        assert_eq!(
            sorted_file_names(&dir.0),
            ["libreal.so", "libreal.so.1", "nested"]
        );
    }
}
