//! Fetching Intel's NPU compiler, and optionally the user-mode driver, from a
//! linux-npu-driver release into a prefix, with no dpkg and no root.

use crate::ci::NPU_DRIVER_REPO;
use crate::consensus::Trust;
use crate::data::Data;
use crate::detect::{self, Machine, NPU_PRELOAD};
use crate::install::{self, TempDir};
use crate::sources;
use crate::version::Ver;
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Read;
use std::os::unix::fs::symlink;
use std::path::Path;

const COMPILER_PKG: &str = "intel-driver-compiler-npu";
const DRIVER_PKG: &str = "intel-level-zero-npu";
/// Where the packages put their libraries.
const LIB_DIR: &str = "usr/lib/x86_64-linux-gnu/";
/// Ubuntu 24.04's build is the one every release since 1.22 ships, and its
/// glibc is older than any current distro's.
const PREFERRED_BASE: &str = "ubuntu2404";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Only the compiler, for a host driver that was packaged without one.
    Compiler,
    /// The user-mode driver and its compiler, used instead of the host's.
    Full,
}

pub struct Options {
    pub driver: Option<String>,
    pub full: bool,
    pub allow_unverified: bool,
    pub force: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct Plan {
    pub version: String,
    pub mode: Mode,
    pub id: String,
    pub url: String,
    pub sha256: String,
    pub trust: Trust,
}

/// Which release, and how much of it. The compiler has to come from the same
/// release as the driver that loads it.
fn choose(host: &Machine, data: &Data, opts: &Options) -> Result<(String, Mode)> {
    match (&host.npu_driver, opts.full) {
        (Some(have), false) => {
            if let Some(want) = &opts.driver
                && Ver::parse(want) != Ver::parse(have)
            {
                bail!(
                    "the host's NPU driver is {have}, and its compiler must come from the same \
                     release. Pass --full to install driver {want} alongside it instead."
                );
            }
            Ok((have.clone(), Mode::Compiler))
        }
        _ => {
            if let Some(want) = &opts.driver {
                return Ok((want.clone(), Mode::Full));
            }
            let codenames: Vec<&str> = host
                .devices
                .iter()
                .filter(|d| d.kind == "npu")
                .filter_map(|d| data.platforms.list.iter().find(|p| p.pci_id == d.pci_id))
                .map(|p| p.codename.as_str())
                .filter(|c| !c.is_empty())
                .collect();
            if codenames.is_empty() {
                bail!("no NPU with a known platform; pass --driver to pick a release");
            }
            data.npu_drivers
                .list
                .iter()
                .filter(|d| codenames.iter().all(|c| d.platforms.iter().any(|p| p == c)))
                .filter(|d| tarball(data, &d.version).is_some())
                .max_by_key(|d| Ver::parse(&d.version))
                .map(|d| (d.version.clone(), Mode::Full))
                .with_context(|| {
                    format!(
                        "no recorded NPU driver release is verified on {}; pass --driver",
                        codenames.join(", ")
                    )
                })
        }
    }
}

/// The ledger id of a release's per-distro tarball, preferring Ubuntu 24.04's.
fn tarball(data: &Data, version: &str) -> Option<String> {
    let prefix = format!("github/{NPU_DRIVER_REPO}/v{version}/linux-npu-driver-");
    let mut ids: Vec<&str> = data
        .ledger
        .list
        .iter()
        .map(|e| e.id.as_str())
        .filter(|id| id.starts_with(&prefix) && id.ends_with(".tar.gz"))
        .collect();
    ids.sort_by_key(|id| !id.contains(PREFERRED_BASE));
    ids.first().map(|id| id.to_string())
}

pub fn plan(host: &Machine, data: &Data, opts: &Options) -> Result<Plan> {
    let (version, mode) = choose(host, data, opts)?;
    let tag = format!("v{version}");
    let (id, sha256, trust) = match tarball(data, &version) {
        Some(id) => {
            let digest = data
                .ledger
                .get(&id)
                .context("ledger entry vanished")?
                .digest
                .clone();
            (id, digest, Trust::Verified)
        }
        None => {
            // GitHub is the only source for these assets, so there is no
            // quorum to fall back on: an unrecorded one is GitHub's word alone.
            let release = sources::github_release(NPU_DRIVER_REPO, &tag)
                .with_context(|| format!("looking up NPU driver release {tag}"))?;
            let mut assets: Vec<_> = release
                .assets
                .iter()
                .filter(|a| a.name.starts_with("linux-npu-driver-") && a.name.ends_with(".tar.gz"))
                .collect();
            assets.sort_by_key(|a| !a.name.contains(PREFERRED_BASE));
            let a = assets.first().with_context(|| {
                format!("NPU driver release {tag} has no per-distro tarball ovfetch can read")
            })?;
            (
                format!("github/{NPU_DRIVER_REPO}/{tag}/{}", a.name),
                a.sha256.clone(),
                Trust::Unverified,
            )
        }
    };
    let name = id.rsplit('/').next().unwrap_or_default();
    Ok(Plan {
        url: format!("https://github.com/{NPU_DRIVER_REPO}/releases/download/{tag}/{name}"),
        version,
        mode,
        id,
        sha256,
        trust,
    })
}

/// The bytes of member `name` in an `ar` archive, the container a `.deb` is.
fn ar_member<'a>(ar: &'a [u8], name: &str) -> Result<&'a [u8]> {
    let mut rest = ar.strip_prefix(b"!<arch>\n").context("not an ar archive")?;
    while rest.len() >= 60 {
        let (header, body) = rest.split_at(60);
        let field =
            |r: std::ops::Range<usize>| String::from_utf8_lossy(&header[r]).trim().to_owned();
        let size: usize = field(48..58).parse().context("bad ar member size")?;
        let data = body.get(..size).context("truncated ar member")?;
        if field(0..16).trim_end_matches('/') == name {
            return Ok(data);
        }
        rest = body.get(size + size % 2..).unwrap_or_default();
    }
    bail!("no {name} in the package")
}

