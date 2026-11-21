//! The shared libraries staged for a prefix: which files count, the symlink
//! names a linker expects, and where each library may search for others.

use crate::elf;
use crate::listing::{file_names, regular_files};
use anyhow::{Context, Result, bail};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

const ONLY_SEARCH_PATH: &str = "$ORIGIN";

pub(crate) fn is_runtime_lib(name: &str) -> bool {
    let file = name.rsplit('/').next().unwrap_or(name);
    file.starts_with("lib") && file.contains(".so")
}

fn is_shared_object_name(name: &str) -> bool {
    name.ends_with(".so") || name.contains(".so.")
}

/// Gives the staged libraries in `lib` every name something links or loads
/// them by, as symlinks.
pub(crate) fn normalise_links(lib: &Path) -> Result<()> {
    link_identical_copies(lib)?;
    link_onnxruntime_names(lib)?;
    link_sonames(lib)?;
    link_unversioned_names(lib)
}

/// Wheels store soname aliases as full copies: byte-identical files of one
/// library become symlinks to the longest name.
fn link_identical_copies(lib: &Path) -> Result<()> {
    let mut files = regular_files(lib)?;
    files.sort_by_key(|p| std::cmp::Reverse(p.file_name().map(|n| n.len()).unwrap_or(0)));
    let library = |p: &Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.split(".so").next())
            .map(str::to_owned)
    };
    let mut kept: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    for file in files {
        let bytes = fs::read(&file)?;
        if let Some((original, _)) = kept
            .iter()
            .find(|(k, b)| library(k) == library(&file) && *b == bytes)
        {
            fs::remove_file(&file)?;
            symlink(original.file_name().context("no file name")?, &file)?;
        } else {
            kept.push((file, bytes));
        }
    }
    Ok(())
}

/// ort-sys links against `libonnxruntime.so{,.1}`, which the wheel omits.
fn link_onnxruntime_names(lib: &Path) -> Result<()> {
    let names = file_names(lib)?;
    if !names.iter().any(|n| n.starts_with("libonnxruntime")) {
        return Ok(());
    }
    let full_version = names
        .iter()
        .find(|n| n.starts_with("libonnxruntime.so.") && n.matches('.').count() == 4)
        .context("no versioned libonnxruntime.so in the build")?;
    for (link, target) in [
        ("libonnxruntime.so.1", full_version.as_str()),
        ("libonnxruntime.so", "libonnxruntime.so.1"),
    ] {
        let link = lib.join(link);
        if !link.exists() {
            symlink(target, link)?;
        }
    }
    Ok(())
}

fn link_sonames(lib: &Path) -> Result<()> {
    for path in regular_files(lib)? {
        let Some(soname) = elf::soname(&fs::read(&path)?) else {
            continue;
        };
        if soname.contains('/') || soname == ".." {
            bail!("{} has an invalid soname {soname:?}", path.display());
        }
        let file_name = path.file_name().context("no file name")?;
        let link = lib.join(&soname);
        if soname != file_name.to_string_lossy() && link.symlink_metadata().is_err() {
            symlink(file_name, link)?;
        }
    }
    Ok(())
}

/// `-lopenvino_c` and openvino-sys resolve `libopenvino_c.so`; the openvino
/// wheel ships only versioned names, so each gets one from its shortest.
fn link_unversioned_names(lib: &Path) -> Result<()> {
    let mut versioned: Vec<String> = file_names(lib)?
        .into_iter()
        .filter(|n| n.contains(".so."))
        .collect();
    versioned.sort_by_key(|n| (n.len(), n.clone()));
    for name in versioned {
        let Some((library, _)) = name.split_once(".so.") else {
            continue;
        };
        let link = lib.join(format!("{library}.so"));
        if link.symlink_metadata().is_err() {
            symlink(&name, link)?;
        }
    }
    Ok(())
}

