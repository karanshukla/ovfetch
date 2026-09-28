//! Turning "the right OpenVINO for this machine" into one concrete, hash-checked download.
//!
//! Only prebuilt ONNX Runtime + OpenVINO builds are ever installed. When none
//! fits the device, resolution fails and says why; ovfetch never compiles.

use crate::consensus::{self, Trust};
use crate::data::Data;
use crate::detect::Machine;
use crate::sources::{self, PypiFile};
use crate::version::Ver;
use anyhow::{Result, bail};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Floor {
    /// `None` when nothing constrains it (no NPU).
    pub openvino: Option<String>,
    pub reason: String,
}

/// Intel's onnxruntime-openvino wheel: ONNX Runtime and the OpenVINO EP,
/// built together against the OpenVINO it bundles.
#[derive(Debug, Clone, Serialize)]
pub struct Artifact {
    pub id: String,
    pub onnxruntime: String,
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

pub fn floor(machine: &Machine, data: &Data, override_min: Option<&str>) -> Floor {
    if let Some(v) = override_min {
        return Floor {
            openvino: Some(v.to_owned()),
            reason: "--min-openvino".into(),
        };
    }
    let mut best: Option<(Ver, String)> = None;
    for npu in machine.devices.iter().filter(|d| d.kind == "npu") {
        let platform = data.platforms.list.iter().find(|p| p.pci_id == npu.pci_id);
        match platform.and_then(|p| Some((p, Ver::parse(p.min_openvino.as_deref()?)?))) {
            Some((p, v)) if best.as_ref().is_none_or(|(b, _)| v > *b) => {
                best = Some((v, format!("{} NPU ({})", p.codename, p.pci_id)));
            }
            Some(_) => {}
            None => {
                return Floor {
                    openvino: None,
                    reason: format!("NPU {} has no known floor yet", npu.pci_id),
                };
            }
        }
    }
    match best {
        Some((v, reason)) => Floor {
            openvino: Some(v.to_string()),
            reason,
        },
        None => Floor {
            openvino: None,
            reason: "no Intel NPU; any current OpenVINO runs the CPU and GPU".into(),
        },
    }
}

/// Newest OpenVINO the installed NPU driver is paired with. The NPU plugin
/// speaks the driver's protocol, so an OpenVINO newer than this fails to
/// compile models, while an older one keeps working.
pub fn driver_ceiling(machine: &Machine, data: &Data) -> Option<Ver> {
    if !machine.devices.iter().any(|d| d.kind == "npu") {
        return None;
    }
    let installed = Ver::parse(machine.npu_driver.as_deref()?)?;
    data.npu_drivers
        .list
        .iter()
        .filter(|d| Ver::parse(&d.version).is_some_and(|v| v <= installed))
        .max_by_key(|d| Ver::parse(&d.version))
        .and_then(|d| Ver::parse(&d.openvino))
        .map(|v| v.minor())
}

fn artifact(file: &PypiFile, data: &Data) -> Result<Artifact> {
    let claims = sources::pypi_claims(file, &sources::mirror_pages(&file.project));
    let (sha256, trust) = consensus::agree(&file.ledger_id(), &claims, &data.ledger)?;
    Ok(Artifact {
        id: file.ledger_id(),
        onnxruntime: file.version.clone(),
        url: file.url.clone(),
        sha256,
        trust,
    })
}

pub fn resolve(
    machine: &Machine,
    data: &Data,
    override_min: Option<&str>,
    ignore_driver: bool,
) -> Result<Plan> {
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
            "this device needs OpenVINO {need}, but NPU driver {} only pairs with {c}. \
             Upgrade the driver (github.com/intel/linux-npu-driver/releases), or pass --ignore-driver",
            machine.npu_driver.as_deref().unwrap_or("?")
        );
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
    let Some((wheel, openvino)) = chosen else {
        let newest = newest_bundled
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".into());
        bail!(
            "no prebuilt ONNX Runtime + OpenVINO fits this device: it needs OpenVINO >= {need}{}, \
             and the newest prebuilt build bundles {newest}. ovfetch does not compile ONNX Runtime; \
             this resolves once Intel publishes a newer onnxruntime-openvino wheel.",
            ceiling
                .as_ref()
                .map(|c| format!(" and <= {c} (NPU driver)"))
                .unwrap_or_default()
        );
    };

    if let Some(c) = &ceiling
        && newest_bundled.as_ref().is_some_and(|n| n.minor() > *c)
        && let Some(d) = data
            .npu_drivers
            .list
            .iter()
            .filter(|d| Ver::parse(&d.openvino).is_some_and(|o| o.minor() > *c))
            .max_by_key(|d| Ver::parse(&d.version))
    {
        warnings.push(format!(
            "held at OpenVINO {c} by NPU driver {}; driver {} unlocks OpenVINO {}",
            machine.npu_driver.as_deref().unwrap_or("?"),
            d.version,
            d.openvino
        ));
    }

    let artifact = artifact(&wheel, data)?;
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

/// The NPU plugin speaks a protocol the user-mode driver must match: each
/// driver release names the OpenVINO it pairs with.
fn npu_requirements(machine: &Machine, data: &Data, ov: &Ver, warnings: &mut Vec<String>) -> Npu {
    let npus: Vec<_> = machine.devices.iter().filter(|d| d.kind == "npu").collect();
    let mut required: Option<Ver> = data
        .npu_drivers
        .list
        .iter()
        .filter(|d| Ver::parse(&d.openvino).is_some_and(|o| o.minor() >= ov.minor()))
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
            "no NPU driver release pairs with OpenVINO {ov} yet; the NPU may fail to compile models"
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
                "NPU driver {} is older than {req}, which OpenVINO {ov} needs (github.com/intel/linux-npu-driver/releases)",
                installed.as_deref().unwrap_or("(none)")
            ));
        }
        if !machine.npu_compiler {
            warnings.push("libopenvino_intel_npu_compiler.so is not on the linker path; the NPU cannot compile models without it".into());
        }
    }
    Npu {
        driver_installed: installed,
        driver_required: required.map(|v| v.to_string()),
        compiler_present: machine.npu_compiler,
    }
}
