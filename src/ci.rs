//! `ci audit` re-checks every recorded hash; `ci discover` records new
//! releases and platforms for a human to review as a PR.

use crate::consensus;
use crate::data::{Data, Entry, NpuDriver, Platform};
use crate::sources::{self, GithubRelease, MirrorPages, PypiFile};
use crate::tempdir;
use crate::upstream::{Network, Upstream};
use crate::version::Ver;
use anyhow::{Context, Result, bail};
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan};
use std::collections::HashMap;

/// PyPI projects whose wheels `discover` records, and the oldest version kept:
/// older onnxruntime-openvino wheels predate OpenVINO being bundled.
/// `openvino` alone serves `install --openvino-only` and every recipe's input.
const TRACKED: &[(&str, &str)] = &[("onnxruntime-openvino", "1.22"), ("openvino", "2025.4")];
pub(crate) const NPU_DRIVER_REPO: &str = "intel/linux-npu-driver";

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

/// `items[n % len]`, or `None` when there is nothing to pick from.
fn nth_wrapping<T>(items: &[T], n: usize) -> Option<&T> {
    items.get(n.checked_rem(items.len())?)
}

/// What `audit` has already asked upstream, so each project and repo is
/// fetched once however many ledger entries name it.
#[derive(Default)]
struct Fetched {
    wheels: HashMap<String, Vec<PypiFile>>,
    pages: HashMap<String, MirrorPages>,
    releases: HashMap<String, Vec<GithubRelease>>,
}

/// Returns the markdown report; fails if any recorded hash changed.
pub fn audit(data: &Data, sample: usize) -> Result<String> {
    let clock_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .subsec_nanos() as usize;
    audit_with(&Network, data, sample, clock_nanos)
}

/// [`audit`] against any [`Upstream`]. `seed` picks which recorded wheels are
/// downloaded and from which source.
pub fn audit_with(
    upstream: &dyn Upstream,
    data: &Data,
    sample: usize,
    seed: usize,
) -> Result<String> {
    let mut problems = Vec::new();
    let mut fetched = Fetched::default();
    let mut downloadable: Vec<(PypiFile, String)> = Vec::new();

    for entry in &data.ledger.list {
        let parts: Vec<&str> = entry.id.split('/').collect();
        let result = match parts.as_slice() {
            ["pypi", project, filename] => {
                audit_pypi_file(upstream, data, &mut fetched, entry, project, filename)
                    .map(|file| downloadable.push((file, entry.digest.clone())))
            }
            ["github", owner, repo, tag, asset] => audit_github_asset(
                upstream,
                &mut fetched,
                entry,
                &format!("{owner}/{repo}"),
                tag,
                asset,
            ),
            _ => Err(anyhow::anyhow!("unrecognised ledger id")),
        };
        if let Err(err) = result {
            problems.push(format!("- `{}`: {err:#}", entry.id));
        }
    }
    audit_recipes(upstream, data, &mut problems);
    let sampled = sample.min(downloadable.len());
    spot_download(
        upstream,
        &fetched.pages,
        &downloadable,
        sampled,
        seed,
        &mut problems,
    )?;

    let checked = data.ledger.list.len() + data.builds.list.len();
    audit_report(checked, sampled, &problems)
}

/// Checks that PyPI and its mirrors still agree with the ledger on one wheel.
fn audit_pypi_file(
    upstream: &dyn Upstream,
    data: &Data,
    fetched: &mut Fetched,
    entry: &Entry,
    project: &str,
    filename: &str,
) -> Result<PypiFile> {
    let wheels = match fetched.wheels.get(project) {
        Some(wheels) => wheels,
        None => fetched
            .wheels
            .entry(project.to_owned())
            .or_insert(upstream.pypi_wheels(project)?),
    };
    let file = wheels
        .iter()
        .find(|w| w.filename == filename)
        .context("no longer on PyPI (deleted or yanked)")?;
    let pages = fetched
        .pages
        .entry(project.to_owned())
        .or_insert_with(|| upstream.mirror_pages(project));
    consensus::agree(&entry.id, &sources::pypi_claims(file, pages), &data.ledger)?;
    Ok(file.clone())
}

