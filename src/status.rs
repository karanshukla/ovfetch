//! What the data says about this machine's NPU, with no network access: the
//! OpenVINO floor and ceiling, and what is missing for the NPU to work.

use crate::data::{Data, Platform};
use crate::detect::Machine;
use crate::version::Ver;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Floor {
    /// `None` when nothing constrains it (no NPU).
    pub openvino: Option<String>,
    pub reason: String,
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

pub fn compiler_warning(machine: &Machine) -> Option<String> {
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
