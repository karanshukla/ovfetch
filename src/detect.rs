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
    /// Whether `libopenvino_intel_npu_compiler.so` is on the linker path. Some
    /// distros package the driver without it, and the NPU cannot compile then.
    pub npu_compiler: bool,
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
    let npu_driver = ldconfig
        .lines()
        .find(|l| l.trim_start().starts_with("libze_intel_npu.so.1 "))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|p| fs::canonicalize(p).ok())
        .and_then(|p| {
            p.file_name()?
                .to_str()?
                .strip_prefix("libze_intel_npu.so.")
                .map(str::to_owned)
        })
        .filter(|v| Ver::parse(v).is_some());
    let npu_compiler = ldconfig.contains("libopenvino_intel_npu_compiler.so");
    Machine {
        devices,
        npu_driver,
        npu_compiler,
    }
}
