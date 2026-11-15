//! An installed prefix: its `SHA256SUMS`, its lock file, and replacing it whole.

use crate::libs;
use crate::listing::regular_files;
use crate::net;
use crate::tempdir::TempDir;
use crate::version::Ver;
use anyhow::{Context, Result, bail};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

pub(crate) const SUMS: &str = "SHA256SUMS";
pub(crate) const LOCK: &str = "ovfetch.lock.json";

/// `(digest, file name)` for each line of a sums file, the two-space form
/// `sha256sum` writes.
fn sums_entries(sums: &str) -> impl Iterator<Item = (&str, &str)> {
    sums.lines().filter_map(|line| line.split_once("  "))
}

pub(crate) fn write_sums(dir: &Path) -> Result<()> {
    let mut names: Vec<String> = regular_files(dir)?
        .iter()
        .filter_map(|p| Some(p.file_name()?.to_string_lossy().into_owned()))
        .filter(|n| n != SUMS && n != LOCK)
        .collect();
    names.sort();
    let mut sums = String::new();
    for name in names {
        let digest = net::sha256_hex(&fs::read(dir.join(&name))?);
        sums.push_str(&format!("{digest}  {name}\n"));
    }
    fs::write(dir.join(SUMS), sums)?;
    Ok(())
}

pub fn verify(prefix: &Path) -> Result<()> {
    let sums = fs::read_to_string(prefix.join(SUMS)).with_context(|| {
        format!(
            "{} has no {SUMS}; was it installed by ovfetch?",
            prefix.display()
        )
    })?;
    let mut bad = Vec::new();
    for (digest, name) in sums_entries(&sums) {
        match fs::read(prefix.join(name)) {
            Ok(bytes) if net::sha256_hex(&bytes) == digest => {}
            Ok(_) => bad.push(format!("{name}: modified")),
            Err(e) => bad.push(format!("{name}: {e}")),
        }
    }
    for name in untracked(prefix, &sums)? {
        bad.push(format!("{name}: not installed by ovfetch"));
    }
    if !bad.is_empty() {
        bail!(
            "{} does not match its {SUMS}:\n  {}",
            prefix.display(),
            bad.join("\n  ")
        );
    }
    Ok(())
}

/// Entries in `prefix` that `sums` does not cover: anything other than a
/// listed file, a symlink to one, or the sums and lock files themselves.
fn untracked(prefix: &Path, sums: &str) -> Result<Vec<String>> {
    let listed: Vec<&str> = sums_entries(sums).map(|(_, name)| name).collect();
    let links_to_a_listed_file = |link: &Path| {
        link.is_symlink()
            && fs::canonicalize(link).is_ok_and(|target| {
                target.parent() == fs::canonicalize(prefix).ok().as_deref()
                    && target
                        .file_name()
                        .is_some_and(|f| listed.contains(&&*f.to_string_lossy()))
            })
    };
    let mut untracked = Vec::new();
    for entry in fs::read_dir(prefix)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let tracked = name == SUMS
            || name == LOCK
            || listed.contains(&name.as_str())
            || links_to_a_listed_file(&entry.path());
        if !tracked {
            untracked.push(name);
        }
    }
    untracked.sort();
    Ok(untracked)
}

/// What install clears from a prefix before copying the new build in.
fn is_replaced(name: &str) -> bool {
    name.starts_with("libonnxruntime")
        || name.starts_with("libopenvino")
        || name.starts_with("libtbb")
        || name == SUMS
        || name == LOCK
}

