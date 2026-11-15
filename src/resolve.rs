//! Turning "the right OpenVINO for this machine" into one concrete, hash-checked download.
//!
//! Prebuilt ONNX Runtime + OpenVINO wheels are preferred. A reviewed build
//! recipe (`data/builds.toml`) is chosen only with `--build`, and only when its
//! OpenVINO is newer than every prebuilt that fits the device. Callers that use
//! OpenVINO directly, without ONNX Runtime, take Intel's `openvino` wheel alone.

use crate::consensus::{self, Trust};
use crate::data::{Build, Data};
use crate::detect::Machine;
use crate::net;
use crate::sources::{self, PypiFile};
use crate::status::{Floor, FloorSource, compiler_warning, driver_ceiling, floor_source};
use crate::upstream::{Network, Upstream};
use crate::version::Ver;
use anyhow::{Context, Result, bail};
use serde::Serialize;

/// What gets installed: Intel's onnxruntime-openvino wheel, ONNX Runtime
/// compiled here from a recipe, or the openvino wheel alone. For a build,
/// `sha256` digests the recipe's inputs, since compiled output differs between
/// toolchains.
#[derive(Debug, Clone, Serialize)]
pub struct Artifact {
    pub id: String,
    /// `None` for an OpenVINO-only install.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub onnxruntime: Option<String>,
    pub url: String,
    pub sha256: String,
    pub trust: Trust,
    pub kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildPlan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Wheel,
    Build,
    /// Intel's `openvino` wheel, without ONNX Runtime.
    Openvino,
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildPlan {
    pub git_repo: String,
    pub git_tag: String,
    pub git_commit: String,
    pub openvino_wheel: Input,
    pub build_args: Vec<String>,
    pub cmake_defines: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Input {
    pub id: String,
    pub url: String,
    pub sha256: String,
    pub trust: Trust,
}

#[derive(Debug, Serialize)]
pub struct Npu {
    pub driver_installed: Option<String>,
    pub driver_required: Option<String>,
    pub compiler_present: bool,
}

#[derive(Debug, Serialize)]
pub struct Plan {
    pub floor: Floor,
    /// Newest OpenVINO the installed NPU driver pairs with, when there is an NPU.
    pub driver_ceiling: Option<String>,
    pub openvino: String,
    pub artifact: Artifact,
    pub npu: Npu,
    pub warnings: Vec<String>,
}

fn agreed(upstream: &dyn Upstream, file: &PypiFile, data: &Data) -> Result<(String, Trust)> {
    let claims = sources::pypi_claims(file, &upstream.mirror_pages(&file.project));
    consensus::agree(&file.ledger_id(), &claims, &data.ledger)
}

fn artifact(upstream: &dyn Upstream, file: &PypiFile, data: &Data) -> Result<Artifact> {
    let (sha256, trust) = agreed(upstream, file, data)?;
    Ok(Artifact {
        id: file.ledger_id(),
        onnxruntime: Some(file.version.clone()),
        url: file.url.clone(),
        sha256,
        trust,
        kind: Kind::Wheel,
        build: None,
    })
}

/// The newest recipe whose OpenVINO sits between the floor and the ceiling.
pub fn pick_build<'a>(builds: &'a [Build], need: &Ver, ceiling: Option<&Ver>) -> Option<&'a Build> {
    builds
        .iter()
        .filter_map(|b| Some((b, Ver::parse(&b.openvino)?)))
        .filter(|(_, ov)| ov.minor() >= *need && ceiling.is_none_or(|c| ov.minor() <= *c))
        .max_by(|(a, x), (b, y)| {
            x.cmp(y)
                .then(Ver::parse(&a.onnxruntime).cmp(&Ver::parse(&b.onnxruntime)))
        })
        .map(|(b, _)| b)
}

/// Digest of everything that decides what a recipe compiles. Paths and job
/// counts are left out: they change where it builds, not what.
pub fn recipe_digest(build: &Build, openvino_sha256: &str) -> String {
    let mut text = format!(
        "ovfetch-build-v1\nsource {} {}\nopenvino {openvino_sha256}\n",
        build.git_repo, build.git_commit
    );
    for a in &build.build_args {
        text.push_str(&format!("arg {a}\n"));
    }
    for d in &build.cmake_defines {
        text.push_str(&format!("define {d}\n"));
    }
    net::sha256_hex(text.as_bytes())
}