/// Checks that GitHub still reports the recorded digest for a release asset.
fn audit_github_asset(
    upstream: &dyn Upstream,
    fetched: &mut Fetched,
    entry: &Entry,
    repo: &str,
    tag: &str,
    asset: &str,
) -> Result<()> {
    let releases = match fetched.releases.get(repo) {
        Some(releases) => releases,
        None => fetched
            .releases
            .entry(repo.to_owned())
            .or_insert(upstream.github_releases(repo)?),
    };
    let published = releases
        .iter()
        .find(|r| r.tag == tag)
        .and_then(|r| r.assets.iter().find(|a| a.name == asset))
        .context("release asset no longer published")?;
    if published.sha256 != entry.digest {
        bail!(
            "GitHub now reports {} (recorded {})",
            published.sha256,
            entry.digest
        );
    }
    Ok(())
}

/// Every recipe's OpenVINO input must be in the ledger, and its tag must
/// still point at the pinned commit.
fn audit_recipes(upstream: &dyn Upstream, data: &Data, problems: &mut Vec<String>) {
    for recipe in &data.builds.list {
        let id = format!("build {} {}", recipe.git_repo, recipe.git_tag);
        if data.ledger.get(&recipe.openvino_wheel).is_none() {
            problems.push(format!(
                "- `{id}`: input {} is not in the ledger",
                recipe.openvino_wheel
            ));
        }
        match upstream.github_tag_commit(&recipe.git_repo, &recipe.git_tag) {
            Ok(commit) if commit == recipe.git_commit => {}
            Ok(commit) => problems.push(format!(
                "- `{id}`: tag now points at {commit}, recipe pins {}",
                recipe.git_commit
            )),
            Err(e) => problems.push(format!("- `{id}`: {e:#}")),
        }
    }
}

/// A stride coprime to any plausible ledger size, so successive picks spread out.
const SAMPLE_STRIDE: usize = 7919;

/// Index pages can agree while a mirror serves different bytes, so this
/// downloads `count` recorded wheels, each from one of its sources, and
/// hashes what arrives.
fn spot_download(
    upstream: &dyn Upstream,
    pages: &HashMap<String, MirrorPages>,
    downloadable: &[(PypiFile, String)],
    count: usize,
    seed: usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    let dir = tempdir::private()?;
    let dest = dir.path().join("spot");
    for i in 0..count {
        let (file, recorded) = &downloadable[(seed + i * SAMPLE_STRIDE) % downloadable.len()];
        let page = pages.get(&file.project).cloned().unwrap_or_default();
        let urls = sources::pypi_download_urls(file, &page);
        let Some(url) = nth_wrapping(&urls, seed / 3 + i) else {
            eprintln!(
                "note: no source lists a download URL for {}",
                file.ledger_id()
            );
            continue;
        };
        match upstream.download(url, &dest) {
            Ok(got) if got == *recorded => {}
            Ok(got) => problems.push(format!(
                "- `{url}` served sha256 {got}, recorded {recorded}"
            )),
            Err(err) => eprintln!("note: spot download of {url} failed: {err:#}"),
        }
    }
    Ok(())
}

