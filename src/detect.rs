//! What hardware and NPU userspace this machine has.

use crate::version::Ver;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Written by `ovfetch npu install`: the libraries an app loads by absolute
/// path, in order, before OpenVINO looks for the NPU driver.
pub const NPU_PRELOAD: &str = "ovfetch-preload.txt";

const INTEL_VENDOR: &str = "0x8086";
/// PCI class prefixes: 0x12 is a processing accelerator, 0x03 a display controller.
const NPU_CLASS: &str = "0x12";
const GPU_CLASS: &str = "0x03";
const KIND_NPU: &str = "npu";
const KIND_GPU: &str = "gpu";

/// Soname of the NPU user-mode driver.
pub const NPU_DRIVER: &str = "libze_intel_npu.so.1";
/// The driver's real file carries its release after this: `libze_intel_npu.so.1.35.0`.
const NPU_DRIVER_FILE_PREFIX: &str = "libze_intel_npu.so.";

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
    /// An `ovfetch npu install` prefix whose libraries count as installed:
    /// its driver replaces the host's, and its compiler is on the path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub npu_prefix: Option<String>,
}

impl Machine {
    pub fn npus(&self) -> impl Iterator<Item = &Device> {
        self.devices.iter().filter(|d| d.kind == KIND_NPU)
    }

    pub fn has_npu(&self) -> bool {
        self.npus().next().is_some()
    }
}

/// Where `ovfetch npu install` puts the NPU driver and compiler by default.
pub fn default_npu_prefix() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".local/lib/ovfetch-npu"))
}

/// The name a driver's binary carries, and the libraries that route loads.
/// 1.35 stores the loader's name split, hence `..._compiler_l`. Measurements
/// are in docs/npu.md.
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

/// The driver opens this one by absolute path from its own directory, not by
/// soname, so a copy anywhere else is never found (docs/npu.md).
pub const COMPILER_BESIDE_DRIVER: &str = "libopenvino_intel_npu_compiler_loader.so";

pub fn compiler_needs(driver: &[u8]) -> Vec<Vec<String>> {
    COMPILER_ROUTES
        .iter()
        .filter(|(needle, _)| driver.windows(needle.len()).any(|w| w == needle.as_bytes()))
        .map(|(_, libs)| libs.iter().map(|l| l.to_string()).collect())
        .collect()
}

/// Whether the Level Zero loader, which a driver in a prefix still needs from
/// the host, is on the linker path.
pub fn ze_loader_present() -> bool {
    sonames(&ldconfig_cache()).contains(&ZE_LOADER)
}