/// Moves `staged` into place at `prefix`, keeping the old prefix whole until
/// the new one is there: a failure leaves one install or the other, never a
/// mix of both.
fn swap_in(staged: &Path, prefix: &Path, old_stem: &str) -> Result<()> {
    if !prefix.exists() {
        return fs::rename(staged, prefix)
            .with_context(|| format!("moving the new install to {}", prefix.display()));
    }
    let parent = prefix.parent().unwrap_or(Path::new("."));
    // Not a guard: if the restore below fails, this is the only copy left.
    let old = TempDir::new_in(parent, old_stem)?.keep();
    fs::remove_dir(&old)?;
    fs::rename(prefix, &old).with_context(|| format!("moving {} aside", prefix.display()))?;
    if let Err(e) = fs::rename(staged, prefix) {
        fs::rename(&old, prefix).with_context(|| {
            format!(
                "restoring {} from {} after: {e}",
                prefix.display(),
                old.display()
            )
        })?;
        return Err(e).with_context(|| format!("moving the new install to {}", prefix.display()));
    }
    if let Err(e) = fs::remove_dir_all(&old) {
        eprintln!(
            "note: could not remove the old install at {}: {e}",
            old.display()
        );
    }
    Ok(())
}

/// Refuses a prefix holding files ovfetch did not install, so replacing it
/// never deletes someone else's.
pub(crate) fn refuse_foreign(prefix: &Path) -> Result<()> {
    if prefix.exists() {
        let sums = fs::read_to_string(prefix.join(SUMS)).unwrap_or_default();
        let foreign: Vec<String> = untracked(prefix, &sums)?
            .into_iter()
            .filter(|n| !is_replaced(n))
            .collect();
        if !foreign.is_empty() {
            bail!(
                "{} holds files ovfetch did not install and will not remove:\n  {}\n\
                 Delete them, or pick an empty --prefix.",
                prefix.display(),
                foreign.join("\n  ")
            );
        }
    }
    Ok(())
}

/// Where `prefix` will end up: a symlink is replaced where it points, not
/// unlinked, and a relative path is made absolute.
pub(crate) fn resolved(prefix: &Path) -> Result<PathBuf> {
    match fs::canonicalize(prefix) {
        Ok(p) => Ok(p),
        Err(_) => Ok(std::path::absolute(prefix)?),
    }
}

/// Checks the libraries staged in `lib`, records their sums and `lock`, and
/// swaps them in for `prefix`. Returns the prefix as resolved.
pub(crate) fn place(lib: &Path, prefix: &Path, lock: &serde_json::Value) -> Result<PathBuf> {
    libs::normalise_links(lib)?;
    libs::check_search_paths(lib)?;
    write_sums(lib)?;
    fs::write(lib.join(LOCK), serde_json::to_string_pretty(lock)?)?;
    verify(lib)?;
    let prefix = &resolved(prefix)?;
    let parent = match prefix.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let name = prefix
        .file_name()
        .with_context(|| format!("{} names no directory", prefix.display()))?
        .to_string_lossy();
    let staged = TempDir::new_in(parent, &format!(".{name}.ovfetch-new"))?;
    for e in fs::read_dir(lib)?.flatten() {
        let dest = staged.0.join(e.file_name());
        if e.path().is_symlink() {
            symlink(fs::read_link(e.path())?, dest)?;
        } else {
            fs::copy(e.path(), dest)?;
        }
    }
    let mode = fs::metadata(prefix).map_or(0o755, |m| m.permissions().mode());
    fs::set_permissions(&staged.0, fs::Permissions::from_mode(mode))?;
    verify(&staged.0)?;
    swap_in(&staged.0, prefix, &format!(".{name}.ovfetch-old"))?;
    verify(prefix)?;
    Ok(prefix.clone())
}

/// The lock ovfetch wrote into `prefix` at install, if there is one.
fn lock(prefix: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&fs::read_to_string(prefix.join(LOCK)).ok()?).ok()
}

pub(crate) fn installed_sha256(prefix: &Path) -> Option<String> {
    Some(lock(prefix)?["artifact"]["sha256"].as_str()?.to_owned())
}

/// What `npu install` left in `prefix`: `compiler` or `full`.
pub(crate) fn installed_npu_mode(prefix: &Path) -> Option<String> {
    Some(lock(prefix)?["mode"].as_str()?.to_owned())
}

