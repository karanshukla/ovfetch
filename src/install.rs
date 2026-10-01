//! Downloading, re-verifying, assembling, and installing a resolved plan.

use crate::consensus::Trust;
use crate::net;
use crate::resolve::Plan;
use crate::version::Ver;
use anyhow::{Context, Result, bail};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SUMS: &str = "SHA256SUMS";
const LOCK: &str = "ovfetch.lock.json";

struct TempDir(PathBuf);

impl TempDir {
    /// A new directory only the current user can enter. Install runs as root
    /// and copies what lands here into the prefix, so a path someone else
    /// created first in /tmp is an error, not something to reuse.
    fn new() -> Result<Self> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.subsec_nanos();
        let p = std::env::temp_dir().join(format!("ovfetch-{}-{nanos}", std::process::id()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&p)
            .with_context(|| format!("creating {}", p.display()))?;
        Ok(Self(p))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Downloads from PyPI, falling back to a mirror only when PyPI is
/// unreachable. Mirrors otherwise just vote on the hash. A digest mismatch
/// aborts rather than falling through: it means the bytes served differ from
/// what every source agreed on.
fn fetch(urls: &[String], sha256: &str, dest: &Path) -> Result<()> {
    let mut last = None;
    for url in urls {
        match net::download(url, dest) {
            Ok(got) if got == sha256 => return Ok(()),
            Ok(got) => bail!(
                "{url} served bytes with sha256 {got}, but every source agreed on {sha256}. Aborting."
            ),
            Err(e) => {
                eprintln!("note: {e:#}; trying the next mirror");
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no source to download from")))
}

fn unzip(archive: &Path, dest: &Path, keep: impl Fn(&str) -> bool, flatten: bool) -> Result<()> {
    let mut zip = zip::ZipArchive::new(fs::File::open(archive)?)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let Some(rel) = entry.enclosed_name() else {
            continue;
        };
        let name = rel.to_string_lossy().into_owned();
        if entry.is_dir() || !keep(&name) {
            continue;
        }
        let out = if flatten {
            dest.join(rel.file_name().context("entry without a file name")?)
        } else {
            dest.join(&rel)
        };
        fs::create_dir_all(out.parent().context("entry without a parent")?)?;
        std::io::copy(&mut entry, &mut fs::File::create(&out)?)?;
    }
    Ok(())
}

fn is_runtime_lib(name: &str) -> bool {
    let file = name.rsplit('/').next().unwrap_or(name);
    file.starts_with("lib") && file.contains(".so")
}

/// Wheels store soname aliases as full copies. Collapse byte-identical files
/// into symlinks to the longest name, and add the `libonnxruntime.so{,.1}`
/// names ort-sys links against, which the wheel omits.
fn normalise_links(lib: &Path) -> Result<()> {
    let mut files: Vec<PathBuf> = fs::read_dir(lib)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && !p.is_symlink())
        .collect();
    files.sort_by_key(|p| std::cmp::Reverse(p.file_name().map(|n| n.len()).unwrap_or(0)));
    let stem = |p: &Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.split(".so").next())
            .map(str::to_owned)
    };
    let mut kept: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    for f in files {
        let bytes = fs::read(&f)?;
        if let Some((target, _)) = kept
            .iter()
            .find(|(k, b)| stem(k) == stem(&f) && *b == bytes)
        {
            fs::remove_file(&f)?;
            symlink(target.file_name().context("no file name")?, &f)?;
        } else {
            kept.push((f, bytes));
        }
    }
    let full = fs::read_dir(lib)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| n.starts_with("libonnxruntime.so.") && n.matches('.').count() == 4)
        .context("no versioned libonnxruntime.so in the build")?;
    for (link, target) in [
        ("libonnxruntime.so.1", full.as_str()),
        ("libonnxruntime.so", "libonnxruntime.so.1"),
    ] {
        let p = lib.join(link);
        if !p.exists() {
            symlink(target, p)?;
        }
    }
    for entry in fs::read_dir(lib)?.flatten() {
        let path = entry.path();
        if !path.is_file() || path.is_symlink() {
            continue;
        }
        if let Some(soname) = soname(&fs::read(&path)?) {
            let link = lib.join(&soname);
            if soname != entry.file_name().to_string_lossy() && link.symlink_metadata().is_err() {
                symlink(entry.file_name(), link)?;
            }
        }
    }
    Ok(())
}

/// DT_SONAME of a little-endian ELF64 shared object, if it has one.
fn soname(elf: &[u8]) -> Option<String> {
    fn uint(b: &[u8], at: usize, len: usize) -> Option<usize> {
        let raw = b.get(at..at.checked_add(len)?)?;
        Some(raw.iter().rev().fold(0usize, |a, &x| (a << 8) | x as usize))
    }
    if elf.get(..4)? != b"\x7fELF" || *elf.get(4)? != 2 || *elf.get(5)? != 1 {
        return None;
    }
    let shoff = uint(elf, 0x28, 8)?;
    let shentsize = uint(elf, 0x3a, 2)?;
    let shnum = uint(elf, 0x3c, 2)?;
    let section = |i: usize| shoff.checked_add(i.checked_mul(shentsize)?);
    for i in 0..shnum {
        let sh = section(i)?;
        if uint(elf, sh + 4, 4)? != 6 {
            continue; // not SHT_DYNAMIC
        }
        let (off, size) = (uint(elf, sh + 0x18, 8)?, uint(elf, sh + 0x20, 8)?);
        let strtab = section(uint(elf, sh + 0x28, 4)?)?;
        let str_off = uint(elf, strtab + 0x18, 8)?;
        for d in (off..off.checked_add(size)?).step_by(16) {
            if uint(elf, d, 8)? == 14 {
                let start = str_off.checked_add(uint(elf, d + 8, 8)?)?;
                let name = elf.get(start..)?;
                let end = name.iter().position(|&c| c == 0)?;
                return String::from_utf8(name[..end].to_vec()).ok();
            }
        }
    }
    None
}