fn build_artifact(upstream: &dyn Upstream, build: &Build, data: &Data) -> Result<Artifact> {
    let (project, filename) = sources::pypi_id_parts(&build.openvino_wheel)
        .with_context(|| format!("recipe input {} is not a PyPI file", build.openvino_wheel))?;
    let wheel = upstream
        .pypi_wheels(project)?
        .into_iter()
        .find(|w| w.filename == filename)
        .with_context(|| format!("{} is no longer on PyPI", build.openvino_wheel))?;
    let (sha256, trust) = agreed(upstream, &wheel, data)?;
    match upstream.github_tag_commit(&build.git_repo, &build.git_tag) {
        Ok(c) if c == build.git_commit => {}
        Ok(c) => bail!(
            "{} tag {} now points at {c}, but the recipe pins {}. A release tag moved; nothing was built.",
            build.git_repo,
            build.git_tag,
            build.git_commit
        ),
        // The clone still has to land on the pinned commit.
        Err(e) => eprintln!(
            "note: could not ask GitHub where {} points: {e:#}",
            build.git_tag
        ),
    }
    Ok(Artifact {
        id: format!(
            "build/onnxruntime-{}+openvino-{}",
            build.onnxruntime, build.openvino
        ),
        onnxruntime: Some(build.onnxruntime.clone()),
        url: format!(
            "https://github.com/{}/tree/{}",
            build.git_repo, build.git_commit
        ),
        sha256: recipe_digest(build, &sha256),
        trust,
        kind: Kind::Build,
        build: Some(BuildPlan {
            git_repo: build.git_repo.clone(),
            git_tag: build.git_tag.clone(),
            git_commit: build.git_commit.clone(),
            openvino_wheel: Input {
                id: build.openvino_wheel.clone(),
                url: wheel.url,
                sha256,
                trust,
            },
            build_args: build.build_args.clone(),
            cmake_defines: build.cmake_defines.clone(),
        }),
    })
}

/// The newest `openvino` release between the floor and the ceiling. Within a
/// release, a wheel the ledger already records wins over a newer CPython tag:
/// the libraries ovfetch takes are not Python-specific, and a recorded wheel
/// installs as verified.
pub fn pick_openvino(
    wheels: &[PypiFile],
    data: &Data,
    need: &Ver,
    ceiling: Option<&Ver>,
) -> Option<(PypiFile, Ver)> {
    let mut versions: Vec<&str> = wheels.iter().map(|w| w.version.as_str()).collect();
    versions.dedup();
    for v in versions {
        let Some(ov) = Ver::parse(v) else {
            continue;
        };
        if ov.minor() < *need {
            break;
        }
        if ceiling.is_some_and(|c| ov.minor() > *c) {
            continue;
        }
        let recorded = wheels
            .iter()
            .find(|w| w.version == v && data.ledger.get(&w.ledger_id()).is_some());
        if let Some(w) = recorded.cloned().or_else(|| sources::pick_wheel(wheels, v)) {
            return Some((w, ov));
        }
    }
    None
}

/// What to install: ONNX Runtime with its OpenVINO EP, or OpenVINO alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    OnnxRuntime { allow_build: bool },
    OpenvinoOnly,
}

/// The OpenVINO range a machine can run, and why.
struct Bounds {
    floor: Floor,
    /// The floor as a version; zero when nothing constrains it.
    need: Ver,
    ceiling: Option<Ver>,
}

impl Bounds {
    /// ` and <= 2026.4 (NPU driver)`, or nothing without a ceiling.
    fn ceiling_clause(&self) -> String {
        self.ceiling
            .as_ref()
            .map(|c| format!(" and <= {c} (NPU driver)"))
            .unwrap_or_default()
    }

    fn allows(&self, openvino: &Ver) -> bool {
        self.ceiling.as_ref().is_none_or(|c| openvino.minor() <= *c)
    }
}

const NO_FLOOR: u64 = 0;

fn bounds(
    machine: &Machine,
    data: &Data,
    override_min: Option<&str>,
    ignore_driver: bool,
) -> Result<Bounds> {
    let source = floor_source(machine, data, override_min);
    let floor = Floor::from(source.clone());
    let ceiling = if ignore_driver {
        None
    } else {
        driver_ceiling(machine, data)
    };
    let need = match &source {
        FloorSource::Override(openvino) => {
            Ver::parse(openvino).with_context(|| format!("bad floor {:?}", floor.openvino))?
        }
        FloorSource::Platform { openvino, .. } => openvino.clone(),
        FloorSource::NoNpu => Ver(vec![NO_FLOOR]),
        FloorSource::UnknownNpu { .. } => bail!(
            "{}. This platform is newer than ovfetch's data, so it cannot pick a safe OpenVINO. \
             Pass --min-openvino, or wait for `ci discover` to record it.",
            floor.reason
        ),
    };
    if let Some(c) = &ceiling
        && *c < need
    {
        bail!(
            "this device needs OpenVINO {need}, but nothing newer than {c} is known to work with NPU driver {}. \
             Upgrade the driver (github.com/intel/linux-npu-driver/releases), or pass --ignore-driver",
            machine.npu_driver.as_deref().unwrap_or("?")
        );
    }
    Ok(Bounds {
        floor,
        need,
        ceiling,
    })
}

