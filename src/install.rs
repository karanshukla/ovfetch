//! Downloading, re-verifying, assembling, and installing a resolved plan.

use crate::build;
use crate::consensus::Trust;
use crate::libs::is_runtime_lib;
use crate::net;
pub use crate::prefix::{installed_openvino, verify};
use crate::prefix::{installed_sha256, place, refuse_foreign};
use crate::resolve::{Kind, Plan};
use crate::tempdir::TempDir;
use crate::version::Ver;
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};

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
        // create_new, so a repeated name is an error: a_flattened_name_collision_is_an_error.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&out)
            .with_context(|| format!("unpacking {name} to {}", out.display()))?;
        std::io::copy(&mut entry, &mut file)?;
    }
    Ok(())
}

pub struct Options {
    pub allow_unverified: bool,
    pub allow_downgrade: bool,
    pub force: bool,
    /// Where `--build` keeps sources and objects between runs.
    pub build_dir: PathBuf,
    pub jobs: usize,
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
    use crate::listing::sorted_file_names;
    use crate::prefix::{LOCK, write_sums};

    fn zip_of(entries: &[(&str, &[u8])]) -> TempDir {
        use std::io::Write;
        let dir = TempDir::new().unwrap();
        let mut w = zip::ZipWriter::new(fs::File::create(dir.0.join("a.whl")).unwrap());
        for (name, data) in entries {
            w.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
        dir
    }

    #[test]
    fn unzip_never_writes_outside_dest() {
        let zip = zip_of(&[
            ("../../libup.so", b"x"),
            ("/abs/libabs.so", b"x"),
            ("onnxruntime/capi/libok.so", b"ok"),
        ]);
        let dest = zip.0.join("out");
        fs::create_dir(&dest).unwrap();
        unzip(&zip.0.join("a.whl"), &dest, |_| true, false).unwrap();
        assert_eq!(sorted_file_names(&zip.0), ["a.whl", "out"]);
        // `enclosed_name` keeps an absolute path, re-rooted under dest.
        assert!(!Path::new("/abs/libabs.so").exists());
        assert_eq!(
            fs::read(dest.join("onnxruntime/capi/libok.so")).unwrap(),
            b"ok"
        );
    }

    #[test]
    fn a_flattened_name_collision_is_an_error() {
        let zip = zip_of(&[("a/libx.so", b"first"), ("b/libx.so", b"second")]);
        let dest = zip.0.join("out");
        fs::create_dir(&dest).unwrap();
        let err = unzip(&zip.0.join("a.whl"), &dest, |_| true, true).unwrap_err();
        assert!(format!("{err:#}").contains("b/libx.so"), "{err:#}");
        assert_eq!(fs::read(dest.join("libx.so")).unwrap(), b"first");
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
}
