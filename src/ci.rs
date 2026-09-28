//! `ci audit` re-checks every recorded hash; `ci discover` records new
//! releases and platforms for a human to review as a PR.

use crate::consensus;
use crate::data::{Data, Entry, NpuDriver, Platform};
use crate::net;
use crate::sources::{self, PypiFile};
use crate::version::Ver;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;

/// The lowest release worth tracking; older wheels predate OpenVINO being bundled.
const TRACKED: &[(&str, &str)] = &[("onnxruntime-openvino", "1.22")];
const NPU_DRIVER_REPO: &str = "intel/linux-npu-driver";

/// Kernel `PCI_DEVICE_ID_*` suffix to the name Intel's driver notes use.
/// A suffix missing here still lands in the PR, with an empty codename to fill.
const CODENAMES: &[(&str, &str)] = &[
    ("MTL", "Meteor Lake"),
    ("ARL", "Arrow Lake"),
    ("LNL", "Lunar Lake"),
    ("PTL_P", "Panther Lake"),
    ("WCL", "Wildcat Lake"),
    ("NVL", "Nova Lake"),
];

fn tracked_wheels(project: &str, min: &str) -> Result<Vec<PypiFile>> {
    let min = Ver::parse(min).context("bad minimum")?;
    let wheels = sources::pypi_wheels(project)?;
    let mut versions: Vec<String> = wheels.iter().map(|w| w.version.clone()).collect();
    versions.dedup();
    Ok(versions
        .iter()
        .filter(|v| Ver::parse(v).is_some_and(|v| v >= min))
        .filter_map(|v| sources::pick_wheel(&wheels, v))
        .collect())
}

/// Returns the markdown report; fails if any recorded hash changed.
pub fn audit(data: &Data, sample: usize) -> Result<String> {
    let mut problems = Vec::new();
    let mut checked = 0;
    let mut pages: HashMap<String, sources::MirrorPages> = HashMap::new();
    let mut wheels: HashMap<String, Vec<PypiFile>> = HashMap::new();
    let mut releases: HashMap<String, Vec<sources::GithubRelease>> = HashMap::new();
    let mut spot: Vec<(PypiFile, String)> = Vec::new();

    for e in &data.ledger.list {
        let parts: Vec<&str> = e.id.split('/').collect();
        let result: Result<()> = (|| match parts.as_slice() {
            ["pypi", project, filename] => {
                let list = match wheels.get(*project) {
                    Some(l) => l,
                    None => wheels
                        .entry(project.to_string())
                        .or_insert(sources::pypi_wheels(project)?),
                };
                let file = list
                    .iter()
                    .find(|w| w.filename == *filename)
                    .context("no longer on PyPI (deleted or yanked)")?;
                let pages = pages
                    .entry(project.to_string())
                    .or_insert_with(|| sources::mirror_pages(project));
                consensus::agree(&e.id, &sources::pypi_claims(file, pages), &data.ledger)?;
                spot.push((file.clone(), e.digest.clone()));
                Ok(())
            }
            ["github", owner, repo, tag, asset] => {
                let key = format!("{owner}/{repo}");
                let list = match releases.get(&key) {
                    Some(l) => l,
                    None => releases
                        .entry(key.clone())
                        .or_insert(sources::github_releases(&key)?),
                };
                let got = list
                    .iter()
                    .find(|r| r.tag == *tag)
                    .and_then(|r| r.assets.iter().find(|a| a.name == *asset))
                    .context("release asset no longer published")?;
                if got.sha256 != e.digest {
                    bail!("GitHub now reports {} (recorded {})", got.sha256, e.digest);
                }
                Ok(())
            }
            _ => bail!("unrecognised ledger id"),
        })();
        checked += 1;
        if let Err(err) = result {
            problems.push(format!("- `{}`: {err:#}", e.id));
        }
    }

    // Index pages can agree while a mirror serves different bytes, so download
    // a random few from a random mirror and hash what actually arrives.
    let start = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .subsec_nanos() as usize;
    let tmp = std::env::temp_dir().join(format!("ovfetch-audit-{}", std::process::id()));
    for i in 0..sample.min(spot.len()) {
        let (file, want) = &spot[(start + i * 7919) % spot.len()];
        let page = pages.get(&file.project).cloned().unwrap_or_default();
        let urls = sources::pypi_download_urls(file, &page);
        let url = &urls[(start / 3 + i) % urls.len()];
        match net::download(url, &tmp) {
            Ok(got) if got == *want => {}
            Ok(got) => problems.push(format!("- `{url}` served sha256 {got}, recorded {want}")),
            Err(err) => eprintln!("note: spot download of {url} failed: {err:#}"),
        }
    }
    let _ = std::fs::remove_file(&tmp);

    let mut report = format!(
        "Audited {checked} recorded artifacts, spot-downloaded {}.\n",
        sample.min(spot.len())
    );
    if problems.is_empty() {
        report.push_str("\nEvery source still agrees with the ledger.\n");
        return Ok(report);
    }
    report.push_str(&format!(
        "\n**{} artifact(s) no longer match what was recorded.** A published artifact's hash must never change; \
         treat each as a possible compromise until explained.\n\n{}\n",
        problems.len(),
        problems.join("\n")
    ));
    bail!(report)
}