pub fn resolve(
    machine: &Machine,
    data: &Data,
    override_min: Option<&str>,
    ignore_driver: bool,
    want: Want,
) -> Result<Plan> {
    resolve_with(&Network, machine, data, override_min, ignore_driver, want)
}

/// [`resolve`] against any [`Upstream`].
pub fn resolve_with(
    upstream: &dyn Upstream,
    machine: &Machine,
    data: &Data,
    override_min: Option<&str>,
    ignore_driver: bool,
    want: Want,
) -> Result<Plan> {
    let bounds = bounds(machine, data, override_min, ignore_driver)?;
    let mut warnings = Vec::new();
    let (artifact, openvino) = match want {
        Want::OpenvinoOnly => openvino_alone(upstream, data, &bounds)?,
        Want::OnnxRuntime { allow_build } => {
            onnxruntime(upstream, machine, data, &bounds, allow_build, &mut warnings)?
        }
    };
    let npu = npu_requirements(machine, data, &openvino, &mut warnings);
    Ok(Plan {
        floor: bounds.floor,
        driver_ceiling: bounds.ceiling.map(|c| c.to_string()),
        openvino: openvino.to_string(),
        artifact,
        npu,
        warnings,
    })
}

fn openvino_alone(
    upstream: &dyn Upstream,
    data: &Data,
    bounds: &Bounds,
) -> Result<(Artifact, Ver)> {
    let wheels = upstream.pypi_wheels("openvino")?;
    let Some((wheel, openvino)) =
        pick_openvino(&wheels, data, &bounds.need, bounds.ceiling.as_ref())
    else {
        bail!(
            "no openvino wheel fits this device: it needs OpenVINO >= {}{}",
            bounds.need,
            bounds.ceiling_clause()
        );
    };
    let mut artifact = artifact(upstream, &wheel, data)?;
    artifact.onnxruntime = None;
    artifact.kind = Kind::Openvino;
    Ok((artifact, openvino))
}

/// What the onnxruntime-openvino wheels on PyPI offer inside the bounds.
struct Prebuilt {
    /// The newest wheel whose OpenVINO fits, and that OpenVINO.
    fitting: Option<(PypiFile, Ver)>,
    /// The OpenVINO the newest wheel bundles, whether or not it fits.
    newest_bundled: Option<Ver>,
}

/// Wheels bundle ever-newer OpenVINO, so this walks newest first and stops
/// once one falls below the floor.
fn prebuilt(upstream: &dyn Upstream, bounds: &Bounds) -> Result<Prebuilt> {
    let wheels = upstream.pypi_wheels("onnxruntime-openvino")?;
    let mut versions: Vec<&str> = wheels.iter().map(|w| w.version.as_str()).collect();
    versions.dedup();
    let mut found = Prebuilt {
        fitting: None,
        newest_bundled: None,
    };
    for version in versions {
        let Some(wheel) = sources::pick_wheel(&wheels, version) else {
            continue;
        };
        let Some(openvino) = upstream.bundled_openvino(&wheel)? else {
            continue;
        };
        found.newest_bundled.get_or_insert(openvino.clone());
        if openvino.minor() < bounds.need {
            break;
        }
        if bounds.allows(&openvino) {
            found.fitting = Some((wheel, openvino));
            break;
        }
    }
    Ok(found)
}

fn recipe_hint(recipe: &Build) -> String {
    format!(
        "OpenVINO {} is available by compiling ONNX Runtime {} here; pass --build",
        recipe.openvino, recipe.onnxruntime
    )
}