/// What `ldconfig -p` prints, empty when it cannot be run.
fn ldconfig_cache() -> String {
    Command::new("ldconfig")
        .arg("-p")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Intel NPUs and GPUs among the PCI devices sysfs lists under `pci_devices`.
fn intel_devices(pci_devices: &Path) -> Vec<Device> {
    let mut devices = Vec::new();
    for entry in fs::read_dir(pci_devices).into_iter().flatten().flatten() {
        let attribute = |name: &str| {
            fs::read_to_string(entry.path().join(name))
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        if attribute("vendor") != INTEL_VENDOR {
            continue;
        }
        let kind = match attribute("class").get(..4) {
            Some(NPU_CLASS) => KIND_NPU,
            Some(GPU_CLASS) => KIND_GPU,
            _ => continue,
        };
        devices.push(Device {
            kind,
            pci_id: attribute("device"),
            slot: entry.file_name().to_string_lossy().into_owned(),
        });
    }
    devices
}

pub const ZE_LOADER: &str = "libze_loader.so.1";

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

/// The release a resolved driver file is named for:
/// `libze_intel_npu.so.1.35.0` is `1.35.0`.
fn driver_version(driver_file: &Path) -> Option<String> {
    driver_file
        .file_name()?
        .to_str()?
        .strip_prefix(NPU_DRIVER_FILE_PREFIX)
        .map(str::to_owned)
        .filter(|v| Ver::parse(v).is_some())
}

/// This machine, counting an NPU prefix at the default location.
pub fn machine() -> Machine {
    machine_with(default_npu_prefix().as_deref())
}

/// This machine, counting the NPU prefix at `npu_prefix` (if it holds a
/// preload list). `None` looks at the host alone.
pub fn machine_with(npu_prefix: Option<&Path>) -> Machine {
    let devices = intel_devices(Path::new("/sys/bus/pci/devices"));
    let ldconfig = ldconfig_cache();
    let npu_prefix = npu_prefix.filter(|p| p.join(NPU_PRELOAD).is_file());
    let in_prefix: Vec<String> = npu_prefix
        .and_then(|p| fs::read_dir(p).ok())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let host_driver = || {
        ldconfig
            .lines()
            .find(|l| l.trim_start().starts_with(&format!("{NPU_DRIVER} ")))
            .and_then(|l| l.rsplit(' ').next())
            .and_then(|p| fs::canonicalize(p).ok())
    };
    let prefix_driver = npu_prefix
        .filter(|_| in_prefix.iter().any(|n| n == NPU_DRIVER))
        .and_then(|p| fs::canonicalize(p.join(NPU_DRIVER)).ok());
    let has_prefix_driver = prefix_driver.is_some();
    let driver_path = prefix_driver.or_else(host_driver);
    let npu_driver = driver_path.as_deref().and_then(driver_version);
    let npu_compiler_needs = driver_path
        .and_then(|p| fs::read(p).ok())
        .map(|b| compiler_needs(&b))
        .unwrap_or_default();
    let mut found = sonames(&ldconfig);
    found.extend(
        in_prefix
            .iter()
            .map(String::as_str)
            .filter(|n| has_prefix_driver || *n != COMPILER_BESIDE_DRIVER),
    );
    let npu_compiler = compiler_present(&npu_compiler_needs, &found);
    Machine {
        devices,
        npu_driver,
        npu_compiler,
        npu_compiler_needs,
        npu_prefix: npu_prefix.map(|p| p.display().to_string()),
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
    fn only_intel_npus_and_gpus_are_devices() {
        let sysfs = std::env::temp_dir().join(format!("ovfetch-sysfs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&sysfs);
        for (slot, vendor, class, device) in [
            ("0000:00:0b.0", "0x8086", "0x120000", "0xfd3e"),
            ("0000:00:02.0", "0x8086", "0x030000", "0xfd80"),
            ("0000:00:14.0", "0x8086", "0x0c0330", "0x4d7d"),
            ("0000:01:00.0", "0x10de", "0x030000", "0x2684"),
        ] {
            let dir = sysfs.join(slot);
            fs::create_dir_all(&dir).unwrap();
            for (name, value) in [("vendor", vendor), ("class", class), ("device", device)] {
                fs::write(dir.join(name), format!("{value}\n")).unwrap();
            }
        }
        let mut found: Vec<(String, &str, String)> = intel_devices(&sysfs)
            .into_iter()
            .map(|d| (d.slot, d.kind, d.pci_id))
            .collect();
        found.sort();
        fs::remove_dir_all(&sysfs).unwrap();
        assert_eq!(
            found,
            [
                ("0000:00:02.0".to_owned(), "gpu", "0xfd80".to_owned()),
                ("0000:00:0b.0".to_owned(), "npu", "0xfd3e".to_owned()),
            ]
        );
        assert!(intel_devices(&sysfs).is_empty());
    }

    #[test]
    fn the_driver_version_comes_from_its_resolved_file_name() {
        let version = |p: &str| driver_version(Path::new(p));
        assert_eq!(
            version("/usr/lib64/libze_intel_npu.so.1.35.0").as_deref(),
            Some("1.35.0")
        );
        assert_eq!(version("/usr/lib64/libze_intel_npu.so.debug"), None);
        assert_eq!(version("/usr/lib64/libze_loader.so.1.21.9"), None);
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
