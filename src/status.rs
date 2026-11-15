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

/// Where the floor comes from. [`Floor`] is the form `detect` and `resolve`
/// print; code that has to act on the cause matches on this instead of on
/// the wording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FloorSource {
    /// `--min-openvino`, kept as the user typed it.
    Override(String),
    /// The highest floor among the machine's NPUs.
    Platform {
        openvino: Ver,
        codename: String,
        pci_id: String,
    },
    NoNpu,
    /// An NPU the data has no floor for, so no version is known to be safe.
    UnknownNpu {
        pci_id: String,
    },
}

impl From<FloorSource> for Floor {
    fn from(source: FloorSource) -> Floor {
        match source {
            FloorSource::Override(openvino) => Floor {
                openvino: Some(openvino),
                reason: "--min-openvino".into(),
            },
            FloorSource::Platform {
                openvino,
                codename,
                pci_id,
            } => Floor {
                openvino: Some(openvino.to_string()),
                reason: format!("{codename} NPU ({pci_id})"),
            },
            FloorSource::NoNpu => Floor {
                openvino: None,
                reason: "no Intel NPU; any current OpenVINO runs the CPU and GPU".into(),
            },
            FloorSource::UnknownNpu { pci_id } => Floor {
                openvino: None,
                reason: format!("NPU {pci_id} has no known floor yet"),
            },
        }
    }
}

pub fn floor(machine: &Machine, data: &Data, override_min: Option<&str>) -> Floor {
    floor_source(machine, data, override_min).into()
}

pub fn floor_source(machine: &Machine, data: &Data, override_min: Option<&str>) -> FloorSource {
    if let Some(openvino) = override_min {
        return FloorSource::Override(openvino.to_owned());
    }
    let mut highest: Option<(Ver, &Platform)> = None;
    for npu in machine.npus() {
        let platform = data.platforms.list.iter().find(|p| p.pci_id == npu.pci_id);
        match platform.and_then(|p| Some((p, Ver::parse(p.min_openvino.as_deref()?)?))) {
            Some((p, v)) if highest.as_ref().is_none_or(|(h, _)| v > *h) => {
                highest = Some((v, p));
            }
            Some(_) => {}
            None => {
                return FloorSource::UnknownNpu {
                    pci_id: npu.pci_id.clone(),
                };
            }
        }
    }
    match highest {
        Some((openvino, platform)) => FloorSource::Platform {
            openvino,
            codename: platform.codename.clone(),
            pci_id: platform.pci_id.clone(),
        },
        None => FloorSource::NoNpu,
    }
}

/// Newest OpenVINO known to work on the installed NPU driver: the newest
/// recorded release at or below it, at Intel's pairing or a newer measured
/// version. Deliberately pessimistic: beyond it is untested, not known to fail.
pub fn driver_ceiling(machine: &Machine, data: &Data) -> Option<Ver> {
    if !machine.has_npu() {
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
    let npus: Vec<_> = machine.npus().collect();
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
            "the NPU driver cannot compile models: it loads {}, and that is not on the linker path \
             (`ovfetch npu install` fetches it)",
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
            builds: Default::default(),
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
            npu_prefix: None,
        }
    }

    fn ceiling(driver: &str, d: &Data) -> Option<String> {
        driver_ceiling(&npu_machine(driver), d).map(|v| v.to_string())
    }

    #[cfg(feature = "cli")]
    fn platform(pci_id: &str, codename: &str, min_openvino: Option<&str>) -> Platform {
        Platform {
            pci_id: pci_id.into(),
            kernel_name: "X".into(),
            codename: codename.into(),
            min_openvino: min_openvino.map(Into::into),
            min_npu_driver: None,
            pinned_note: None,
        }
    }

    #[cfg(feature = "cli")]
    fn floor_json(machine: &Machine, override_min: Option<&str>) -> String {
        let mut d = data(vec![]);
        d.platforms.list = vec![
            platform("0xfd3e", "Wildcat Lake", Some("2026.0")),
            platform("0x7d1d", "Meteor Lake", Some("2025.1.0")),
            platform("0xaaaa", "", None),
        ];
        serde_json::to_string(&floor(machine, &d, override_min)).unwrap()
    }

    #[cfg(feature = "cli")]
    fn machine_with_npus(pci_ids: &[&str]) -> Machine {
        let mut machine = npu_machine("1.35.0");
        machine.devices = pci_ids
            .iter()
            .map(|id| Device {
                kind: "npu",
                pci_id: id.to_string(),
                slot: String::new(),
            })
            .collect();
        machine
    }

    /// serde_json is a `cli` dependency, so this runs with the default features.
    #[cfg(feature = "cli")]
    #[test]
    fn the_floor_serialises_as_it_always_has() {
        let wildcat = machine_with_npus(&["0xfd3e"]);
        assert_eq!(
            floor_json(&wildcat, None),
            r#"{"openvino":"2026.0","reason":"Wildcat Lake NPU (0xfd3e)"}"#
        );
        assert_eq!(
            floor_json(&wildcat, Some("v2026.2")),
            r#"{"openvino":"v2026.2","reason":"--min-openvino"}"#
        );
        assert_eq!(
            floor_json(&machine_with_npus(&["0x7d1d", "0xfd3e"]), None),
            r#"{"openvino":"2026.0","reason":"Wildcat Lake NPU (0xfd3e)"}"#
        );
        assert_eq!(
            floor_json(&machine_with_npus(&["0x7d1d"]), None),
            r#"{"openvino":"2025.1.0","reason":"Meteor Lake NPU (0x7d1d)"}"#
        );
        assert_eq!(
            floor_json(&machine_with_npus(&[]), None),
            r#"{"openvino":null,"reason":"no Intel NPU; any current OpenVINO runs the CPU and GPU"}"#
        );
        for unknown in ["0xaaaa", "0xbbbb"] {
            assert_eq!(
                floor_json(&machine_with_npus(&["0xfd3e", unknown]), None),
                format!(r#"{{"openvino":null,"reason":"NPU {unknown} has no known floor yet"}}"#)
            );
        }
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