/// When the ceiling holds the pick below what the newest wheel bundles: the
/// driver release that would lift it.
fn held_back_warning(
    machine: &Machine,
    data: &Data,
    bounds: &Bounds,
    newest_bundled: Option<&Ver>,
) -> Option<String> {
    let ceiling = bounds.ceiling.as_ref()?;
    if !newest_bundled.is_some_and(|n| n.minor() > *ceiling) {
        return None;
    }
    let newer_driver = data
        .npu_drivers
        .list
        .iter()
        .filter(|d| d.max_openvino().is_some_and(|o| o.minor() > *ceiling))
        .max_by_key(|d| Ver::parse(&d.version))?;
    Some(format!(
        "held at OpenVINO {ceiling}, the newest known to work with NPU driver {}; driver {} is known to work with OpenVINO {}",
        machine.npu_driver.as_deref().unwrap_or("?"),
        newer_driver.version,
        newer_driver
            .max_openvino()
            .map(|v| v.to_string())
            .unwrap_or_default()
    ))
}

/// A prebuilt wheel when one fits; a recipe only when it offers a newer
/// OpenVINO than that wheel, and is only built with `allow_build`.
fn onnxruntime(
    upstream: &dyn Upstream,
    machine: &Machine,
    data: &Data,
    bounds: &Bounds,
    allow_build: bool,
    warnings: &mut Vec<String>,
) -> Result<(Artifact, Ver)> {
    let Prebuilt {
        fitting,
        newest_bundled,
    } = prebuilt(upstream, bounds)?;
    let recipe = pick_build(&data.builds.list, &bounds.need, bounds.ceiling.as_ref()).filter(|b| {
        let openvino = Ver::parse(&b.openvino);
        fitting
            .as_ref()
            .is_none_or(|(_, have)| openvino.as_ref() > Some(have))
    });

    if fitting.is_none() && !(allow_build && recipe.is_some()) {
        let newest = newest_bundled
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".into());
        bail!(
            "no prebuilt ONNX Runtime + OpenVINO fits this device: it needs OpenVINO >= {}{}, \
             and the newest prebuilt build bundles {newest}. {}",
            bounds.need,
            bounds.ceiling_clause(),
            match recipe {
                Some(b) => format!("{}.", recipe_hint(b)),
                None => "No build recipe fits either; this resolves once Intel publishes a newer \
                         onnxruntime-openvino wheel or a recipe is added to data/builds.toml."
                    .into(),
            }
        );
    }

    warnings.extend(held_back_warning(
        machine,
        data,
        bounds,
        newest_bundled.as_ref(),
    ));

    match (recipe, fitting) {
        (Some(b), _) if allow_build => Ok((
            build_artifact(upstream, b, data)?,
            Ver::parse(&b.openvino).context("bad recipe openvino")?,
        )),
        (unused_recipe, Some((wheel, openvino))) => {
            warnings.extend(unused_recipe.map(recipe_hint));
            Ok((artifact(upstream, &wheel, data)?, openvino))
        }
        (_, None) => unreachable!("bailed above"),
    }
}