fn write_sums(dir: &Path) -> Result<()> {
    let mut names: Vec<String> = fs::read_dir(dir)?
        .flatten()
        .filter(|e| e.path().is_file() && !e.path().is_symlink())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != SUMS && n != LOCK)
        .collect();
    names.sort();
    let mut out = String::new();
    for n in names {
        out.push_str(&format!(
            "{}  {n}\n",
            net::sha256_hex(&fs::read(dir.join(&n))?)
        ));
    }
    fs::write(dir.join(SUMS), out)?;
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
    for line in sums.lines() {
        let Some((digest, name)) = line.split_once("  ") else {
            continue;
        };
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
    let listed: Vec<&str> = sums
        .lines()
        .filter_map(|l| l.split_once("  "))
        .map(|(_, n)| n)
        .collect();
    let mut out = Vec::new();
    for e in fs::read_dir(prefix)?.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        let owned = n == SUMS
            || n == LOCK
            || listed.contains(&n.as_str())
            || (e.path().is_symlink()
                && fs::canonicalize(e.path()).is_ok_and(|t| {
                    t.parent() == fs::canonicalize(prefix).ok().as_deref()
                        && t.file_name()
                            .is_some_and(|f| listed.contains(&&*f.to_string_lossy()))
                }));
        if !owned {
            out.push(n);
        }
    }
    out.sort();
    Ok(out)
}

/// What install clears from a prefix before copying the new build in.
fn is_replaced(name: &str) -> bool {
    name.starts_with("libonnxruntime")
        || name.starts_with("libopenvino")
        || name.starts_with("libtbb")
        || name == SUMS
        || name == LOCK
}

pub struct Options {
    pub allow_unverified: bool,
    pub allow_downgrade: bool,
    pub force: bool,
}

fn installed_sha256(prefix: &Path) -> Option<String> {
    let lock: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(prefix.join(LOCK)).ok()?).ok()?;
    Some(lock["artifact"]["sha256"].as_str()?.to_owned())
}

/// OpenVINO version of whatever ovfetch installed in `prefix` before.
pub fn installed_openvino(prefix: &Path) -> Option<Ver> {
    let lock: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(prefix.join(LOCK)).ok()?).ok()?;
    Ver::parse(lock["openvino"].as_str()?)
}

pub fn install(
    plan: &Plan,
    prefix: &Path,
    opts: &Options,
    mirror_urls: impl Fn(&str) -> Vec<String>,
) -> Result<()> {
    if let (Some(have), Some(want)) = (installed_openvino(prefix), Ver::parse(&plan.openvino))
        && want < have
        && !opts.allow_downgrade
    {
        bail!(
            "{} already has OpenVINO {have}, newer than the {want} this plan resolves to. \
                 Keeping it; pass --allow-downgrade to replace it anyway.",
            prefix.display()
        );
    }
    if plan.artifact.trust == Trust::Unverified && !opts.allow_unverified {
        bail!(
            "{} is not in the reviewed ledger (see `ovfetch resolve`).\n\
             Sources agreed on their hashes, but nothing pins them yet. Pass --allow-unverified to accept that.",
            plan.artifact.id
        );
    }
    if !opts.force
        && installed_sha256(prefix).as_deref() == Some(plan.artifact.sha256.as_str())
        && verify(prefix).is_ok()
    {
        eprintln!(
            "already installed: {} matches {}; pass --force to reinstall",
            prefix.display(),
            plan.artifact.id
        );
        return Ok(());
    }
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
    let tmp = TempDir::new()?;
    let lib = tmp.0.join("lib");
    fs::create_dir_all(&lib)?;

    let whl = tmp.0.join("ort.whl");
    let mut urls = mirror_urls(&plan.artifact.id);
    if urls.is_empty() {
        urls.push(plan.artifact.url.clone());
    }
    fetch(&urls, &plan.artifact.sha256, &whl)?;
    unzip(
        &whl,
        &lib,
        |n| n.starts_with("onnxruntime/capi/") && is_runtime_lib(n),
        true,
    )?;

    normalise_links(&lib)?;
    write_sums(&lib)?;
    fs::write(lib.join(LOCK), serde_json::to_string_pretty(plan)?)?;
    verify(&lib)?;

    fs::create_dir_all(prefix).with_context(|| format!("creating {}", prefix.display()))?;
    for e in fs::read_dir(prefix)?.flatten() {
        if is_replaced(&e.file_name().to_string_lossy()) {
            fs::remove_file(e.path())?;
        }
    }
    for e in fs::read_dir(&lib)?.flatten() {
        let dest = prefix.join(e.file_name());
        if e.path().is_symlink() {
            symlink(fs::read_link(e.path())?, dest)?;
        } else {
            fs::copy(e.path(), dest)?;
        }
    }
    verify(prefix)?;
    eprintln!(
        "installed ONNX Runtime {} + OpenVINO {} into {}",
        plan.artifact.onnxruntime,
        plan.openvino,
        prefix.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_soname_from_a_shared_object() {
        let Some(libc) = ["/lib/x86_64-linux-gnu/libc.so.6", "/usr/lib64/libc.so.6"]
            .iter()
            .find_map(|p| fs::read(p).ok())
        else {
            return;
        };
        assert_eq!(soname(&libc).as_deref(), Some("libc.so.6"));
        assert_eq!(soname(b"not elf"), None);
    }

    #[test]
    fn temp_dir_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let mode = fs::metadata(&dir.0).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

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
}
