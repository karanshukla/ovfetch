//! What hardware and NPU userspace this machine has.

use crate::version::Ver;
use serde::Serialize;
use std::fs;
use std::process::Command;

#[derive(Debug, Serialize)]
pub struct Device {
    pub kind: &'static str,
    pub pci_id: String,
    pub slot: String,
}

#[derive(Debug, Serialize)]
pub struct Machine {
    pub devices: Vec<Device>,
    /// Version of the loaded `libze_intel_npu.so.1`, the NPU user-mode driver.
    pub npu_driver: Option<String>,
    /// Whether the compiler the installed driver loads is on the linker path.
    /// Some distros package the driver without it, and the NPU cannot compile
    /// then; `compile_model` fails with `ZE_RESULT_ERROR_UNSUPPORTED_FEATURE`.
    pub npu_compiler: bool,
    /// The compiler libraries the driver's binary names, as alternatives each
    /// of which needs every library in it.
    pub npu_compiler_needs: Vec<Vec<String>>,
}

/// The name a driver's binary carries, and the libraries that route loads.
/// Measured 2026-09-28: 1.32.0 names only `libnpu_driver_compiler.so`; 1.35.0
/// tries the OpenVINO compiler loader first (stored split, as `..._compiler_l`),
/// which maps `libopenvino_intel_npu_compiler.so` in turn.
const COMPILER_ROUTES: &[(&str, &[&str])] = &[
    (
        "libopenvino_intel_npu_compiler_l",
        &[
            "libopenvino_intel_npu_compiler_loader.so",
            "libopenvino_intel_npu_compiler.so",
        ],
    ),
    ("libnpu_driver_compiler.so", &["libnpu_driver_compiler.so"]),
];

pub fn compiler_needs(driver: &[u8]) -> Vec<Vec<String>> {
    COMPILER_ROUTES
        .iter()
        .filter(|(needle, _)| driver.windows(needle.len()).any(|w| w == needle.as_bytes()))
        .map(|(_, libs)| libs.iter().map(|l| l.to_string()).collect())
        .collect()
}

/// Sonames `ldconfig -p` lists, the first word of each entry.
pub fn sonames(ldconfig: &str) -> Vec<&str> {
    ldconfig
        .lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().next())
        .collect()
}

pub fn compiler_present(needs: &[Vec<String>], sonames: &[&str]) -> bool {
    needs
        .iter()
        .any(|set| set.iter().all(|lib| sonames.contains(&lib.as_str())))
}

pub fn machine() -> Machine {
    let mut devices = Vec::new();
    for entry in fs::read_dir("/sys/bus/pci/devices")
        .into_iter()
        .flatten()
        .flatten()
    {
        let read = |f: &str| {
            fs::read_to_string(entry.path().join(f))
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        if read("vendor") != "0x8086" {
            continue;
        }
        let kind = match read("class").get(..4) {
            Some("0x12") => "npu",
            Some("0x03") => "gpu",
            _ => continue,
        };
        devices.push(Device {
            kind,
            pci_id: read("device"),
            slot: entry.file_name().to_string_lossy().into_owned(),
        });
    }
    let ldconfig = Command::new("ldconfig")
        .arg("-p")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let driver_path = ldconfig
        .lines()
        .find(|l| l.trim_start().starts_with("libze_intel_npu.so.1 "))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|p| fs::canonicalize(p).ok());
    let npu_driver = driver_path
        .as_ref()
        .and_then(|p| {
            p.file_name()?
                .to_str()?
                .strip_prefix("libze_intel_npu.so.")
                .map(str::to_owned)
        })
        .filter(|v| Ver::parse(v).is_some());
    let npu_compiler_needs = driver_path
        .and_then(|p| fs::read(p).ok())
        .map(|b| compiler_needs(&b))
        .unwrap_or_default();
    let npu_compiler = compiler_present(&npu_compiler_needs, &sonames(&ldconfig));
    Machine {
        devices,
        npu_driver,
        npu_compiler,
        npu_compiler_needs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LDCONFIG: &str = "3 libs found in cache `/etc/ld.so.cache'
\tlibze_intel_npu.so.1 (libc6,x86-64) => /lib64/libze_intel_npu.so.1
\tlibopenvino_intel_npu_compiler_loader.so (libc6,x86-64) => /lib64/libopenvino_intel_npu_compiler_loader.so
\tlibopenvino_intel_npu_compiler.so (libc6,x86-64) => /lib64/libopenvino_intel_npu_compiler.so
";

    fn needs(driver: &[u8]) -> Vec<Vec<String>> {
        compiler_needs(driver)
    }

    #[test]
    fn driver_1_32_needs_the_old_compiler_name() {
        let n = needs(b"..\0libnpu_driver_compiler.so\0..");
        assert_eq!(n, vec![vec!["libnpu_driver_compiler.so".to_string()]]);
        assert!(!compiler_present(&n, &sonames(LDCONFIG)));
    }

    #[test]
    fn driver_1_35_is_satisfied_by_the_openvino_loader() {
        let n = needs(b"libopenvino_intel_npu_compiler_l\0libnpu_driver_compiler.so");
        assert_eq!(n.len(), 2);
        assert!(compiler_present(&n, &sonames(LDCONFIG)));
    }

    #[test]
    fn the_loader_alone_is_not_enough() {
        let n = needs(b"libopenvino_intel_npu_compiler_l");
        let only_loader = ["libopenvino_intel_npu_compiler_loader.so"];
        assert!(!compiler_present(&n, &only_loader));
    }

    #[test]
    fn an_unrecognised_driver_is_assumed_to_have_no_compiler() {
        assert!(needs(b"nothing here").is_empty());
        assert!(!compiler_present(&[], &sonames(LDCONFIG)));
    }

    #[test]
    fn a_soname_prefix_does_not_count() {
        let n = vec![vec!["libopenvino_intel_npu_compiler.so".to_string()]];
        assert!(!compiler_present(
            &n,
            &["libopenvino_intel_npu_compiler.so.1"]
        ));
    }
}