/// OpenVINO version of whatever ovfetch installed in `prefix` before.
pub fn installed_openvino(prefix: &Path) -> Option<Ver> {
    Ver::parse(lock(prefix)?["openvino"].as_str()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elf::tests::elf_with_runpath;
    use crate::listing::sorted_file_names;

    #[test]
    fn verify_flags_files_it_did_not_write() {
        let dir = TempDir::new().unwrap();
        let p = &dir.0;
        fs::write(p.join("libonnxruntime.so.1.23.0"), b"ort").unwrap();
        symlink("libonnxruntime.so.1.23.0", p.join("libonnxruntime.so.1")).unwrap();
        write_sums(p).unwrap();
        fs::write(p.join(LOCK), "{}").unwrap();
        verify(p).unwrap();

        fs::write(
            p.join("onnxruntime_pybind11_state.cpython-313-x86_64-linux-gnu.so"),
            b"x",
        )
        .unwrap();
        symlink("/etc/hostname", p.join("libstray.so")).unwrap();
        let err = format!("{:#}", verify(p).unwrap_err());
        assert!(
            err.contains(
                "onnxruntime_pybind11_state.cpython-313-x86_64-linux-gnu.so: not installed"
            )
        );
        assert!(err.contains("libstray.so: not installed"));
        assert!(!err.contains("libonnxruntime"));
    }

    #[test]
    fn a_swap_replaces_the_whole_prefix() {
        let dir = TempDir::new().unwrap();
        let (prefix, staged) = (dir.0.join("ort"), dir.0.join("staged"));
        fs::create_dir(&prefix).unwrap();
        fs::write(prefix.join("libopenvino.so.2026.3.0"), b"old").unwrap();
        fs::create_dir(&staged).unwrap();
        fs::write(staged.join("libopenvino.so.2026.4.1"), b"new").unwrap();
        swap_in(&staged, &prefix, ".ort.ovfetch-old").unwrap();
        assert_eq!(sorted_file_names(&prefix), ["libopenvino.so.2026.4.1"]);
        assert_eq!(sorted_file_names(&dir.0), ["ort"]);
    }

    #[test]
    fn a_failed_swap_keeps_the_old_prefix() {
        let dir = TempDir::new().unwrap();
        let prefix = dir.0.join("ort");
        fs::create_dir(&prefix).unwrap();
        fs::write(prefix.join("libopenvino.so.2026.3.0"), b"old").unwrap();
        assert!(swap_in(&dir.0.join("missing"), &prefix, ".ort.ovfetch-old").is_err());
        assert_eq!(
            fs::read(prefix.join("libopenvino.so.2026.3.0")).unwrap(),
            b"old"
        );
        assert_eq!(sorted_file_names(&dir.0), ["ort"]);
    }

    #[test]
    fn place_replaces_a_symlinked_prefix_where_it_points() {
        let dir = TempDir::new().unwrap();
        let real = dir.0.join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("libopenvino.so.2026.3.0"), b"old").unwrap();
        write_sums(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o750)).unwrap();
        symlink(&real, dir.0.join("link")).unwrap();

        let lib = dir.0.join("staging");
        fs::create_dir(&lib).unwrap();
        fs::write(lib.join("libfoo.so.1"), elf_with_runpath(b"$ORIGIN")).unwrap();
        let got = place(&lib, &dir.0.join("link"), &serde_json::json!({})).unwrap();

        assert_eq!(got, fs::canonicalize(&real).unwrap());
        assert!(dir.0.join("link").is_symlink());
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o750
        );
        assert_eq!(
            sorted_file_names(&real),
            ["SHA256SUMS", "libfoo.so", "libfoo.so.1", LOCK]
        );
        verify(&real).unwrap();
        // Nothing staged or set aside is left beside it.
        assert_eq!(sorted_file_names(&dir.0), ["link", "real", "staging"]);
    }
}