/// Copies the shared libraries from a package's `data.tar.gz` into `dest`,
/// flattened. Anything else, or anything named outside the library
/// directory, is skipped; a symlink may only name a file beside it.
fn unpack_libs(data_tar_gz: &[u8], dest: &Path) -> Result<Vec<String>> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(data_tar_gz));
    let mut names = Vec::new();
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        let Some(file) = path.trim_start_matches("./").strip_prefix(LIB_DIR) else {
            continue;
        };
        if file.contains('/') || !install::is_runtime_lib(file) {
            continue;
        }
        let out = dest.join(file);
        match entry.header().entry_type() {
            tar::EntryType::Regular => {
                std::io::copy(&mut entry, &mut fs::File::create(&out)?)?;
            }
            tar::EntryType::Symlink => {
                let target = entry
                    .link_name()?
                    .context("symlink without a target")?
                    .to_string_lossy()
                    .into_owned();
                if target.contains('/') {
                    bail!("{file} links outside its directory, to {target}");
                }
                symlink(target, &out)?;
            }
            _ => continue,
        }
        names.push(file.to_owned());
    }
    Ok(names)
}

/// Unpacks `packages` from a release tarball into `dest`, failing if any is
/// missing from it.
fn unpack(tarball: &Path, packages: &[&str], dest: &Path) -> Result<()> {
    let mut outer = tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(tarball)?));
    let mut found = Vec::new();
    for entry in outer.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        let name = path.rsplit('/').next().unwrap_or_default();
        let Some(pkg) = packages
            .iter()
            .find(|p| name.starts_with(&format!("{p}_")) && name.ends_with(".deb"))
        else {
            continue;
        };
        let mut deb = Vec::new();
        entry.read_to_end(&mut deb)?;
        let libs = unpack_libs(
            ar_member(&deb, "data.tar.gz").context(name.to_owned())?,
            dest,
        )?;
        if libs.is_empty() {
            bail!("{name} holds no libraries");
        }
        found.push(*pkg);
    }
    if let Some(missing) = packages.iter().find(|p| !found.contains(p)) {
        bail!("the release tarball has no {missing} package");
    }
    Ok(())
}

