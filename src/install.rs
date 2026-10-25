//! Downloading, re-verifying, assembling, and installing a resolved plan.

use crate::build;
use crate::consensus::Trust;
use crate::net;
use crate::resolve::{Kind, Plan};
use crate::version::Ver;
use anyhow::{Context, Result, bail};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SUMS: &str = "SHA256SUMS";
const LOCK: &str = "ovfetch.lock.json";

pub(crate) struct TempDir(pub(crate) PathBuf);

impl TempDir {
    /// A new directory only the current user can enter. Install runs as root
    /// and copies what lands here into the prefix, so a path someone else
    /// created first in /tmp is an error, not something to reuse.
    pub(crate) fn new() -> Result<Self> {
        Self::new_in(&std::env::temp_dir(), "ovfetch")
    }

    fn new_in(dir: &Path, stem: &str) -> Result<Self> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.subsec_nanos();
        let p = dir.join(format!("{stem}-{}-{nanos}", std::process::id()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&p)
            .with_context(|| format!("creating {}", p.display()))?;
        Ok(Self(p))
    }

    /// The path, no longer removed on drop.
    fn keep(self) -> PathBuf {
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

/// Downloads from PyPI, falling back to a mirror only when PyPI is
/// unreachable. Mirrors otherwise just vote on the hash. A digest mismatch
/// aborts rather than falling through: it means the bytes served differ from
/// what every source agreed on.
pub(crate) fn fetch(urls: &[String], sha256: &str, dest: &Path) -> Result<()> {
    fetch_with(urls, sha256, dest, net::download)
}

/// [`fetch`] with the downloader passed in: it writes `url` to the path and
/// returns the sha256 of what it wrote.
fn fetch_with(
    urls: &[String],
    sha256: &str,
    dest: &Path,
    download: impl Fn(&str, &Path) -> Result<String>,
) -> Result<()> {
    let mut last = None;
    for url in urls {
        match download(url, dest) {
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

pub(crate) fn unzip(
    archive: &Path,
    dest: &Path,
    keep: impl Fn(&str) -> bool,
    flatten: bool,
) -> Result<()> {
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

pub(crate) fn is_runtime_lib(name: &str) -> bool {
    let file = name.rsplit('/').next().unwrap_or(name);
    file.starts_with("lib") && file.contains(".so")
}

/// Wheels store soname aliases as full copies. Collapse byte-identical files
/// into symlinks to the longest name, and add the `libonnxruntime.so{,.1}`
/// names ort-sys links against, which the wheel omits, when there is an ONNX
/// Runtime at all.
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
    let names: Vec<String> = fs::read_dir(lib)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    if names.iter().any(|n| n.starts_with("libonnxruntime")) {
        let full = names
            .iter()
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
    }
    for entry in fs::read_dir(lib)?.flatten() {
        let path = entry.path();
        if !path.is_file() || path.is_symlink() {
            continue;
        }
        if let Some(soname) = soname(&fs::read(&path)?) {
            if soname.contains('/') || soname == ".." {
                bail!("{} has an invalid soname {soname:?}", path.display());
            }
            let link = lib.join(&soname);
            if soname != entry.file_name().to_string_lossy() && link.symlink_metadata().is_err() {
                symlink(entry.file_name(), link)?;
            }
        }
    }
    // The onnxruntime-openvino wheel ships `libopenvino_c.so`, the name `-lopenvino_c`
    // and openvino-sys resolve; the openvino wheel a build installs ships only sonames.
    let mut versioned: Vec<String> = fs::read_dir(lib)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".so."))
        .collect();
    versioned.sort_by_key(|n| (n.len(), n.clone()));
    for name in versioned {
        let Some((stem, _)) = name.split_once(".so.") else {
            continue;
        };
        let link = lib.join(format!("{stem}.so"));
        if link.symlink_metadata().is_err() {
            symlink(&name, link)?;
        }
    }
    Ok(())
}

/// DT_SONAME of a little-endian ELF64 shared object, if it has one.
fn soname(elf: &[u8]) -> Option<String> {
    dynamic_strings(elf, 14)?.into_iter().next()
}

/// Every RPATH and RUNPATH entry, `None` when `elf` is not one ovfetch can read.
pub(crate) fn search_paths(elf: &[u8]) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for tag in [15, 29] {
        for s in dynamic_strings(elf, tag)? {
            out.extend(s.split(':').filter(|p| !p.is_empty()).map(str::to_owned));
        }
    }
    Some(out)
}

/// Strings for one dynamic tag of a little-endian ELF64 file, read the way the
/// loader reads them: the `PT_DYNAMIC` segment, with `DT_STRTAB` mapped to a
/// file offset through the `PT_LOAD` segments. Section headers are not used;
/// they can be stripped, or disagree with what the loader sees. `None` when
/// the file is not such an ELF, or is malformed.
fn dynamic_strings(elf: &[u8], tag: usize) -> Option<Vec<String>> {
    fn uint(b: &[u8], at: usize, len: usize) -> Option<usize> {
        let raw = b.get(at..at.checked_add(len)?)?;
        Some(raw.iter().rev().fold(0usize, |a, &x| (a << 8) | x as usize))
    }
    let field = |at: usize, off: usize, len: usize| uint(elf, at.checked_add(off)?, len);
    if elf.get(..4)? != b"\x7fELF" || *elf.get(4)? != 2 || *elf.get(5)? != 1 {
        return None;
    }
    let (phoff, phentsize, phnum) = (field(0, 0x20, 8)?, field(0, 0x36, 2)?, field(0, 0x38, 2)?);
    // (p_type, p_offset, p_vaddr, p_filesz)
    let headers = (0..phnum)
        .map(|i| {
            let at = phoff.checked_add(i.checked_mul(phentsize)?)?;
            Some((
                field(at, 0, 4)?,
                field(at, 0x08, 8)?,
                field(at, 0x10, 8)?,
                field(at, 0x20, 8)?,
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    const PT_LOAD: usize = 1;
    const PT_DYNAMIC: usize = 2;
    let Some(&(_, dyn_off, _, dyn_size)) = headers.iter().find(|h| h.0 == PT_DYNAMIC) else {
        return Some(Vec::new()); // statically linked: nothing to search
    };
    let file_offset = |addr: usize| {
        headers
            .iter()
            .find(|h| h.0 == PT_LOAD && h.2 <= addr && addr - h.2 < h.3)
            .and_then(|h| (addr - h.2).checked_add(h.1))
    };
    let (mut strtab, mut wanted) = (None, Vec::new());
    for d in (dyn_off..dyn_off.checked_add(dyn_size)?).step_by(16) {
        match field(d, 0, 8)? {
            0 => break, // DT_NULL
            5 => strtab = Some(field(d, 8, 8)?),
            t if t == tag => wanted.push(field(d, 8, 8)?),
            _ => {}
        }
    }
    if wanted.is_empty() {
        return Some(Vec::new());
    }
    let base = file_offset(strtab?)?;
    wanted
        .into_iter()
        .map(|o| {
            let name = elf.get(base.checked_add(o)?..)?;
            let end = name.iter().position(|&c| c == 0)?;
            // Lossy, so one odd byte can't hide the rest of the path.
            Some(String::from_utf8_lossy(&name[..end]).into_owned())
        })
        .collect()
}

/// A library may only search its own directory: an absolute RUNPATH from a
/// build tree would load whatever sits there later, outside SHA256SUMS.
pub(crate) fn check_search_paths(lib: &Path) -> Result<()> {
    for e in fs::read_dir(lib)?.flatten() {
        let path = e.path();
        if !path.is_file() || path.is_symlink() {
            continue;
        }
        let bytes = fs::read(&path)?;
        let name = e.file_name().to_string_lossy().into_owned();
        let shared_object = name.ends_with(".so") || name.contains(".so.");
        let paths = match search_paths(&bytes) {
            Some(paths) => paths,
            // Fail closed: what can't be read can't be shown to search only $ORIGIN.
            None if shared_object || bytes.starts_with(b"\x7fELF") => bail!(
                "{} is not an ELF64 library ovfetch can read, so its library search path can't be checked",
                path.display()
            ),
            None => continue,
        };
        if let Some(bad) = paths
            .iter()
            .find(|p| *p != "$ORIGIN" && !p.starts_with("$ORIGIN/"))
        {
            bail!(
                "{} searches {bad} for libraries; only $ORIGIN is allowed",
                path.display()
            );
        }
    }
    Ok(())
}

pub(crate) fn write_sums(dir: &Path) -> Result<()> {
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
    normalise_links(lib)?;
    check_search_paths(lib)?;
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

pub struct Options {
    pub allow_unverified: bool,
    pub allow_downgrade: bool,
    pub force: bool,
    /// Where `--build` keeps sources and objects between runs.
    pub build_dir: PathBuf,
    pub jobs: usize,
}

pub(crate) fn installed_sha256(prefix: &Path) -> Option<String> {
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
    refuse_foreign(prefix)?;
    let tmp = TempDir::new()?;
    let lib = tmp.0.join("lib");
    fs::create_dir_all(&lib)?;

    let fetch_input = |id: &str, url: &str, sha256: &str, dest: &Path| {
        let mut urls = mirror_urls(id);
        if urls.is_empty() {
            urls.push(url.to_owned());
        }
        fetch(&urls, sha256, dest)
    };
    let mut lock = serde_json::to_value(plan)?;
    match plan.artifact.kind {
        Kind::Wheel => {
            let whl = tmp.0.join("ort.whl");
            fetch_input(
                &plan.artifact.id,
                &plan.artifact.url,
                &plan.artifact.sha256,
                &whl,
            )?;
            unzip(
                &whl,
                &lib,
                |n| n.starts_with("onnxruntime/capi/") && is_runtime_lib(n),
                true,
            )?;
        }
        Kind::Openvino => {
            let whl = tmp.0.join("openvino.whl");
            fetch_input(
                &plan.artifact.id,
                &plan.artifact.url,
                &plan.artifact.sha256,
                &whl,
            )?;
            unzip(
                &whl,
                &lib,
                |n| {
                    n.strip_prefix("openvino/libs/")
                        .is_some_and(|f| !f.contains('/') && build::is_openvino_runtime(f))
                },
                true,
            )?;
            if fs::read_dir(&lib)?.next().is_none() {
                bail!("{} holds no OpenVINO libraries", plan.artifact.id);
            }
        }
        Kind::Build => {
            let recipe = plan
                .artifact
                .build
                .as_ref()
                .context("build plan without a recipe")?;
            lock["built"] = build::stage(plan, recipe, &lib, opts, &fetch_input)?;
        }
    }

    let prefix = &place(&lib, prefix, &lock)?;
    let what = match &plan.artifact.onnxruntime {
        Some(ort) => format!("ONNX Runtime {ort} + OpenVINO {}", plan.openvino),
        None => format!("OpenVINO {}", plan.openvino),
    };
    eprintln!(
        "installed {what}{} into {}",
        if plan.artifact.kind == Kind::Build {
            " (built here)"
        } else {
            ""
        },
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
    fn libraries_may_only_search_their_own_directory() {
        let Some(libc) = ["/lib/x86_64-linux-gnu/libc.so.6", "/usr/lib64/libc.so.6"]
            .iter()
            .find_map(|p| fs::read(p).ok())
        else {
            return;
        };
        assert_eq!(search_paths(&libc), Some(vec![]));
        let dir = TempDir::new().unwrap();
        fs::write(dir.0.join("libx.so"), &libc).unwrap();
        check_search_paths(&dir.0).unwrap();
    }

    /// A minimal ELF64 shared object with one DT_RUNPATH and no section
    /// headers, so only the program headers say where its dynamic strings are.
    fn elf_with_runpath(runpath: &[u8]) -> Vec<u8> {
        const BASE: u64 = 0x40_0000;
        let (ph, dynamic, strtab) = (64u64, 64 + 2 * 56, 64 + 2 * 56 + 3 * 16);
        let mut strings = vec![0u8];
        strings.extend_from_slice(runpath);
        strings.push(0);
        let len = strtab + strings.len() as u64;
        let mut f = vec![0u8; len as usize];
        let put = |f: &mut Vec<u8>, at: u64, v: u64, n: usize| {
            f[at as usize..at as usize + n].copy_from_slice(&v.to_le_bytes()[..n])
        };
        f[..4].copy_from_slice(b"\x7fELF");
        f[4] = 2; // ELFCLASS64
        f[5] = 1; // little-endian
        f[6] = 1;
        put(&mut f, 0x10, 3, 2); // ET_DYN
        put(&mut f, 0x12, 62, 2); // x86-64
        put(&mut f, 0x20, ph, 8);
        put(&mut f, 0x36, 56, 2);
        put(&mut f, 0x38, 2, 2);
        // PT_LOAD over the whole file, PT_DYNAMIC over the three entries.
        for (i, (ty, off, size)) in [(1u64, 0, len), (2, dynamic, 3 * 16)]
            .into_iter()
            .enumerate()
        {
            let at = ph + i as u64 * 56;
            put(&mut f, at, ty, 4);
            put(&mut f, at + 0x08, off, 8);
            put(&mut f, at + 0x10, BASE + off, 8);
            put(&mut f, at + 0x20, size, 8);
        }
        for (i, (tag, val)) in [(5u64, BASE + strtab), (29, 1), (0, 0)]
            .into_iter()
            .enumerate()
        {
            put(&mut f, dynamic + i as u64 * 16, tag, 8);
            put(&mut f, dynamic + i as u64 * 16 + 8, val, 8);
        }
        f[strtab as usize..].copy_from_slice(&strings);
        f
    }

    #[test]
    fn runpath_is_read_from_program_headers() {
        let elf = elf_with_runpath(b"$ORIGIN:/home/u/build/lib");
        assert_eq!(
            search_paths(&elf),
            Some(vec!["$ORIGIN".into(), "/home/u/build/lib".into()])
        );
        let dir = TempDir::new().unwrap();
        fs::write(dir.0.join("libbuilt.so.1"), &elf).unwrap();
        let err = check_search_paths(&dir.0).unwrap_err().to_string();
        assert!(err.contains("/home/u/build/lib"), "{err}");
    }

    #[test]
    fn an_origin_runpath_passes() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.0.join("libok.so"), elf_with_runpath(b"$ORIGIN/../lib")).unwrap();
        check_search_paths(&dir.0).unwrap();
    }

    #[test]
    fn a_non_utf8_runpath_is_still_checked() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.0.join("libodd.so"),
            elf_with_runpath(b"\xff:/home/u/build"),
        )
        .unwrap();
        assert!(check_search_paths(&dir.0).is_err());
    }

    #[test]
    fn a_library_that_cannot_be_read_fails_closed() {
        let dir = TempDir::new().unwrap();
        let mut truncated = elf_with_runpath(b"/home/u/build");
        truncated.truncate(100); // cuts through the program headers
        fs::write(dir.0.join("libcut.so.2"), truncated).unwrap();
        assert!(check_search_paths(&dir.0).is_err());

        let dir = TempDir::new().unwrap();
        fs::write(dir.0.join("libnot.so"), b"not elf").unwrap();
        assert!(check_search_paths(&dir.0).is_err());

        // Not a library, not ELF: nothing to check.
        let dir = TempDir::new().unwrap();
        fs::write(dir.0.join("plugins.xml"), b"<xml/>").unwrap();
        check_search_paths(&dir.0).unwrap();
    }

    #[test]
    fn unversioned_names_link_to_the_shortest_versioned_one() {
        let dir = TempDir::new().unwrap();
        let p = &dir.0;
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
        let dir = TempDir::new().unwrap();
        let p = &dir.0;
        fs::write(p.join("libopenvino.so.2641"), b"ov").unwrap();
        fs::write(p.join("libopenvino_c.so.2641"), b"c").unwrap();
        normalise_links(p).unwrap();
        assert_eq!(
            fs::read_link(p.join("libopenvino_c.so")).unwrap(),
            Path::new("libopenvino_c.so.2641")
        );
        assert!(!p.join("libonnxruntime.so").exists());
    }

    #[test]
    fn temp_dir_is_private() {
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

    #[test]
    fn a_swap_replaces_the_whole_prefix() {
        let dir = TempDir::new().unwrap();
        let (prefix, staged) = (dir.0.join("ort"), dir.0.join("staged"));
        fs::create_dir(&prefix).unwrap();
        fs::write(prefix.join("libopenvino.so.2026.3.0"), b"old").unwrap();
        fs::create_dir(&staged).unwrap();
        fs::write(staged.join("libopenvino.so.2026.4.1"), b"new").unwrap();
        swap_in(&staged, &prefix, ".ort.ovfetch-old").unwrap();
        let names: Vec<_> = fs::read_dir(&prefix)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names, ["libopenvino.so.2026.4.1"]);
        let left: Vec<_> = fs::read_dir(&dir.0)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left, ["ort"]);
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
        let left: Vec<_> = fs::read_dir(&dir.0)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left, ["ort"]);
    }

    #[test]
    fn a_hash_mismatch_aborts_without_trying_the_next_mirror() {
        let dir = TempDir::new().unwrap();
        let tried = std::cell::RefCell::new(Vec::new());
        let urls = ["https://a/x".to_owned(), "https://b/x".to_owned()];
        let err = fetch_with(&urls, "good", &dir.0.join("x"), |url, _| {
            tried.borrow_mut().push(url.to_owned());
            Ok("tampered".into())
        })
        .unwrap_err();
        assert!(err.to_string().contains("Aborting"), "{err}");
        assert_eq!(*tried.borrow(), ["https://a/x"]);
    }

    #[test]
    fn only_a_transport_error_falls_through_to_the_next_mirror() {
        let dir = TempDir::new().unwrap();
        let urls = ["https://a/x".to_owned(), "https://b/x".to_owned()];
        let dl = |url: &str, _: &Path| match url {
            "https://a/x" => bail!("connection refused"),
            _ => Ok("good".to_owned()),
        };
        fetch_with(&urls, "good", &dir.0.join("x"), dl).unwrap();

        let all_down = |url: &str, _: &Path| -> Result<String> { bail!("{url} is down") };
        let err = fetch_with(&urls, "good", &dir.0.join("x"), all_down).unwrap_err();
        assert_eq!(err.to_string(), "https://b/x is down");

        let err = fetch_with(&[], "good", &dir.0.join("x"), all_down).unwrap_err();
        assert!(err.to_string().contains("no source"), "{err}");
    }

    fn plan(openvino: &str, sha256: &str, trust: Trust) -> Plan {
        use crate::resolve::{Artifact, Npu};
        Plan {
            floor: crate::status::Floor {
                openvino: None,
                reason: String::new(),
            },
            driver_ceiling: None,
            openvino: openvino.into(),
            artifact: Artifact {
                id: "pypi/openvino/openvino-x.whl".into(),
                onnxruntime: None,
                // Refused by check_url, so a test that gets as far as a
                // download fails offline.
                url: "http://not-allowed.invalid/openvino-x.whl".into(),
                sha256: sha256.into(),
                trust,
                kind: Kind::Openvino,
                build: None,
            },
            npu: Npu {
                driver_installed: None,
                driver_required: None,
                compiler_present: false,
            },
            warnings: vec![],
        }
    }

    fn options() -> Options {
        Options {
            allow_unverified: false,
            allow_downgrade: false,
            force: false,
            build_dir: PathBuf::new(),
            jobs: 1,
        }
    }

    /// A prefix as ovfetch leaves it: one library, its sums, and a lock.
    fn installed(openvino: &str, sha256: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::write(dir.0.join("libopenvino.so.2026.4.1"), b"ov").unwrap();
        write_sums(&dir.0).unwrap();
        fs::write(
            dir.0.join(LOCK),
            serde_json::json!({"openvino": openvino, "artifact": {"sha256": sha256}}).to_string(),
        )
        .unwrap();
        dir
    }

    #[test]
    fn a_newer_install_is_kept_unless_downgrade_is_allowed() {
        let prefix = installed("2026.4.1", "old");
        let p = plan("2026.3.0", "new", Trust::Verified);
        let no_fetch = |_: &str| -> Vec<String> { panic!("fetched") };
        let err = install(&p, &prefix.0, &options(), no_fetch).unwrap_err();
        assert!(err.to_string().contains("--allow-downgrade"), "{err}");

        // Allowed, it moves on to the next gate.
        let p = plan("2026.3.0", "new", Trust::Unverified);
        let opts = Options {
            allow_downgrade: true,
            ..options()
        };
        let err = install(&p, &prefix.0, &opts, no_fetch).unwrap_err();
        assert!(err.to_string().contains("--allow-unverified"), "{err}");
    }

    #[test]
    fn an_unverified_artifact_needs_the_flag() {
        let prefix = TempDir::new().unwrap();
        let p = plan("2026.4.1", "new", Trust::Unverified);
        let no_fetch = |_: &str| -> Vec<String> { panic!("fetched") };
        let err = install(&p, &prefix.0.join("ort"), &options(), no_fetch).unwrap_err();
        assert!(err.to_string().contains("--allow-unverified"), "{err}");
        assert!(!prefix.0.join("ort").exists());
    }

    #[test]
    fn a_matching_intact_install_is_left_alone() {
        let prefix = installed("2026.4.1", "same");
        let p = plan("2026.4.1", "same", Trust::Verified);
        install(&p, &prefix.0, &options(), |_| panic!("fetched")).unwrap();
        verify(&prefix.0).unwrap();
    }

    /// Runs install expecting it to go past every gate to the download, which
    /// the plan's URL makes fail offline.
    fn reaches_the_download(p: &Plan, prefix: &Path, opts: &Options) {
        let asked = std::cell::Cell::new(false);
        let err = install(p, prefix, opts, |_| {
            asked.set(true);
            vec![]
        })
        .unwrap_err();
        assert!(asked.get(), "stopped before the download: {err:#}");
        assert!(err.to_string().contains("non-HTTPS"), "{err:#}");
    }

    #[test]
    fn force_or_a_modified_install_downloads_again() {
        let prefix = installed("2026.4.1", "same");
        let p = plan("2026.4.1", "same", Trust::Verified);
        let force = Options {
            force: true,
            ..options()
        };
        reaches_the_download(&p, &prefix.0, &force);

        fs::write(prefix.0.join("libopenvino.so.2026.4.1"), b"changed").unwrap();
        reaches_the_download(&p, &prefix.0, &options());
        // The failed download left the prefix as it was.
        assert_eq!(
            fs::read(prefix.0.join("libopenvino.so.2026.4.1")).unwrap(),
            b"changed"
        );
    }

    #[test]
    fn a_prefix_with_foreign_files_is_refused_and_left_intact() {
        let prefix = installed("2026.4.1", "old");
        fs::write(prefix.0.join("notes.txt"), b"mine").unwrap();
        let p = plan("2026.4.1", "new", Trust::Verified);
        let err = install(&p, &prefix.0, &options(), |_| panic!("fetched")).unwrap_err();
        assert!(err.to_string().contains("notes.txt"), "{err}");
        assert_eq!(fs::read(prefix.0.join("notes.txt")).unwrap(), b"mine");
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
        let mut names: Vec<String> = fs::read_dir(&real)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["SHA256SUMS", "libfoo.so", "libfoo.so.1", LOCK]);
        verify(&real).unwrap();
        // Nothing staged or set aside is left beside it.
        let mut left: Vec<String> = fs::read_dir(&dir.0)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["link", "real", "staging"]);
    }
}