fn audit_report(checked: usize, sampled: usize, problems: &[String]) -> Result<String> {
    let mut report = format!("Audited {checked} recorded artifacts, spot-downloaded {sampled}.\n");
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

/// Today's UTC date as `YYYY-MM-DD`, the form PyPI's upload dates and the
/// ledger's `first_seen` use, so dates compare as text.
fn today() -> String {
    date_ago(0)
}

fn date_ago(days: u32) -> String {
    days_before(utc_today(), days).to_string()
}

fn utc_today() -> Date {
    Timestamp::now().to_zoned(TimeZone::UTC).date()
}

/// Saturates at the calendar's first day; no real cooldown reaches it.
fn days_before(date: Date, days: u32) -> Date {
    date.saturating_sub(i64::from(days).days())
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
            // Only onnxruntime-openvino wheels have an intel/onnxruntime release to match.
            let origin = if *project == "onnxruntime-openvino" {
                let needle = format!("onnxruntime {}", w.version);
                match intel
                    .iter()
                    .find(|r| r.body.to_lowercase().contains(&needle))
                {
                    Some(r) => format!(", Intel release [{}]({})", r.tag, r.url),
                    None => format!(
                        ", **no {INTEL_ORT_REPO} release mentions ONNX Runtime {}; check before merging**",
                        w.version
                    ),
                }
            } else {
                String::new()
            };
            changes.push(format!(
                "- ledger: `{id}` = `{digest}`, uploaded {}{origin}{}",
                w.uploaded,
                if has_provenance {
                    ", PyPI provenance published"
                } else {
                    ""
                }
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
    fn picking_from_no_urls_does_not_panic() {
        assert_eq!(nth_wrapping::<String>(&[], 7), None);
        assert_eq!(nth_wrapping(&["a", "b", "c"], 7), Some(&"b"));
    }

    fn is_iso_date(s: &str) -> bool {
        let b = s.as_bytes();
        b.len() == 10
            && b.iter().enumerate().all(|(i, c)| match i {
                4 | 7 => *c == b'-',
                _ => c.is_ascii_digit(),
            })
    }

    #[test]
    fn dates_are_utc_iso_days_that_sort_as_text() {
        let (today, week_ago, year_ago) = (today(), date_ago(COOLDOWN_DAYS), date_ago(366));
        for d in [&today, &week_ago, &year_ago] {
            assert!(is_iso_date(d), "{d:?}");
        }
        assert_eq!(date_ago(0), today);
        assert!(year_ago < week_ago && week_ago < today);
        assert_ne!(&year_ago[..4], &today[..4]);
    }

    #[test]
    fn the_cooldown_counts_back_across_month_year_and_leap_day() {
        let before = |y, m, d, n| days_before(jiff::civil::date(y, m, d), n).to_string();
        assert_eq!(before(2026, 11, 15, 0), "2026-11-15");
        assert_eq!(before(2026, 11, 15, 7), "2026-11-08");
        assert_eq!(before(2026, 11, 3, 7), "2026-10-27");
        assert_eq!(before(2027, 1, 2, 7), "2026-12-26");
        assert_eq!(before(2028, 3, 3, 7), "2028-02-25");
        assert_eq!(before(2028, 3, 1, 1), "2028-02-29");
        assert_eq!(before(2027, 3, 1, 1), "2027-02-28");
    }

    use crate::data::{Build, Builds, Ledger, NpuDrivers, Platforms};
    use crate::sources::GithubAsset;
    use crate::upstream::fake::{Fake, wheel};

    const DRIVER_ASSET: &str = "github/intel/linux-npu-driver/v1.35.0/driver.tar.gz";

    fn ledger_entry(id: &str, digest: &str) -> Entry {
        Entry {
            id: id.into(),
            digest: digest.into(),
            first_seen: "2026-01-08".into(),
            provenance: false,
        }
    }

    /// One recorded wheel and one recorded release asset, with an upstream
    /// that still agrees with both.
    fn recorded() -> (Fake, Data) {
        let ov = wheel("openvino", "2026.4.1", "cp313", "0a");
        let mut fake = Fake::with_wheels(vec![ov.clone()]);
        fake.releases.insert(
            NPU_DRIVER_REPO.into(),
            vec![GithubRelease {
                tag: "v1.35.0".into(),
                url: String::new(),
                body: String::new(),
                assets: vec![GithubAsset {
                    name: "driver.tar.gz".into(),
                    sha256: "d1".repeat(32),
                }],
            }],
        );
        fake.downloads.insert(ov.url.clone(), ov.sha256.clone());
        let data = Data {
            dir: None,
            platforms: Platforms::default(),
            npu_drivers: NpuDrivers::default(),
            ledger: Ledger {
                list: vec![
                    ledger_entry(&ov.ledger_id(), &ov.sha256),
                    ledger_entry(DRIVER_ASSET, &"d1".repeat(32)),
                ],
            },
            builds: Builds::default(),
        };
        (fake, data)
    }

    #[test]
    fn an_audit_passes_while_every_source_agrees_with_the_ledger() {
        let (fake, data) = recorded();
        let report = audit_with(&fake, &data, 1, 0).unwrap();
        assert_eq!(
            report,
            "Audited 2 recorded artifacts, spot-downloaded 1.\n\nEvery source still agrees with the ledger.\n"
        );
        assert_eq!(fake.downloaded.borrow().len(), 1);
    }

    #[test]
    fn an_audit_samples_no_more_wheels_than_asked_or_recorded() {
        let (fake, data) = recorded();
        audit_with(&fake, &data, 0, 0).unwrap();
        assert!(fake.downloaded.borrow().is_empty());
        let report = audit_with(&fake, &data, 5, 3).unwrap();
        assert!(report.contains("spot-downloaded 1."), "{report}");
    }

    fn audit_failure(fake: &Fake, data: &Data) -> String {
        audit_with(fake, data, 1, 0).unwrap_err().to_string()
    }

    #[test]
    fn an_audit_fails_when_a_mirror_or_github_reports_another_hash() {
        let (mut fake, data) = recorded();
        let filename = fake.wheels[0].filename.clone();
        fake.mirror_disagrees.insert(filename, "ee".repeat(32));
        fake.releases.get_mut(NPU_DRIVER_REPO).unwrap()[0].assets[0].sha256 = "d2".repeat(32);
        let report = audit_failure(&fake, &data);
        assert!(
            report.contains("**2 artifact(s) no longer match"),
            "{report}"
        );
        assert!(report.contains("SOURCES DISAGREE"), "{report}");
        assert!(
            report.contains(&format!("GitHub now reports {}", "d2".repeat(32))),
            "{report}"
        );
    }

    #[test]
    fn an_audit_fails_when_a_recorded_artifact_is_gone_or_unrecognised() {
        let (mut fake, mut data) = recorded();
        fake.wheels.clear();
        fake.releases.get_mut(NPU_DRIVER_REPO).unwrap()[0]
            .assets
            .clear();
        data.ledger.list.push(ledger_entry("ftp/somewhere/x", "00"));
        let report = audit_failure(&fake, &data);
        assert!(
            report.contains("no longer on PyPI (deleted or yanked)"),
            "{report}"
        );
        assert!(
            report.contains("release asset no longer published"),
            "{report}"
        );
        assert!(
            report.contains("`ftp/somewhere/x`: unrecognised ledger id"),
            "{report}"
        );
    }

    #[test]
    fn an_audit_fails_when_a_download_serves_other_bytes() {
        let (mut fake, data) = recorded();
        let url = fake.wheels[0].url.clone();
        fake.downloads.insert(url.clone(), "bad".into());
        let report = audit_failure(&fake, &data);
        assert!(
            report.contains(&format!(
                "`{url}` served sha256 bad, recorded {}",
                "0a".repeat(32)
            )),
            "{report}"
        );
    }

    #[test]
    fn an_audit_checks_each_recipes_input_and_tag() {
        let (mut fake, mut data) = recorded();
        let pinned = Build {
            onnxruntime: "1.24.1".into(),
            openvino: "2026.4.1".into(),
            git_repo: "microsoft/onnxruntime".into(),
            git_tag: "v1.24.1".into(),
            git_commit: "c0ffee".into(),
            openvino_wheel: data.ledger.list[0].id.clone(),
            build_args: vec![],
            cmake_defines: vec![],
            note: None,
        };
        fake.tags
            .insert("microsoft/onnxruntime v1.24.1".into(), "c0ffee".into());
        data.builds.list.push(pinned.clone());
        let report = audit_with(&fake, &data, 0, 0).unwrap();
        assert!(
            report.starts_with("Audited 3 recorded artifacts"),
            "{report}"
        );

        fake.tags
            .insert("microsoft/onnxruntime v1.24.1".into(), "deadbeef".into());
        data.builds.list[0].openvino_wheel = "pypi/openvino/unrecorded.whl".into();
        let report = audit_failure(&fake, &data);
        assert!(
            report.contains("input pypi/openvino/unrecorded.whl is not in the ledger"),
            "{report}"
        );
        assert!(
            report.contains("tag now points at deadbeef, recipe pins c0ffee"),
            "{report}"
        );
    }

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