/// A release only enters the ledger after this long in public, so a
/// malicious upload has time to be noticed and pulled first.
const COOLDOWN_DAYS: u32 = 7;
const INTEL_ORT_REPO: &str = "intel/onnxruntime";

fn today() -> String {
    date_ago(0)
}

fn date_ago(days: u32) -> String {
    std::process::Command::new("date")
        .args(["-u", "-d", &format!("{days} days ago"), "+%F"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

fn record(data: &mut Data, id: String, digest: String, changes: &mut Vec<String>) {
    if data.ledger.get(&id).is_none() {
        changes.push(format!("- ledger: `{id}` = `{digest}`"));
        data.ledger.list.push(Entry {
            id,
            digest,
            first_seen: today(),
            provenance: false,
        });
    }
}

/// First-cell platform names of a driver release's "Verified Configuration"
/// table, and the OpenVINO version from its component table.
fn parse_driver_notes(body: &str) -> (Vec<String>, Option<Ver>) {
    let mut platforms = Vec::new();
    let mut openvino = None;
    let mut in_platforms = false;
    for line in body.lines().map(str::trim) {
        let row = line.strip_prefix('|').unwrap_or(line);
        let cells: Vec<&str> = row
            .strip_suffix('|')
            .unwrap_or(row)
            .split('|')
            .map(str::trim)
            .collect();
        let first = cells.first().copied().unwrap_or_default();
        let text = first
            .trim_start_matches('[')
            .split(']')
            .next()
            .unwrap_or_default()
            .trim();
        if !line.starts_with('|') {
            in_platforms = false;
            continue;
        }
        if text == "Platform" {
            in_platforms = true;
            continue;
        }
        if text == "Component" {
            in_platforms = false;
        }
        let is_name = text.starts_with(|c: char| c.is_ascii_uppercase())
            && text.chars().all(|c| c.is_ascii_alphanumeric() || c == ' ');
        if in_platforms && is_name {
            platforms.push(text.to_owned());
        }
        if openvino.is_none() && (text == "OpenVINO" || text == "OpenVINO Archive Package") {
            let cell = cells.get(1).copied().unwrap_or_default();
            openvino = Ver::parse(
                cell.trim_start_matches('[')
                    .split(']')
                    .next()
                    .unwrap_or_default(),
            );
        }
    }
    (platforms, openvino)
}

/// Returns markdown describing every change, empty when nothing is new.
pub fn discover(data: &mut Data) -> Result<String> {
    let mut changes = Vec::new();

    let cutoff = date_ago(COOLDOWN_DAYS);
    let intel = sources::github_releases(INTEL_ORT_REPO)?;
    let mut held = Vec::new();
    for (project, min) in TRACKED {
        let pages = sources::mirror_pages(project);
        let attested = sources::pypi_provenance(project)?;
        for w in tracked_wheels(project, min)? {
            let id = w.ledger_id();
            let has_provenance = attested.contains(&w.filename);
            if let Some(e) = data.ledger.list.iter_mut().find(|e| e.id == id) {
                if has_provenance && !e.provenance {
                    e.provenance = true;
                    changes.push(format!(
                        "- **PyPI now publishes provenance for `{id}`.** ovfetch does not verify it yet; that is the next thing to build."
                    ));
                }
                continue;
            }
            if w.uploaded.as_str() > cutoff.as_str() {
                held.push(format!(
                    "- `{id}` (uploaded {}), recorded once it is {COOLDOWN_DAYS} days old",
                    w.uploaded
                ));
                continue;
            }
            // A new release still has to clear the same agreement bar as an install.
            let (digest, _) =
                consensus::agree(&id, &sources::pypi_claims(&w, &pages), &data.ledger)?;
            let needle = format!("onnxruntime {}", w.version);
            let release = intel
                .iter()
                .find(|r| r.body.to_lowercase().contains(&needle));
            changes.push(format!(
                "- ledger: `{id}` = `{digest}`, uploaded {}, {}{}",
                w.uploaded,
                match release {
                    Some(r) => format!("Intel release [{}]({})", r.tag, r.url),
                    None => format!("**no {INTEL_ORT_REPO} release mentions ONNX Runtime {}; check before merging**", w.version),
                },
                if has_provenance { ", PyPI provenance published" } else { "" }
            ));
            data.ledger.list.push(Entry {
                id,
                digest,
                first_seen: today(),
                provenance: has_provenance,
            });
        }
    }

    let mut drivers = sources::github_releases(NPU_DRIVER_REPO)?;
    drivers.sort_by_key(|r| Ver::parse(&r.tag));
    for r in &drivers {
        let (platforms, openvino) = parse_driver_notes(&r.body);
        let (Some(version), Some(openvino)) = (Ver::parse(&r.tag), openvino) else {
            continue;
        };
        if !data
            .npu_drivers
            .list
            .iter()
            .any(|d| Ver::parse(&d.version).as_ref() == Some(&version))
        {
            changes.push(format!(
                "- NPU driver {version}: OpenVINO {openvino}, verified on {}",
                platforms.join(", ")
            ));
            data.npu_drivers.list.push(NpuDriver {
                version: version.to_string(),
                openvino: openvino.to_string(),
                platforms,
                measured_openvino: None,
                measured_note: None,
            });
        }
        for a in &r.assets {
            record(
                data,
                format!("github/{NPU_DRIVER_REPO}/{}/{}", r.tag, a.name),
                a.sha256.clone(),
                &mut changes,
            );
        }
    }
    data.npu_drivers
        .list
        .sort_by_key(|d| Ver::parse(&d.version));

    let header = sources::github_raw("torvalds/linux", "master", "drivers/accel/ivpu/ivpu_drv.h")?;
    for line in header.lines() {
        let mut words = line.split_whitespace();
        let (Some("#define"), Some(name), Some(id)) = (words.next(), words.next(), words.next())
        else {
            continue;
        };
        let Some(kernel_name) = name.strip_prefix("PCI_DEVICE_ID_") else {
            continue;
        };
        if !data.platforms.list.iter().any(|p| p.pci_id == id) {
            let codename = CODENAMES
                .iter()
                .find(|(k, _)| *k == kernel_name)
                .map(|(_, c)| c.to_string())
                .unwrap_or_default();
            changes.push(format!(
                "- new NPU platform `{id}` ({kernel_name}){}",
                if codename.is_empty() {
                    ": **codename unknown, fill it in before merging**".to_owned()
                } else {
                    format!(" = {codename}")
                }
            ));
            data.platforms.list.push(Platform {
                pci_id: id.to_owned(),
                kernel_name: kernel_name.to_owned(),
                codename,
                min_openvino: None,
                min_npu_driver: None,
                pinned_note: None,
            });
        }
    }

    // A platform's floor is the pairing of the first driver release verified on it.
    for p in data
        .platforms
        .list
        .iter_mut()
        .filter(|p| p.pinned_note.is_none() && !p.codename.is_empty())
    {
        let Some(first) = data
            .npu_drivers
            .list
            .iter()
            .find(|d| d.platforms.contains(&p.codename))
        else {
            continue;
        };
        let ov = Ver::parse(&first.openvino).map(|v| v.minor().to_string());
        if p.min_openvino != ov || p.min_npu_driver.as_deref() != Some(first.version.as_str()) {
            changes.push(format!(
                "- {} floor: OpenVINO {} with NPU driver {}",
                p.codename,
                ov.as_deref().unwrap_or("?"),
                first.version
            ));
            p.min_openvino = ov;
            p.min_npu_driver = Some(first.version.clone());
        }
    }

    if !changes.is_empty() && !held.is_empty() {
        changes.push(format!("\nHeld back by the {COOLDOWN_DAYS}-day cooldown:"));
        changes.extend(held);
    }
    Ok(changes.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_platforms_and_openvino_from_driver_notes() {
        let body = "## Verified Configuration\n\n|Platform|System|Kernel|\n|:---:|:---:|:---:|\n\
                    |[Meteor Lake](https://x)|Ubuntu|6.8|\n||**Ubuntu24.04**|6.11|\n|Wildcat Lake|Ubuntu|7.0|\n|----------|\n|**MTL**|x|\n\n\
                    |Component|Version|\n|:---|:---|\n|OpenVINO|[2025.1.0rc2-pre-release](https://y)|\n";
        let (platforms, ov) = parse_driver_notes(body);
        assert_eq!(platforms, ["Meteor Lake", "Wildcat Lake"]);
        assert_eq!(ov.unwrap().to_string(), "2025.1.0");
    }

    #[test]
    fn prefers_the_archive_row_in_older_notes() {
        let body = "|Component|Version|\n|OpenVINO Archive Package|[2024.4.0](https://z)|\n|OpenVINO Python Package|[2024.3.0](https://z)|\n";
        assert_eq!(parse_driver_notes(body).1.unwrap().to_string(), "2024.4.0");
    }
}