fn is_allowed_search_path(path: &str) -> bool {
    if path == ONLY_SEARCH_PATH {
        return true;
    }
    let Some(rest) = path.strip_prefix(&format!("{ONLY_SEARCH_PATH}/")) else {
        return false;
    };
    !rest.is_empty()
        && rest
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

/// A library may only search its own directory: an absolute RUNPATH from a
/// build tree would load whatever sits there later, outside SHA256SUMS.
/// A library that cannot be read fails closed.
pub(crate) fn check_search_paths(lib: &Path) -> Result<()> {
    for path in regular_files(lib)? {
        let bytes = fs::read(&path)?;
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let paths = match elf::search_paths(&bytes) {
            Some(paths) => paths,
            None if is_shared_object_name(&name) || elf::is_elf(&bytes) => bail!(
                "{} is not an ELF64 library ovfetch can read, so its library search path can't be checked",
                path.display()
            ),
            None => continue,
        };
        if let Some(outside) = paths.iter().find(|p| !is_allowed_search_path(p)) {
            bail!(
                "{} searches {outside:?} for libraries; only {ONLY_SEARCH_PATH} is allowed",
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elf::tests::elf_with_runpath;
    use crate::tempdir;
    use std::path::Path;

    #[test]
    fn libraries_may_only_search_their_own_directory() {
        let Some(libc) = ["/lib/x86_64-linux-gnu/libc.so.6", "/usr/lib64/libc.so.6"]
            .iter()
            .find_map(|p| fs::read(p).ok())
        else {
            return;
        };
        assert_eq!(elf::search_paths(&libc), Some(vec![]));
        let dir = tempdir::private().unwrap();
        fs::write(dir.path().join("libx.so"), &libc).unwrap();
        check_search_paths(dir.path()).unwrap();
    }

    #[test]
    fn runpath_is_read_from_program_headers() {
        let elf = elf_with_runpath(b"$ORIGIN:/home/u/build/lib");
        assert_eq!(
            elf::search_paths(&elf),
            Some(vec!["$ORIGIN".into(), "/home/u/build/lib".into()])
        );
        let dir = tempdir::private().unwrap();
        fs::write(dir.path().join("libbuilt.so.1"), &elf).unwrap();
        let err = check_search_paths(dir.path()).unwrap_err().to_string();
        assert!(err.contains("/home/u/build/lib"), "{err}");
    }

    #[test]
    fn an_origin_runpath_passes() {
        let dir = tempdir::private().unwrap();
        fs::write(dir.path().join("libok.so"), elf_with_runpath(b"$ORIGIN")).unwrap();
        fs::write(
            dir.path().join("libsub.so"),
            elf_with_runpath(b"$ORIGIN/sub"),
        )
        .unwrap();
        check_search_paths(dir.path()).unwrap();
    }

    #[test]
    fn empty_or_parent_traversing_runpaths_are_refused() {
        for bad in [
            &b""[..],
            b":$ORIGIN",
            b"$ORIGIN:",
            b"$ORIGIN::",
            b"$ORIGIN/..",
            b"$ORIGIN/../lib",
            b"$ORIGIN/sub/../../lib",
            b"$ORIGIN/.",
            b"$ORIGIN/",
            b"$ORIGIN//sub",
        ] {
            let dir = tempdir::private().unwrap();
            fs::write(dir.path().join("libbad.so"), elf_with_runpath(bad)).unwrap();
            assert!(
                check_search_paths(dir.path()).is_err(),
                "runpath {:?} should be refused",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn a_non_utf8_runpath_is_still_checked() {
        let dir = tempdir::private().unwrap();
        fs::write(
            dir.path().join("libodd.so"),
            elf_with_runpath(b"\xff:/home/u/build"),
        )
        .unwrap();
        assert!(check_search_paths(dir.path()).is_err());
    }

    #[test]
    fn a_library_that_cannot_be_read_fails_closed() {
        let dir = tempdir::private().unwrap();
        let mut truncated = elf_with_runpath(b"/home/u/build");
        truncated.truncate(100); // cuts through the program headers
        fs::write(dir.path().join("libcut.so.2"), truncated).unwrap();
        assert!(check_search_paths(dir.path()).is_err());

        let dir = tempdir::private().unwrap();
        fs::write(dir.path().join("libnot.so"), b"not elf").unwrap();
        assert!(check_search_paths(dir.path()).is_err());

        // Not a library, not ELF: nothing to check.
        let dir = tempdir::private().unwrap();
        fs::write(dir.path().join("plugins.xml"), b"<xml/>").unwrap();
        check_search_paths(dir.path()).unwrap();
    }

    #[test]
    fn unversioned_names_link_to_the_shortest_versioned_one() {
        let dir = tempdir::private().unwrap();
        let p = dir.path();
        fs::write(p.join("libonnxruntime.so.1.24.1"), b"ort").unwrap();
        fs::write(p.join("libopenvino_c.so.2641"), b"c").unwrap();
        fs::write(p.join("libopenvino_c.so.2026.4.1"), b"other").unwrap();
        fs::write(p.join("libtbb.so"), b"own").unwrap();
        fs::write(p.join("libtbb.so.12"), b"tbb").unwrap();
        normalise_links(p).unwrap();
        let target = |n: &str| fs::read_link(p.join(n)).unwrap();
        assert_eq!(
            target("libopenvino_c.so"),
            Path::new("libopenvino_c.so.2641")
        );
        assert_eq!(
            target("libonnxruntime.so"),
            Path::new("libonnxruntime.so.1")
        );
        assert!(!p.join("libtbb.so").is_symlink());
    }

    #[test]
    fn openvino_alone_needs_no_onnxruntime() {
        let dir = tempdir::private().unwrap();
        let p = dir.path();
        fs::write(p.join("libopenvino.so.2641"), b"ov").unwrap();
        fs::write(p.join("libopenvino_c.so.2641"), b"c").unwrap();
        normalise_links(p).unwrap();
        assert_eq!(
            fs::read_link(p.join("libopenvino_c.so")).unwrap(),
            Path::new("libopenvino_c.so.2641")
        );
        assert!(!p.join("libonnxruntime.so").exists());
    }
}