/// The oldest driver known to work with `ov`, raised to the platform's
/// first verified driver.
fn npu_requirements(machine: &Machine, data: &Data, ov: &Ver, warnings: &mut Vec<String>) -> Npu {
    let npus: Vec<_> = machine.npus().collect();
    let mut required: Option<Ver> = data
        .npu_drivers
        .list
        .iter()
        .filter(|d| d.max_openvino().is_some_and(|o| o.minor() >= ov.minor()))
        .filter_map(|d| Ver::parse(&d.version))
        .min();
    if required.is_none() && !npus.is_empty() {
        required = data
            .npu_drivers
            .list
            .iter()
            .filter_map(|d| Ver::parse(&d.version))
            .max();
        warnings.push(format!(
            "no NPU driver release is known to work with OpenVINO {ov} yet; the NPU may fail to compile models"
        ));
    }
    for p in data
        .platforms
        .list
        .iter()
        .filter(|p| npus.iter().any(|n| n.pci_id == p.pci_id))
    {
        if let Some(min) = p.min_npu_driver.as_deref().and_then(Ver::parse) {
            required = required.max(Some(min));
        }
    }
    if npus.is_empty() {
        required = None;
    }
    let installed = machine.npu_driver.clone();
    if let Some(req) = &required {
        if installed
            .as_deref()
            .and_then(Ver::parse)
            .is_none_or(|i| i < *req)
        {
            warnings.push(format!(
                "NPU driver {} is older than {req}, the first known to work with OpenVINO {ov} on this platform (github.com/intel/linux-npu-driver/releases)",
                installed.as_deref().unwrap_or("(none)")
            ));
        }
        if let Some(w) = compiler_warning(machine) {
            warnings.push(w);
        }
    }
    Npu {
        driver_installed: installed,
        driver_required: required.map(|v| v.to_string()),
        compiler_present: machine.npu_compiler,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe(ort: &str, ov: &str) -> Build {
        Build {
            onnxruntime: ort.into(),
            openvino: ov.into(),
            git_repo: "microsoft/onnxruntime".into(),
            git_tag: format!("v{ort}"),
            git_commit: "c0ffee".into(),
            openvino_wheel: "pypi/openvino/x.whl".into(),
            build_args: vec!["--use_openvino".into()],
            cmake_defines: vec!["CMAKE_INSTALL_RPATH=$ORIGIN".into()],
            note: None,
        }
    }

    fn v(s: &str) -> Ver {
        Ver::parse(s).unwrap()
    }

    #[test]
    fn picks_the_newest_recipe_inside_the_bounds() {
        let builds = [
            recipe("1.24.1", "2026.4.1"),
            recipe("1.24.1", "2026.2.0"),
            recipe("1.25.0", "2026.6.0"),
        ];
        let got = pick_build(&builds, &v("2025.4"), Some(&v("2026.4"))).unwrap();
        assert_eq!(got.openvino, "2026.4.1");
        assert_eq!(
            pick_build(&builds, &v("2025.4"), None).unwrap().openvino,
            "2026.6.0"
        );
        assert!(pick_build(&builds, &v("2026.7"), None).is_none());
    }

    fn ov_wheel(version: &str, build: &str, cp: &str) -> PypiFile {
        PypiFile {
            project: "openvino".into(),
            version: version.into(),
            filename: format!("openvino-{version}-{build}-{cp}-{cp}-manylinux_2_28_x86_64.whl"),
            url: String::new(),
            size: 0,
            sha256: String::new(),
            uploaded: String::new(),
        }
    }

    #[test]
    fn openvino_only_takes_the_newest_release_inside_the_bounds() {
        // Synthetic data, so a discover PR that records more wheels can't
        // change what this expects.
        let data = Data {
            dir: None,
            platforms: Default::default(),
            npu_drivers: Default::default(),
            ledger: crate::data::Ledger {
                list: vec![crate::data::Entry {
                    id: ov_wheel("2026.4.1", "22982", "cp313").ledger_id(),
                    digest: "ab".repeat(32),
                    first_seen: "2026-10-01".into(),
                    provenance: false,
                }],
            },
            builds: Default::default(),
        };
        // Newest first, as pypi_wheels returns them.
        let wheels = [
            ov_wheel("2026.5.0", "23000", "cp314"),
            ov_wheel("2026.4.1", "22982", "cp314"),
            ov_wheel("2026.4.1", "22982", "cp313"),
            ov_wheel("2026.3.0", "22000", "cp314"),
            ov_wheel("2025.3.0", "20000", "cp314"),
        ];
        let (w, ov) = pick_openvino(&wheels, &data, &v("2025.4"), Some(&v("2026.4"))).unwrap();
        assert_eq!(ov, v("2026.4.1"));
        // cp313 is the one the ledger records, so it beats the newer cp314.
        assert!(w.filename.contains("cp313"), "{}", w.filename);
        let (w, _) = pick_openvino(&wheels, &data, &v("2025.4"), None).unwrap();
        assert_eq!(w.version, "2026.5.0");
        let (_, ov) = pick_openvino(&wheels, &data, &v("2025.4"), Some(&v("2026.3"))).unwrap();
        assert_eq!(ov, v("2026.3.0"));
        assert!(pick_openvino(&wheels, &data, &v("2026.6"), None).is_none());
        assert!(pick_openvino(&wheels[4..], &data, &v("2025.4"), None).is_none());
    }

    #[test]
    fn same_openvino_prefers_the_newer_onnxruntime() {
        let builds = [recipe("1.24.1", "2026.4.1"), recipe("1.25.0", "2026.4.1")];
        assert_eq!(
            pick_build(&builds, &v("2025.4"), None).unwrap().onnxruntime,
            "1.25.0"
        );
    }

    #[test]
    fn the_recipe_digest_covers_every_input() {
        let base = recipe("1.24.1", "2026.4.1");
        let d = recipe_digest(&base, "aa");
        assert_eq!(d, recipe_digest(&base.clone(), "aa"));
        assert_ne!(d, recipe_digest(&base, "bb"));
        let mut other = base.clone();
        other.git_commit = "decaf".into();
        assert_ne!(d, recipe_digest(&other, "aa"));
        let mut other = base.clone();
        other.cmake_defines.push("X=1".into());
        assert_ne!(d, recipe_digest(&other, "aa"));
        let mut other = base;
        other.build_args.push("--x".into());
        assert_ne!(d, recipe_digest(&other, "aa"));
    }

    use crate::data::{Entry, Ledger, NpuDriver, NpuDrivers, Platform, Platforms};
    use crate::detect::Device;
    use crate::upstream::fake::{Fake, wheel};

    const ORT: &str = "onnxruntime-openvino";

    /// A Wildcat Lake NPU on `driver`, with its compiler in place.
    fn npu_machine(driver: &str) -> Machine {
        Machine {
            devices: vec![Device {
                kind: "npu",
                pci_id: "0xfd3e".into(),
                slot: String::new(),
            }],
            npu_driver: Some(driver.into()),
            npu_compiler: true,
            npu_compiler_needs: vec![],
            npu_prefix: None,
        }
    }

    fn no_npu() -> Machine {
        Machine {
            devices: vec![],
            npu_driver: None,
            npu_compiler: false,
            npu_compiler_needs: vec![],
            npu_prefix: None,
        }
    }

    /// Wildcat Lake floors at 2025.2; driver 1.32.0 pairs with 2025.4 and
    /// 1.35.0 with 2026.4. `recorded` wheels are in the ledger.
    fn data(recorded: &[&PypiFile], builds: Vec<Build>) -> Data {
        let driver = |version: &str, openvino: &str| NpuDriver {
            version: version.into(),
            openvino: openvino.into(),
            platforms: vec!["Wildcat Lake".into()],
            measured_openvino: None,
            measured_note: None,
        };
        Data {
            dir: None,
            platforms: Platforms {
                list: vec![Platform {
                    pci_id: "0xfd3e".into(),
                    kernel_name: "WCL".into(),
                    codename: "Wildcat Lake".into(),
                    min_openvino: Some("2025.2".into()),
                    min_npu_driver: Some("1.32.0".into()),
                    pinned_note: None,
                }],
            },
            npu_drivers: NpuDrivers {
                list: vec![driver("1.32.0", "2025.4"), driver("1.35.0", "2026.4")],
            },
            ledger: Ledger {
                list: recorded
                    .iter()
                    .map(|w| Entry {
                        id: w.ledger_id(),
                        digest: w.sha256.clone(),
                        first_seen: "2026-01-08".into(),
                        provenance: false,
                    })
                    .collect(),
            },
            builds: crate::data::Builds { list: builds },
        }
    }

    /// Three onnxruntime-openvino releases, newest first as PyPI lists them.
    fn three_releases() -> (Fake, [PypiFile; 3]) {
        let wheels = [
            wheel(ORT, "1.25.0", "cp313", "c3"),
            wheel(ORT, "1.24.1", "cp313", "c2"),
            wheel(ORT, "1.22.0", "cp313", "c1"),
        ];
        let fake = Fake::default()
            .bundling(wheels[0].clone(), "2026.5.0")
            .bundling(wheels[1].clone(), "2025.4.1")
            .bundling(wheels[2].clone(), "2025.1.0");
        (fake, wheels)
    }

    const PREBUILT: Want = Want::OnnxRuntime { allow_build: false };

    #[test]
    fn the_newest_prebuilt_under_the_ceiling_is_chosen_and_the_hold_explained() {
        let (fake, wheels) = three_releases();
        let d = data(&[&wheels[1]], vec![]);
        let plan = resolve_with(&fake, &npu_machine("1.32.0"), &d, None, false, PREBUILT).unwrap();
        assert_eq!(plan.openvino, "2025.4.1");
        assert_eq!(plan.driver_ceiling.as_deref(), Some("2025.4"));
        assert_eq!(plan.floor.openvino.as_deref(), Some("2025.2"));
        assert_eq!(plan.artifact.id, wheels[1].ledger_id());
        assert_eq!(plan.artifact.kind, Kind::Wheel);
        assert_eq!(plan.artifact.trust, Trust::Verified);
        assert_eq!(plan.artifact.sha256, "c2".repeat(32));
        assert_eq!(plan.artifact.onnxruntime.as_deref(), Some("1.24.1"));
        assert_eq!(plan.npu.driver_required.as_deref(), Some("1.32.0"));
        assert_eq!(
            plan.warnings,
            [
                "held at OpenVINO 2025.4, the newest known to work with NPU driver 1.32.0; \
              driver 1.35.0 is known to work with OpenVINO 2026.4"
            ]
        );
    }

    #[test]
    fn ignoring_the_driver_lifts_the_ceiling() {
        let (fake, wheels) = three_releases();
        let d = data(&[], vec![]);
        let plan = resolve_with(&fake, &npu_machine("1.32.0"), &d, None, true, PREBUILT).unwrap();
        assert_eq!(plan.openvino, "2026.5.0");
        assert_eq!(plan.driver_ceiling, None);
        assert_eq!(plan.artifact.id, wheels[0].ledger_id());
        assert_eq!(plan.artifact.trust, Trust::Unverified);
    }

    #[test]
    fn without_an_npu_the_newest_build_is_chosen_with_no_bounds() {
        let (fake, _) = three_releases();
        let plan =
            resolve_with(&fake, &no_npu(), &data(&[], vec![]), None, false, PREBUILT).unwrap();
        assert_eq!(plan.openvino, "2026.5.0");
        assert_eq!(plan.floor.openvino, None);
        assert_eq!(plan.npu.driver_required, None);
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    #[test]
    fn a_floor_above_every_prebuilt_stops_and_names_the_newest() {
        let (fake, _) = three_releases();
        let d = data(&[], vec![]);
        let err = resolve_with(
            &fake,
            &npu_machine("1.35.0"),
            &d,
            Some("2026.6"),
            true,
            PREBUILT,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("needs OpenVINO >= 2026.6"), "{err}");
        assert!(err.contains("bundles 2026.5.0"), "{err}");
        assert!(err.contains("No build recipe fits either"), "{err}");
    }

    #[test]
    fn a_floor_above_the_ceiling_stops_before_asking_upstream() {
        let fake = Fake::default();
        let d = data(&[], vec![]);
        let err = resolve_with(
            &fake,
            &npu_machine("1.32.0"),
            &d,
            Some("2026.2"),
            false,
            PREBUILT,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("needs OpenVINO 2026.2"), "{err}");
        assert!(err.contains("nothing newer than 2025.4"), "{err}");
        assert!(err.contains("--ignore-driver"), "{err}");
    }

    #[test]
    fn an_npu_the_data_does_not_know_is_refused_unless_a_floor_is_given() {
        let (fake, _) = three_releases();
        let mut machine = npu_machine("1.35.0");
        machine.devices[0].pci_id = "0xbeef".into();
        let d = data(&[], vec![]);
        let err = resolve_with(&fake, &machine, &d, None, false, PREBUILT)
            .unwrap_err()
            .to_string();
        assert!(err.contains("NPU 0xbeef has no known floor yet"), "{err}");
        assert!(err.contains("newer than ovfetch's data"), "{err}");

        let plan = resolve_with(&fake, &machine, &d, Some("2025.4"), false, PREBUILT).unwrap();
        assert_eq!(plan.openvino, "2025.4.1");
        assert_eq!(plan.floor.reason, "--min-openvino");
    }

    #[test]
    fn a_floor_that_is_not_a_version_is_an_error() {
        let (fake, _) = three_releases();
        let err = resolve_with(
            &fake,
            &no_npu(),
            &data(&[], vec![]),
            Some("new"),
            false,
            PREBUILT,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(err, r#"bad floor Some("new")"#);
    }

    #[test]
    fn a_mirror_that_disagrees_stops_the_plan() {
        let (mut fake, wheels) = three_releases();
        fake.mirror_disagrees
            .insert(wheels[0].filename.clone(), "ee".repeat(32));
        let err = resolve_with(&fake, &no_npu(), &data(&[], vec![]), None, false, PREBUILT)
            .unwrap_err()
            .to_string();
        assert!(err.contains("SOURCES DISAGREE"), "{err}");
    }

    fn recipe_for(openvino_wheel: &PypiFile) -> Build {
        Build {
            openvino: "2026.4.1".into(),
            openvino_wheel: openvino_wheel.ledger_id(),
            ..recipe("1.24.1", "2026.4.1")
        }
    }

    #[test]
    fn a_newer_recipe_is_only_hinted_at_without_build() {
        let (mut fake, wheels) = three_releases();
        let ov = wheel("openvino", "2026.4.1", "cp313", "0a");
        fake.wheels.push(ov.clone());
        let d = data(&[&wheels[1], &ov], vec![recipe_for(&ov)]);
        let plan = resolve_with(&fake, &npu_machine("1.35.0"), &d, None, false, PREBUILT).unwrap();
        assert_eq!(plan.openvino, "2025.4.1");
        assert_eq!(plan.artifact.kind, Kind::Wheel);
        assert_eq!(
            plan.warnings,
            ["OpenVINO 2026.4.1 is available by compiling ONNX Runtime 1.24.1 here; pass --build"]
        );
    }

    #[test]
    fn with_build_the_recipe_is_planned_from_its_pinned_inputs() {
        let (mut fake, wheels) = three_releases();
        let ov = wheel("openvino", "2026.4.1", "cp313", "0a");
        fake.wheels.push(ov.clone());
        fake.tags
            .insert("microsoft/onnxruntime v1.24.1".into(), "c0ffee".into());
        let build = recipe_for(&ov);
        let d = data(&[&wheels[1], &ov], vec![build.clone()]);
        let want = Want::OnnxRuntime { allow_build: true };
        let plan = resolve_with(&fake, &npu_machine("1.35.0"), &d, None, false, want).unwrap();
        assert_eq!(plan.openvino, "2026.4.1");
        assert_eq!(plan.artifact.kind, Kind::Build);
        assert_eq!(
            plan.artifact.id,
            "build/onnxruntime-1.24.1+openvino-2026.4.1"
        );
        assert_eq!(plan.artifact.trust, Trust::Verified);
        assert_eq!(
            plan.artifact.sha256,
            recipe_digest(&build, &"0a".repeat(32))
        );
        let input = &plan.artifact.build.as_ref().unwrap().openvino_wheel;
        assert_eq!(
            (input.id.as_str(), input.url.as_str()),
            (ov.ledger_id().as_str(), ov.url.as_str())
        );
    }

    #[test]
    fn a_moved_release_tag_stops_a_build_plan() {
        let (mut fake, _) = three_releases();
        let ov = wheel("openvino", "2026.4.1", "cp313", "0a");
        fake.wheels.push(ov.clone());
        fake.tags
            .insert("microsoft/onnxruntime v1.24.1".into(), "deadbeef".into());
        let d = data(&[&ov], vec![recipe_for(&ov)]);
        let want = Want::OnnxRuntime { allow_build: true };
        let err = resolve_with(&fake, &npu_machine("1.35.0"), &d, None, false, want)
            .unwrap_err()
            .to_string();
        assert!(err.contains("A release tag moved"), "{err}");
    }

    #[test]
    fn a_recipe_input_that_left_pypi_stops_a_build_plan() {
        let (fake, _) = three_releases();
        let ov = wheel("openvino", "2026.4.1", "cp313", "0a");
        let d = data(&[&ov], vec![recipe_for(&ov)]);
        let want = Want::OnnxRuntime { allow_build: true };
        let err = resolve_with(&fake, &npu_machine("1.35.0"), &d, None, false, want)
            .unwrap_err()
            .to_string();
        assert!(err.contains("is no longer on PyPI"), "{err}");
    }

    #[test]
    fn openvino_alone_is_planned_without_onnxruntime() {
        let ov = [
            wheel("openvino", "2026.5.0", "cp313", "0b"),
            wheel("openvino", "2026.4.1", "cp313", "0a"),
        ];
        let fake = Fake::with_wheels(ov.to_vec());
        let d = data(&[&ov[1]], vec![]);
        let plan = resolve_with(
            &fake,
            &npu_machine("1.35.0"),
            &d,
            None,
            false,
            Want::OpenvinoOnly,
        )
        .unwrap();
        assert_eq!(plan.openvino, "2026.4.1");
        assert_eq!(plan.artifact.kind, Kind::Openvino);
        assert_eq!(plan.artifact.onnxruntime, None);
        assert_eq!(plan.artifact.trust, Trust::Verified);
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);

        let err = resolve_with(
            &fake,
            &npu_machine("1.32.0"),
            &d,
            None,
            false,
            Want::OpenvinoOnly,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            err,
            "no openvino wheel fits this device: it needs OpenVINO >= 2025.2 and <= 2025.4 (NPU driver)"
        );
    }

    #[test]
    fn a_driver_older_than_the_pick_needs_is_a_warning_not_a_refusal() {
        let (fake, _) = three_releases();
        let mut d = data(&[], vec![]);
        d.npu_drivers.list[0].openvino = "2025.2".into();
        let plan = resolve_with(&fake, &npu_machine("1.32.0"), &d, None, true, PREBUILT).unwrap();
        assert_eq!(plan.openvino, "2026.5.0");
        assert_eq!(plan.npu.driver_installed.as_deref(), Some("1.32.0"));
        assert!(
            plan.warnings.iter().any(
                |w| w.contains("no NPU driver release is known to work with OpenVINO 2026.5.0")
            ),
            "{:?}",
            plan.warnings
        );
    }
}
