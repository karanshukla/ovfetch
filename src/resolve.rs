//! Turning "the right OpenVINO for this machine" into one concrete, hash-checked download.
//!
//! Only prebuilt ONNX Runtime + OpenVINO builds are ever installed. When none
//! fits the device, resolution fails and says why; ovfetch never compiles.

use crate::consensus::{self, Trust};
use crate::data::{Data, Platform};
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

/// Newest OpenVINO known to work on the installed NPU driver: the newest
/// recorded release at or below it, at Intel's pairing or a newer measured
/// version. Deliberately pessimistic: beyond it is untested, not known to fail.
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
        .and_then(|d| d.max_openvino())
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
            "this device needs OpenVINO {need}, but nothing newer than {c} is known to work with NPU driver {}. \
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

/// What `detect` adds to the machine: the platform's data and what it implies,
/// with no network access.
#[derive(Debug, Serialize)]
pub struct Status<'a> {
    #[serde(flatten)]
    pub machine: &'a Machine,
    pub platforms: Vec<&'a Platform>,
    /// Newest OpenVINO known to work on the installed NPU driver.
    pub openvino_ceiling: Option<String>,
    /// Intel's first driver release verified on the platform.
    pub npu_driver_required: Option<String>,
    pub warnings: Vec<String>,
}

pub fn status<'a>(machine: &'a Machine, data: &'a Data) -> Status<'a> {
    let npus: Vec<_> = machine.devices.iter().filter(|d| d.kind == "npu").collect();
    let platforms: Vec<&Platform> = data
        .platforms
        .list
        .iter()
        .filter(|p| npus.iter().any(|n| n.pci_id == p.pci_id))
        .collect();
    let mut warnings = Vec::new();
    for n in &npus {
        if !platforms
            .iter()
            .any(|p| p.pci_id == n.pci_id && p.min_npu_driver.is_some())
        {
            warnings.push(format!(
                "NPU {} is not in ovfetch's data yet, so nothing is known to work on it",
                n.pci_id
            ));
        }
    }
    let required = platforms
        .iter()
        .filter_map(|p| p.min_npu_driver.as_deref().and_then(Ver::parse))
        .max();
    if !npus.is_empty() {
        let installed = machine.npu_driver.as_deref().and_then(Ver::parse);
        match (&installed, &required) {
            (None, _) => warnings.push(
                "no NPU user-mode driver (libze_intel_npu.so.1) is on the linker path (github.com/intel/linux-npu-driver/releases)".into(),
            ),
            (Some(i), Some(r)) if i < r => warnings.push(format!(
                "NPU driver {i} is older than {r}, Intel's first release verified on this platform (github.com/intel/linux-npu-driver/releases)"
            )),
            _ => {}
        }
        if installed.is_some()
            && let Some(w) = compiler_warning(machine)
        {
            warnings.push(w);
        }
    }
    Status {
        machine,
        platforms,
        openvino_ceiling: driver_ceiling(machine, data).map(|v| v.to_string()),
        npu_driver_required: required.map(|v| v.to_string()),
        warnings,
    }
}

fn compiler_warning(machine: &Machine) -> Option<String> {
    if machine.npu_compiler {
        return None;
    }
    let needs: Vec<String> = machine
        .npu_compiler_needs
        .iter()
        .map(|set| set.join(" + "))
        .collect();
    Some(if needs.is_empty() {
        "the NPU driver names no compiler library ovfetch recognises; assuming it cannot compile models".into()
    } else {
        format!(
            "the NPU driver cannot compile models: it loads {}, and that is not on the linker path",
            needs.join(", or ")
        )
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
    use crate::data::{Ledger, NpuDriver, NpuDrivers, Platforms};
    use crate::detect::Device;

    fn driver(version: &str, openvino: &str, measured: Option<&str>) -> NpuDriver {
        NpuDriver {
            version: version.into(),
            openvino: openvino.into(),
            platforms: vec![],
            measured_openvino: measured.map(Into::into),
            measured_note: None,
        }
    }

    fn data(drivers: Vec<NpuDriver>) -> Data {
        Data {
            dir: None,
            platforms: Platforms::default(),
            npu_drivers: NpuDrivers { list: drivers },
            ledger: Ledger::default(),
        }
    }

    fn npu_machine(driver: &str) -> Machine {
        Machine {
            devices: vec![Device {
                kind: "npu",
                pci_id: "0xfd3e".into(),
                slot: "0000:00:0b.0".into(),
            }],
            npu_driver: Some(driver.into()),
            npu_compiler: true,
            npu_compiler_needs: vec![],
        }
    }

    fn ceiling(driver: &str, d: &Data) -> Option<String> {
        driver_ceiling(&npu_machine(driver), d).map(|v| v.to_string())
    }

    #[test]
    fn ceiling_is_the_pairing_without_a_measurement() {
        let d = data(vec![driver("1.32.0", "2026.0", None)]);
        assert_eq!(ceiling("1.32.0", &d).as_deref(), Some("2026.0"));
    }

    #[test]
    fn a_measured_openvino_raises_the_ceiling() {
        let d = data(vec![driver("1.35.0", "2026.2", Some("2026.4"))]);
        assert_eq!(ceiling("1.35.0", &d).as_deref(), Some("2026.4"));
    }

    #[test]
    fn a_newer_driver_does_not_inherit_an_older_ones_measurement() {
        let d = data(vec![
            driver("1.35.0", "2026.2", Some("2026.4")),
            driver("1.38.0", "2026.3.1", None),
        ]);
        assert_eq!(ceiling("1.38.0", &d).as_deref(), Some("2026.3"));
    }

    #[test]
    fn an_unrecorded_driver_uses_the_newest_release_below_it() {
        let d = data(vec![driver("1.35.0", "2026.2", Some("2026.4"))]);
        assert_eq!(ceiling("1.36.0", &d).as_deref(), Some("2026.4"));
    }

    #[test]
    fn drivers_newer_than_the_installed_one_do_not_count() {
        let d = data(vec![
            driver("1.32.0", "2026.0", None),
            driver("1.35.0", "2026.2", Some("2026.4")),
        ]);
        assert_eq!(ceiling("1.33.0", &d).as_deref(), Some("2026.0"));
    }

    #[test]
    fn shipped_data_puts_driver_1_35_at_2026_4() {
        let d = Data::load(None).unwrap();
        assert_eq!(ceiling("1.35.0", &d).as_deref(), Some("2026.4"));
    }
}