/// What an app loads, in order. The compiler comes first, because the
/// libraries before it in the chain find it by soname. A driver from the
/// prefix comes last, after the host's Level Zero loader, named by soname:
/// loaded before the loader, the driver crashes on its first call into it.
fn preload_list(prefix: &Path, compiler: &[String], mode: Mode) -> String {
    let mut lines: Vec<String> = Vec::new();
    if mode == Mode::Full {
        lines.push(detect::ZE_LOADER.into());
    }
    let mut libs: Vec<&str> = compiler.iter().rev().map(String::as_str).collect();
    if mode == Mode::Full {
        libs.push("libze_intel_npu.so.1");
    }
    lines.extend(libs.iter().map(|l| prefix.join(l).display().to_string()));
    lines.iter().map(|l| format!("{l}\n")).collect()
}

pub fn install(plan: &Plan, host: &Machine, prefix: &Path, opts: &Options) -> Result<()> {
    if plan.trust == Trust::Unverified && !opts.allow_unverified {
        bail!(
            "{} is not in the reviewed ledger, and GitHub is its only source.\n\
             Pass --allow-unverified to take GitHub's digest for it.",
            plan.id
        );
    }
    if plan.mode == Mode::Compiler && host.npu_compiler && !opts.force {
        eprintln!(
            "the host's NPU driver {} already finds its compiler; nothing to install \
             (pass --full to use a driver from {} instead)",
            plan.version,
            prefix.display()
        );
        return Ok(());
    }
    let lock_mode = fs::read_to_string(prefix.join("ovfetch.lock.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|l| l["mode"].as_str().map(str::to_owned));
    if !opts.force
        && install::installed_sha256(prefix).as_deref() == Some(plan.sha256.as_str())
        // A full install also covers a request for the compiler alone.
        && (lock_mode.as_deref() == serde_json::to_value(plan.mode)?.as_str()
            || lock_mode.as_deref() == Some("full"))
        && install::verify(prefix).is_ok()
    {
        eprintln!(
            "already installed: {} matches {}; pass --force to reinstall",
            prefix.display(),
            plan.id
        );
        return Ok(());
    }
    install::refuse_foreign(prefix)?;

    if plan.mode == Mode::Full {
        require_ze_loader()?;
    }

    let tmp = TempDir::new()?;
    let lib = tmp.0.join("lib");
    fs::create_dir_all(&lib)?;
    let tarball = tmp.0.join("driver.tar.gz");
    install::fetch(std::slice::from_ref(&plan.url), &plan.sha256, &tarball)?;
    let mut mode = plan.mode;
    if mode == Mode::Compiler {
        unpack(&tarball, &[COMPILER_PKG], &lib)?;
        if covered(&host.npu_compiler_needs, &lib, false)?.is_none() {
            eprintln!(
                "note: driver {}'s compiler is only found beside the driver, so the host's \
                 driver cannot load it from {}; installing the user-mode driver too",
                plan.version,
                prefix.display()
            );
            require_ze_loader()?;
            mode = Mode::Full;
        }
    }
    match (plan.mode, mode) {
        (Mode::Full, _) => unpack(&tarball, &[COMPILER_PKG, DRIVER_PKG], &lib)?,
        (Mode::Compiler, Mode::Full) => unpack(&tarball, &[DRIVER_PKG], &lib)?,
        (Mode::Compiler, Mode::Compiler) => {}
    }
    let needs = match mode {
        Mode::Compiler => host.npu_compiler_needs.clone(),
        Mode::Full => detect::compiler_needs(
            &fs::read(lib.join("libze_intel_npu.so.1"))
                .context("the driver package has no libze_intel_npu.so.1")?,
        ),
    };
    let route = covered(&needs, &lib, mode == Mode::Full)?.with_context(|| {
        format!(
            "release {}'s compiler package provides none of what its driver loads ({needs:?})",
            plan.version
        )
    })?;

    let final_prefix = install::resolved(prefix)?;
    fs::write(
        lib.join(NPU_PRELOAD),
        preload_list(&final_prefix, &route, mode),
    )?;
    let lock = serde_json::json!({
        "npu_driver": plan.version,
        "mode": mode,
        "artifact": { "id": plan.id, "sha256": plan.sha256, "trust": plan.trust },
    });
    let prefix = install::place(&lib, prefix, &lock)?;
    let what = match mode {
        Mode::Compiler => "compiler",
        Mode::Full => "user-mode driver and compiler",
    };
    eprintln!(
        "installed the NPU {what} from driver {} into {}",
        plan.version,
        prefix.display()
    );
    eprintln!(
        "Load each line of {} in order (dlopen, RTLD_GLOBAL) after OpenVINO's own libraries \
         and before it opens the NPU,\nor start the app with LD_LIBRARY_PATH={}",
        prefix.join(NPU_PRELOAD).display(),
        prefix.display()
    );
    Ok(())
}

fn require_ze_loader() -> Result<()> {
    if !detect::ze_loader_present() {
        bail!(
            "a driver in a prefix still needs the host's Level Zero loader ({}), and it is not \
             on the linker path. Install your distro's level-zero (libze1) package first.",
            detect::ZE_LOADER
        );
    }
    Ok(())
}

/// The first compiler route whose libraries all landed in `lib`. Unless the
/// driver is there too, a route through [`detect::COMPILER_BESIDE_DRIVER`]
/// does not count: the driver would look for it in its own directory.
fn covered(needs: &[Vec<String>], lib: &Path, with_driver: bool) -> Result<Option<Vec<String>>> {
    let have: Vec<String> = fs::read_dir(lib)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    Ok(needs
        .iter()
        .filter(|set| with_driver || !set.iter().any(|l| l == detect::COMPILER_BESIDE_DRIVER))
        .find(|set| set.iter().all(|l| have.contains(l)))
        .cloned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn ar(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = b"!<arch>\n".to_vec();
        for (name, data) in members {
            out.extend(
                format!(
                    "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                    format!("{name}/"),
                    0,
                    0,
                    0,
                    644,
                    data.len()
                )
                .as_bytes(),
            );
            out.extend_from_slice(data);
            if data.len() % 2 == 1 {
                out.push(b'\n');
            }
        }
        out
    }

    fn tar_gz(build: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        build(&mut b);
        let raw = b.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&raw).unwrap();
        gz.finish().unwrap()
    }

    fn file(b: &mut tar::Builder<Vec<u8>>, path: &str, data: &[u8]) {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, path, data).unwrap();
    }

    fn link(b: &mut tar::Builder<Vec<u8>>, path: &str, target: &str) {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_cksum();
        b.append_link(&mut h, path, target).unwrap();
    }

    #[test]
    fn finds_a_member_after_an_odd_sized_one() {
        let deb = ar(&[
            ("debian-binary", b"2.0\n"),
            ("control.tar.gz", b"odd"),
            ("data.tar.gz", b"payload"),
        ]);
        assert_eq!(ar_member(&deb, "data.tar.gz").unwrap(), b"payload");
        assert!(ar_member(&deb, "data.tar.zst").is_err());
        assert!(ar_member(b"not an archive", "data.tar.gz").is_err());
    }

    #[test]
    fn only_libraries_in_the_library_directory_are_unpacked() {
        let data = tar_gz(|b| {
            file(
                b,
                "./usr/lib/x86_64-linux-gnu/libze_intel_npu.so.1.35.0",
                b"umd",
            );
            link(
                b,
                "./usr/lib/x86_64-linux-gnu/libze_intel_npu.so.1",
                "libze_intel_npu.so.1.35.0",
            );
            file(b, "./usr/share/doc/copyright", b"text");
            file(b, "./usr/lib/x86_64-linux-gnu/nested/libx.so", b"x");
            file(b, "./etc/libfake.so", b"x");
        });
        let dir = TempDir::new().unwrap();
        let mut got = unpack_libs(&data, &dir.0).unwrap();
        got.sort();
        assert_eq!(got, ["libze_intel_npu.so.1", "libze_intel_npu.so.1.35.0"]);
        assert_eq!(
            fs::read_link(dir.0.join("libze_intel_npu.so.1")).unwrap(),
            Path::new("libze_intel_npu.so.1.35.0")
        );
    }

    #[test]
    fn a_symlink_out_of_the_directory_is_refused() {
        let data = tar_gz(|b| link(b, "usr/lib/x86_64-linux-gnu/libz.so.1", "/etc/shadow"));
        let dir = TempDir::new().unwrap();
        assert!(unpack_libs(&data, &dir.0).is_err());
    }

    #[test]
    fn unpacks_the_named_packages_from_a_release_tarball() {
        let payload = tar_gz(|b| {
            file(
                b,
                "./usr/lib/x86_64-linux-gnu/libnpu_driver_compiler.so",
                b"c",
            )
        });
        let deb = ar(&[("debian-binary", b"2.0\n"), ("data.tar.gz", &payload)]);
        let release = tar_gz(|b| {
            file(
                b,
                "./intel-driver-compiler-npu_1.32.0.1~ubuntu24.04_amd64.deb",
                &deb,
            );
            file(
                b,
                "./intel-driver-compiler-npu_1.32.0.1~ubuntu24.04_amd64.deb.asc",
                b"sig",
            );
        });
        let dir = TempDir::new().unwrap();
        let tgz = dir.0.join("r.tar.gz");
        fs::write(&tgz, release).unwrap();
        let out = dir.0.join("out");
        fs::create_dir(&out).unwrap();
        unpack(&tgz, &[COMPILER_PKG], &out).unwrap();
        assert!(out.join("libnpu_driver_compiler.so").is_file());
        let err = unpack(&tgz, &[COMPILER_PKG, DRIVER_PKG], &out).unwrap_err();
        assert!(format!("{err:#}").contains(DRIVER_PKG));
    }

    #[test]
    fn the_compiler_is_preloaded_before_what_loads_it() {
        let route = vec![
            "libopenvino_intel_npu_compiler_loader.so".to_string(),
            "libopenvino_intel_npu_compiler.so".to_string(),
        ];
        assert_eq!(
            preload_list(Path::new("/p"), &route, Mode::Full),
            "libze_loader.so.1\n/p/libopenvino_intel_npu_compiler.so\n/p/libopenvino_intel_npu_compiler_loader.so\n/p/libze_intel_npu.so.1\n"
        );
    }

    fn host(driver: Option<&str>) -> Machine {
        Machine {
            devices: vec![detect::Device {
                kind: "npu",
                pci_id: "0xfd3e".into(),
                slot: String::new(),
            }],
            npu_driver: driver.map(Into::into),
            npu_compiler: false,
            npu_compiler_needs: vec![],
            npu_prefix: None,
        }
    }

    fn opts(driver: Option<&str>, full: bool) -> Options {
        Options {
            driver: driver.map(Into::into),
            full,
            allow_unverified: false,
            force: false,
        }
    }

    #[test]
    fn the_compiler_comes_from_the_host_drivers_release() {
        let data = Data::load(None).unwrap();
        let p = plan(&host(Some("1.32.0")), &data, &opts(None, false)).unwrap();
        assert_eq!(
            (p.version.as_str(), p.mode, p.trust),
            ("1.32.0", Mode::Compiler, Trust::Verified)
        );
        assert!(p.id.ends_with("ubuntu2404.tar.gz"), "{}", p.id);
        assert!(
            p.url.starts_with(
                "https://github.com/intel/linux-npu-driver/releases/download/v1.32.0/"
            )
        );
        assert!(plan(&host(Some("1.32.0")), &data, &opts(Some("1.38.0"), false)).is_err());
    }

    #[test]
    fn with_no_host_driver_the_newest_verified_release_is_installed() {
        let data = Data::load(None).unwrap();
        let p = plan(&host(None), &data, &opts(None, false)).unwrap();
        assert_eq!(p.mode, Mode::Full);
        let newest = data
            .npu_drivers
            .list
            .iter()
            .filter(|d| d.platforms.iter().any(|c| c == "Wildcat Lake"))
            .filter_map(|d| Ver::parse(&d.version))
            .max()
            .unwrap();
        assert_eq!(Ver::parse(&p.version), Some(newest));
    }

    #[test]
    fn a_compiler_found_beside_the_driver_needs_the_driver_too() {
        let dir = TempDir::new().unwrap();
        for f in [
            detect::COMPILER_BESIDE_DRIVER,
            "libopenvino_intel_npu_compiler.so",
        ] {
            fs::write(dir.0.join(f), b"").unwrap();
        }
        let needs = vec![
            vec![
                detect::COMPILER_BESIDE_DRIVER.to_string(),
                "libopenvino_intel_npu_compiler.so".to_string(),
            ],
            vec!["libnpu_driver_compiler.so".to_string()],
        ];
        assert_eq!(covered(&needs, &dir.0, false).unwrap(), None);
        assert_eq!(
            covered(&needs, &dir.0, true).unwrap().as_ref(),
            Some(&needs[0])
        );
        fs::write(dir.0.join("libnpu_driver_compiler.so"), b"").unwrap();
        assert_eq!(
            covered(&needs, &dir.0, false).unwrap().as_ref(),
            Some(&needs[1])
        );
    }

    fn npu_plan(mode: Mode, trust: Trust) -> Plan {
        Plan {
            version: "1.35.0".into(),
            mode,
            id: "github/intel/linux-npu-driver/v1.35.0/x.tar.gz".into(),
            url: "http://not-allowed.invalid/x.tar.gz".into(),
            sha256: "same".into(),
            trust,
        }
    }

    /// A prefix `npu install` left in `mode`, plus a file it did not write,
    /// so an install that gets past the short-circuit stops at
    /// `refuse_foreign` instead of reaching the network.
    fn npu_prefix(mode: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::write(dir.0.join("libnpu_driver_compiler.so"), b"c").unwrap();
        install::write_sums(&dir.0).unwrap();
        fs::write(
            dir.0.join("ovfetch.lock.json"),
            serde_json::json!({"mode": mode, "artifact": {"sha256": "same"}}).to_string(),
        )
        .unwrap();
        dir
    }

    fn foreign(dir: &TempDir) -> &TempDir {
        fs::write(dir.0.join("mine.txt"), b"x").unwrap();
        dir
    }

    #[test]
    fn an_unverified_driver_release_needs_the_flag() {
        let prefix = TempDir::new().unwrap();
        let p = npu_plan(Mode::Compiler, Trust::Unverified);
        let err = install(&p, &host(Some("1.35.0")), &prefix.0, &opts(None, false)).unwrap_err();
        assert!(err.to_string().contains("--allow-unverified"), "{err}");
    }

    #[test]
    fn a_full_install_covers_a_request_for_the_compiler_alone() {
        let prefix = npu_prefix("full");
        foreign(&prefix);
        let p = npu_plan(Mode::Compiler, Trust::Verified);
        // The foreign file breaks verify, so it is not "already installed".
        assert!(install(&p, &host(Some("1.35.0")), &prefix.0, &opts(None, false)).is_err());
        fs::remove_file(prefix.0.join("mine.txt")).unwrap();
        install(&p, &host(Some("1.35.0")), &prefix.0, &opts(None, false)).unwrap();
    }

    #[test]
    fn a_compiler_install_does_not_cover_a_full_request() {
        let prefix = npu_prefix("compiler");
        let h = host(Some("1.35.0"));
        install(
            &npu_plan(Mode::Compiler, Trust::Verified),
            &h,
            &prefix.0,
            &opts(None, false),
        )
        .unwrap();
        // Past the short-circuit, a full install stops at the Level Zero
        // loader check or the offline download; either way it is an error.
        let full = npu_plan(Mode::Full, Trust::Verified);
        assert!(install(&full, &h, &prefix.0, &opts(None, true)).is_err());
    }

    #[test]
    fn a_host_that_finds_its_compiler_needs_nothing() {
        let prefix = TempDir::new().unwrap();
        let mut h = host(Some("1.35.0"));
        h.npu_compiler = true;
        install(
            &npu_plan(Mode::Compiler, Trust::Verified),
            &h,
            &prefix.0.join("npu"),
            &opts(None, false),
        )
        .unwrap();
        assert!(!prefix.0.join("npu").exists());
    }
}
