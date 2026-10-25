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
use crate::status::{Floor, compiler_warning, driver_ceiling, floor};
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

fn agreed(file: &PypiFile, data: &Data) -> Result<(String, Trust)> {
    let claims = sources::pypi_claims(file, &sources::mirror_pages(&file.project));
    consensus::agree(&file.ledger_id(), &claims, &data.ledger)
}

fn artifact(file: &PypiFile, data: &Data) -> Result<Artifact> {
    let (sha256, trust) = agreed(file, data)?;
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

fn build_artifact(build: &Build, data: &Data) -> Result<Artifact> {
    let (project, filename) = build
        .openvino_wheel
        .strip_prefix("pypi/")
        .and_then(|r| r.split_once('/'))
        .with_context(|| format!("recipe input {} is not a PyPI file", build.openvino_wheel))?;
    let wheel = sources::pypi_wheels(project)?
        .into_iter()
        .find(|w| w.filename == filename)
        .with_context(|| format!("{} is no longer on PyPI", build.openvino_wheel))?;
    let (sha256, trust) = agreed(&wheel, data)?;
    match sources::github_tag_commit(&build.git_repo, &build.git_tag) {
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

pub fn resolve(
    machine: &Machine,
    data: &Data,
    override_min: Option<&str>,
    ignore_driver: bool,
    want: Want,
) -> Result<Plan> {
    let allow_build = match want {
        Want::OnnxRuntime { allow_build } => allow_build,
        Want::OpenvinoOnly => false,
    };
    let floor = floor(machine, data, override_min);
    let mut warnings = Vec::new();
    let ceiling = if ignore_driver {
        None
    } else {
        driver_ceiling(machine, data)
    };
    let Some(need) = floor
        .openvino
        .as_deref()
        .map(Ver::parse)
        .unwrap_or(Some(Ver(vec![0])))
    else {
        bail!("bad floor {:?}", floor.openvino);
    };
    if floor.reason.contains("no known floor") {
        bail!(
            "{}. This platform is newer than ovfetch's data, so it cannot pick a safe OpenVINO. \
             Pass --min-openvino, or wait for `ci discover` to record it.",
            floor.reason
        );
    }
    if let Some(c) = &ceiling
        && *c < need
    {
        bail!(
            "this device needs OpenVINO {need}, but nothing newer than {c} is known to work with NPU driver {}. \
             Upgrade the driver (github.com/intel/linux-npu-driver/releases), or pass --ignore-driver",
            machine.npu_driver.as_deref().unwrap_or("?")
        );
    }

    if want == Want::OpenvinoOnly {
        let wheels = sources::pypi_wheels("openvino")?;
        let Some((wheel, ov)) = pick_openvino(&wheels, data, &need, ceiling.as_ref()) else {
            bail!(
                "no openvino wheel fits this device: it needs OpenVINO >= {need}{}",
                ceiling
                    .as_ref()
                    .map(|c| format!(" and <= {c} (NPU driver)"))
                    .unwrap_or_default()
            );
        };
        let mut artifact = artifact(&wheel, data)?;
        artifact.onnxruntime = None;
        artifact.kind = Kind::Openvino;
        let npu = npu_requirements(machine, data, &ov, &mut warnings);
        return Ok(Plan {
            floor,
            driver_ceiling: ceiling.map(|c| c.to_string()),
            openvino: ov.to_string(),
            artifact,
            npu,
            warnings,
        });
    }

    // Wheels bundle ever-newer OpenVINO, so walk newest first and stop once
    // one falls below the floor.
    let wheels = sources::pypi_wheels("onnxruntime-openvino")?;
    let mut versions: Vec<&str> = wheels.iter().map(|w| w.version.as_str()).collect();
    versions.dedup();
    let mut newest_bundled = None;
    let mut chosen = None;
    for v in versions {
        let Some(w) = sources::pick_wheel(&wheels, v) else {
            continue;
        };
        let Some(ov) = sources::bundled_openvino(&w)? else {
            continue;
        };
        newest_bundled.get_or_insert(ov.clone());
        if ov.minor() < need {
            break;
        }
        if ceiling.as_ref().is_none_or(|c| ov.minor() <= *c) {
            chosen = Some((w, ov));
            break;
        }
    }
    let recipe = pick_build(&data.builds.list, &need, ceiling.as_ref()).filter(|b| {
        let ov = Ver::parse(&b.openvino);
        chosen
            .as_ref()
            .is_none_or(|(_, have)| ov.as_ref() > Some(have))
    });
    let recipe_hint = |b: &Build| {
        format!(
            "OpenVINO {} is available by compiling ONNX Runtime {} here; pass --build",
            b.openvino, b.onnxruntime
        )
    };

    if chosen.is_none() && !(allow_build && recipe.is_some()) {
        let newest = newest_bundled
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".into());
        bail!(
            "no prebuilt ONNX Runtime + OpenVINO fits this device: it needs OpenVINO >= {need}{}, \
             and the newest prebuilt build bundles {newest}. {}",
            ceiling
                .as_ref()
                .map(|c| format!(" and <= {c} (NPU driver)"))
                .unwrap_or_default(),
            match recipe {
                Some(b) => format!("{}.", recipe_hint(b)),
                None => "No build recipe fits either; this resolves once Intel publishes a newer \
                         onnxruntime-openvino wheel or a recipe is added to data/builds.toml."
                    .into(),
            }
        );
    }

    if let Some(c) = &ceiling
        && newest_bundled.as_ref().is_some_and(|n| n.minor() > *c)
        && let Some(d) = data
            .npu_drivers
            .list
            .iter()
            .filter(|d| d.max_openvino().is_some_and(|o| o.minor() > *c))
            .max_by_key(|d| Ver::parse(&d.version))
    {
        warnings.push(format!(
            "held at OpenVINO {c}, the newest known to work with NPU driver {}; driver {} is known to work with OpenVINO {}",
            machine.npu_driver.as_deref().unwrap_or("?"),
            d.version,
            d.max_openvino().map(|v| v.to_string()).unwrap_or_default()
        ));
    }

    let (artifact, openvino) = match (recipe, chosen) {
        (Some(b), _) if allow_build => (
            build_artifact(b, data)?,
            Ver::parse(&b.openvino).context("bad recipe openvino")?,
        ),
        (r, Some((wheel, ov))) => {
            if let Some(b) = r {
                warnings.push(recipe_hint(b));
            }
            (artifact(&wheel, data)?, ov)
        }
        (_, None) => unreachable!("bailed above"),
    };
    let npu = npu_requirements(machine, data, &openvino, &mut warnings);
    Ok(Plan {
        floor,
        driver_ceiling: ceiling.map(|c| c.to_string()),
        openvino: openvino.to_string(),
        artifact,
        npu,
        warnings,
    })
}

/// The oldest driver known to work with `ov`, raised to the platform's
/// first verified driver.
fn npu_requirements(machine: &Machine, data: &Data, ov: &Ver, warnings: &mut Vec<String>) -> Npu {
    let npus: Vec<_> = machine.devices.iter().filter(|d| d.kind == "npu").collect();
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
}
