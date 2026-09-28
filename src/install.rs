//! Downloading, re-verifying, assembling, and installing a resolved plan.

use crate::consensus::Trust;
use crate::net;
use crate::resolve::Plan;
use crate::version::Ver;
use anyhow::{Context, Result, bail};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

const SUMS: &str = "SHA256SUMS";
const LOCK: &str = "ovfetch.lock.json";

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<Self> {
        let p = std::env::temp_dir().join(format!("ovfetch-{}", std::process::id()));
        fs::create_dir_all(&p)?;
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
    Ok(())
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
    if !bad.is_empty() {
        bail!(
            "{} does not match its {SUMS}:\n  {}",
            prefix.display(),
            bad.join("\n  ")
        );
    }
    Ok(())
}

pub struct Options {
    pub allow_unverified: bool,
    pub allow_downgrade: bool,
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
        let n = e.file_name().to_string_lossy().into_owned();
        if n.starts_with("libonnxruntime")
            || n.starts_with("libopenvino")
            || n.starts_with("libtbb")
            || n == SUMS
            || n == LOCK
        {
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
