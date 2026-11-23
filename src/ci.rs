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

fn tracked_wheels(upstream: &dyn Upstream, project: &str, min: &str) -> Result<Vec<PypiFile>> {
    let min = Ver::parse(min).context("bad minimum")?;
    let wheels = upstream.pypi_wheels(project)?;
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
    let mut unavailable = Vec::new();
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
    audit_recipes(upstream, data, &mut problems, &mut unavailable);
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
    audit_report(checked, sampled, &problems, &unavailable)
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
/// A recipe's source repository that GitHub no longer serves is reported as
/// unavailable, not as a mismatch: nothing recorded was contradicted, it just
/// cannot be compared. A tag that is gone from a repository that is still
/// there is a change to a pinned source, so it stays a problem (pinned by
/// `tests::a_recipes_repository_that_is_gone_is_unavailable_not_a_mismatch`,
/// `tests::a_recipes_tag_deleted_from_a_live_repository_is_a_problem`).
fn audit_recipes(
    upstream: &dyn Upstream,
    data: &Data,
    problems: &mut Vec<String>,
    unavailable: &mut Vec<String>,
) {
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
            Err(e) if crate::net::http_status(&e) == Some(404) => {
                match upstream.github_repo_exists(&recipe.git_repo) {
                    Ok(false) => unavailable.push(format!(
                        "- `{id}`: GitHub no longer serves {}, so the pinned commit {} cannot be fetched or compared",
                        recipe.git_repo, recipe.git_commit
                    )),
                    Ok(true) => problems.push(format!(
                        "- `{id}`: the tag no longer exists in {}, which recipe pins at {}",
                        recipe.git_repo, recipe.git_commit
                    )),
                    Err(check) => problems.push(format!("- `{id}`: {e:#}; and asking whether the repository exists failed: {check:#}")),
                }
            }
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

fn audit_report(
    checked: usize,
    sampled: usize,
    problems: &[String],
    unavailable: &[String],
) -> Result<String> {
    let mut report = format!("Audited {checked} recorded artifacts, spot-downloaded {sampled}.\n");
    if !unavailable.is_empty() {
        report.push_str(&format!(
            "\n**{} recorded source(s) unavailable.** Not a mismatch: nothing reachable contradicted the ledger, \
             but these could not be compared, and anything that builds from them cannot work until they are back.\n\n{}\n",
            unavailable.len(),
            unavailable.join("\n")
        ));
    }
    if problems.is_empty() {
        if unavailable.is_empty() {
            report.push_str("\nEvery source still agrees with the ledger.\n");
        } else {
            report.push_str("\nEvery source that could be reached still agrees with the ledger.\n");
        }
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
/// Where the kernel's `ivpu` driver lists the NPU PCI IDs it binds.
const KERNEL_REPO: &str = "torvalds/linux";
const KERNEL_BRANCH: &str = "master";
const IVPU_HEADER: &str = "drivers/accel/ivpu/ivpu_drv.h";

/// Today in UTC. Printed, a `Date` is `YYYY-MM-DD`, the form PyPI's upload
/// dates and the ledger's `first_seen` use, so dates compare as text.
fn utc_today() -> Date {
    Timestamp::now().to_zoned(TimeZone::UTC).date()
}

/// Saturates at the calendar's first day; no real cooldown reaches it.
fn days_before(date: Date, days: u32) -> Date {
    date.saturating_sub(i64::from(days).days())
}

fn record(data: &mut Data, id: String, digest: String, today: Date, changes: &mut Vec<String>) {
    if data.ledger.get(&id).is_none() {
        changes.push(format!("- ledger: `{id}` = `{digest}`"));
        data.ledger.list.push(Entry {
            id,
            digest,
            first_seen: today.to_string(),
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
    discover_with(&Network, data, utc_today())
}

/// [`discover`] against any [`Upstream`], on a given day.
pub fn discover_with(upstream: &dyn Upstream, data: &mut Data, today: Date) -> Result<String> {
    let mut changes = Vec::new();
    let held = record_wheels(upstream, data, today, &mut changes)?;
    record_npu_drivers(upstream, data, today, &mut changes)?;
    record_platforms(upstream, data, &mut changes)?;
    update_floors(data, &mut changes);
    if !changes.is_empty() && !held.is_empty() {
        changes.push(format!("\nHeld back by the {COOLDOWN_DAYS}-day cooldown:"));
        changes.extend(held);
    }
    Ok(changes.join("\n"))
}

/// Records every tracked wheel that is past the cooldown and that all
/// sources agree on. Returns a line for each wheel still inside it.
fn record_wheels(
    upstream: &dyn Upstream,
    data: &mut Data,
    today: Date,
    changes: &mut Vec<String>,
) -> Result<Vec<String>> {
    let cutoff = days_before(today, COOLDOWN_DAYS);
    let intel_releases = upstream.github_releases(INTEL_ORT_REPO)?;
    let mut held = Vec::new();
    for (project, min) in TRACKED {
        let pages = upstream.mirror_pages(project);
        let attested = upstream.pypi_provenance(project)?;
        for wheel in tracked_wheels(upstream, project, min)? {
            let id = wheel.ledger_id();
            let has_provenance = attested.contains(&wheel.filename);
            if let Some(entry) = data.ledger.list.iter_mut().find(|e| e.id == id) {
                if has_provenance && !entry.provenance {
                    entry.provenance = true;
                    changes.push(format!(
                        "- **PyPI now publishes provenance for `{id}`.** ovfetch does not verify it yet; that is the next thing to build."
                    ));
                }
                continue;
            }
            let uploaded_date = wheel.uploaded.parse::<Date>().ok();
            if uploaded_date.is_none_or(|d| d > cutoff) {
                let when = if wheel.uploaded.is_empty() {
                    "unknown"
                } else {
                    wheel.uploaded.as_str()
                };
                held.push(format!(
                    "- `{id}` (uploaded {when}), recorded once it is {COOLDOWN_DAYS} days old"
                ));
                continue;
            }
            // A new release has to clear the same agreement bar as an install.
            let (digest, _) =
                consensus::agree(&id, &sources::pypi_claims(&wheel, &pages), &data.ledger)?;
            changes.push(format!(
                "- ledger: `{id}` = `{digest}`, uploaded {}{}{}",
                wheel.uploaded,
                intel_origin(&wheel, &intel_releases),
                if has_provenance {
                    ", PyPI provenance published"
                } else {
                    ""
                }
            ));
            data.ledger.list.push(Entry {
                id,
                digest,
                first_seen: today.to_string(),
                provenance: has_provenance,
            });
        }
    }
    Ok(held)
}

/// The intel/onnxruntime release an onnxruntime-openvino wheel was built
/// from, as report text. Other projects have no such release to match.
fn intel_origin(wheel: &PypiFile, intel_releases: &[GithubRelease]) -> String {
    if wheel.project != "onnxruntime-openvino" {
        return String::new();
    }
    let needle = format!("onnxruntime {}", wheel.version);
    match intel_releases
        .iter()
        .find(|r| r.body.to_lowercase().contains(&needle))
    {
        Some(r) => format!(", Intel release [{}]({})", r.tag, r.url),
        None => format!(
            ", **no {INTEL_ORT_REPO} release mentions ONNX Runtime {}; check before merging**",
            wheel.version
        ),
    }
}

/// Records each NPU driver release whose notes name an OpenVINO, oldest
/// first, and the digest of every asset it ships.
fn record_npu_drivers(
    upstream: &dyn Upstream,
    data: &mut Data,
    today: Date,
    changes: &mut Vec<String>,
) -> Result<()> {
    let mut releases = upstream.github_releases(NPU_DRIVER_REPO)?;
    releases.sort_by_key(|r| Ver::parse(&r.tag));
    for release in &releases {
        let (platforms, openvino) = parse_driver_notes(&release.body);
        let (Some(version), Some(openvino)) = (Ver::parse(&release.tag), openvino) else {
            continue;
        };
        let known = data
            .npu_drivers
            .list
            .iter()
            .any(|d| Ver::parse(&d.version).as_ref() == Some(&version));
        if !known {
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
        for asset in &release.assets {
            record(
                data,
                format!("github/{NPU_DRIVER_REPO}/{}/{}", release.tag, asset.name),
                asset.sha256.clone(),
                today,
                changes,
            );
        }
    }
    data.npu_drivers
        .list
        .sort_by_key(|d| Ver::parse(&d.version));
    Ok(())
}

/// `(kernel name, PCI id)` for each `#define PCI_DEVICE_ID_<name> <id>` line.
fn ivpu_pci_ids(header: &str) -> impl Iterator<Item = (&str, &str)> {
    header.lines().filter_map(|line| {
        let mut words = line.split_whitespace();
        let (Some("#define"), Some(name), Some(id)) = (words.next(), words.next(), words.next())
        else {
            return None;
        };
        Some((name.strip_prefix("PCI_DEVICE_ID_")?, id))
    })
}

/// Adds every NPU PCI ID the kernel's `ivpu` driver binds and the data lacks.
fn record_platforms(
    upstream: &dyn Upstream,
    data: &mut Data,
    changes: &mut Vec<String>,
) -> Result<()> {
    let header = upstream.github_raw(KERNEL_REPO, KERNEL_BRANCH, IVPU_HEADER)?;
    for (kernel_name, id) in ivpu_pci_ids(&header) {
        if data.platforms.list.iter().any(|p| p.pci_id == id) {
            continue;
        }
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
    Ok(())
}

/// A platform's floor is the pairing of the first driver release verified on
/// it. A floor pinned by hand, or a platform with no codename, is left alone.
fn update_floors(data: &mut Data, changes: &mut Vec<String>) {
    for platform in data
        .platforms
        .list
        .iter_mut()
        .filter(|p| p.pinned_note.is_none() && !p.codename.is_empty())
    {
        let Some(first_driver) = data
            .npu_drivers
            .list
            .iter()
            .find(|d| d.platforms.contains(&platform.codename))
        else {
            continue;
        };
        let openvino = Ver::parse(&first_driver.openvino).map(|v| v.minor().to_string());
        if platform.min_openvino != openvino
            || platform.min_npu_driver.as_deref() != Some(first_driver.version.as_str())
        {
            changes.push(format!(
                "- {} floor: OpenVINO {} with NPU driver {}",
                platform.codename,
                openvino.as_deref().unwrap_or("?"),
                first_driver.version
            ));
            platform.min_openvino = openvino;
            platform.min_npu_driver = Some(first_driver.version.clone());
        }
    }
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
        let today = utc_today();
        let printed = |days| days_before(today, days).to_string();
        let (now, week_ago, year_ago) = (printed(0), printed(COOLDOWN_DAYS), printed(366));
        for d in [&now, &week_ago, &year_ago] {
            assert!(is_iso_date(d), "{d:?}");
        }
        assert_eq!(now, today.to_string());
        assert!(year_ago < week_ago && week_ago < now);
        assert_ne!(&year_ago[..4], &now[..4]);
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

    /// The recipe the audit tests share, with its tag served at its pinned commit.
    fn with_recipe() -> (Fake, Data) {
        let (mut fake, mut data) = recorded();
        data.builds.list.push(Build {
            onnxruntime: "1.24.1".into(),
            openvino: "2026.4.1".into(),
            git_repo: "microsoft/onnxruntime".into(),
            git_tag: "v1.24.1".into(),
            git_commit: "c0ffee".into(),
            openvino_wheel: data.ledger.list[0].id.clone(),
            build_args: vec![],
            cmake_defines: vec![],
            note: None,
        });
        fake.tags
            .insert("microsoft/onnxruntime v1.24.1".into(), "c0ffee".into());
        (fake, data)
    }

    #[test]
    fn a_recipes_repository_that_is_gone_is_unavailable_not_a_mismatch() {
        let (mut fake, data) = with_recipe();
        fake.tags.clear();
        fake.repos_gone.insert("microsoft/onnxruntime".into());
        let report = audit_with(&fake, &data, 0, 0).unwrap();
        assert!(
            report.contains("**1 recorded source(s) unavailable.**"),
            "{report}"
        );
        assert!(
            report.contains("GitHub no longer serves microsoft/onnxruntime"),
            "{report}"
        );
        assert!(!report.contains("no longer match"), "{report}");
        assert!(!report.contains("compromise"), "{report}");
        assert!(
            report.contains("Every source that could be reached still agrees"),
            "{report}"
        );
    }

    #[test]
    fn a_recipes_tag_deleted_from_a_live_repository_is_a_problem() {
        let (mut fake, data) = with_recipe();
        fake.tags.clear();
        let report = audit_failure(&fake, &data);
        assert!(
            report.contains("**1 artifact(s) no longer match"),
            "{report}"
        );
        assert!(
            report.contains("the tag no longer exists in microsoft/onnxruntime"),
            "{report}"
        );
        assert!(!report.contains("unavailable"), "{report}");
    }

    #[test]
    fn a_real_mismatch_still_fails_beside_an_unavailable_source() {
        let (mut fake, data) = with_recipe();
        fake.tags.clear();
        fake.repos_gone.insert("microsoft/onnxruntime".into());
        let filename = fake.wheels[0].filename.clone();
        fake.mirror_disagrees.insert(filename, "ee".repeat(32));
        let report = audit_failure(&fake, &data);
        assert!(
            report.contains("**1 artifact(s) no longer match"),
            "{report}"
        );
        assert!(report.contains("SOURCES DISAGREE"), "{report}");
        assert!(
            report.contains("**1 recorded source(s) unavailable.**"),
            "{report}"
        );
    }

    const ORT: &str = "onnxruntime-openvino";

    fn today() -> Date {
        jiff::civil::date(2026, 11, 15)
    }

    fn uploaded(mut wheel: PypiFile, day: &str) -> PypiFile {
        wheel.uploaded = day.into();
        wheel
    }

    fn platform(pci_id: &str, kernel_name: &str, codename: &str) -> Platform {
        Platform {
            pci_id: pci_id.into(),
            kernel_name: kernel_name.into(),
            codename: codename.into(),
            min_openvino: None,
            min_npu_driver: None,
            pinned_note: None,
        }
    }

    fn driver_release(
        tag: &str,
        platforms: &[&str],
        openvino: &str,
        asset_seed: &str,
    ) -> GithubRelease {
        let rows: String = platforms
            .iter()
            .map(|p| format!("|{p}|Ubuntu|7.0|\n"))
            .collect();
        GithubRelease {
            tag: tag.into(),
            url: String::new(),
            body: format!(
                "|Platform|System|Kernel|\n|:---:|:---:|:---:|\n{rows}\n\
                 |Component|Version|\n|:---|:---|\n|OpenVINO|{openvino}|\n"
            ),
            assets: vec![GithubAsset {
                name: format!("linux-npu-driver-{tag}-ubuntu2404.tar.gz"),
                sha256: asset_seed.repeat(32),
            }],
        }
    }

    /// An upstream with nothing new, and the data that already records it:
    /// one openvino wheel, driver 1.32.0 and its asset, and Wildcat Lake with
    /// a hand-pinned floor.
    fn settled() -> (Fake, Data) {
        let ov = wheel("openvino", "2026.4.1", "cp313", "0a");
        let mut fake = Fake::with_wheels(vec![ov.clone()]);
        fake.releases.insert(INTEL_ORT_REPO.into(), vec![]);
        fake.releases.insert(
            NPU_DRIVER_REPO.into(),
            vec![driver_release(
                "v1.32.0",
                &["Wildcat Lake"],
                "2026.0.0",
                "32",
            )],
        );
        fake.raw_files.insert(
            format!("{KERNEL_REPO}@{KERNEL_BRANCH}:{IVPU_HEADER}"),
            "#define PCI_DEVICE_ID_WCL 0xfd3e\n#define IVPU_HW_IP_37XX 37\n".into(),
        );
        let data = Data {
            dir: None,
            platforms: Platforms {
                list: vec![Platform {
                    min_openvino: Some("2025.4".into()),
                    min_npu_driver: Some("1.32.0".into()),
                    pinned_note: Some("measured by hand".into()),
                    ..platform("0xfd3e", "WCL", "Wildcat Lake")
                }],
            },
            npu_drivers: NpuDrivers {
                list: vec![NpuDriver {
                    version: "1.32.0".into(),
                    openvino: "2026.0.0".into(),
                    platforms: vec!["Wildcat Lake".into()],
                    measured_openvino: Some("2026.4".into()),
                    measured_note: None,
                }],
            },
            ledger: Ledger {
                list: vec![
                    ledger_entry(&ov.ledger_id(), &ov.sha256),
                    ledger_entry(
                        "github/intel/linux-npu-driver/v1.32.0/linux-npu-driver-v1.32.0-ubuntu2404.tar.gz",
                        &"32".repeat(32),
                    ),
                ],
            },
            builds: Builds::default(),
        };
        (fake, data)
    }

    #[test]
    fn discover_reports_nothing_when_nothing_is_new() {
        let (fake, mut data) = settled();
        assert_eq!(discover_with(&fake, &mut data, today()).unwrap(), "");
        assert_eq!(data.ledger.list.len(), 2);
        assert_eq!(
            data.platforms.list[0].min_openvino.as_deref(),
            Some("2025.4")
        );
        assert_eq!(
            data.npu_drivers.list[0].measured_openvino.as_deref(),
            Some("2026.4")
        );
    }

    #[test]
    fn discover_records_wheels_past_the_cooldown_and_holds_younger_ones() {
        let (mut fake, mut data) = settled();
        let at_cutoff = uploaded(wheel(ORT, "1.24.1", "cp313", "c2"), "2026-11-08");
        let too_young = uploaded(wheel(ORT, "1.25.0", "cp313", "c3"), "2026-11-09");
        let unmatched = wheel(ORT, "1.23.0", "cp313", "c1");
        let untracked = wheel(ORT, "1.21.0", "cp313", "c0");
        let new_openvino = wheel("openvino", "2026.5.0", "cp313", "0b");
        let old_openvino = wheel("openvino", "2025.3.0", "cp313", "09");
        fake.provenance = vec![new_openvino.filename.clone()];
        fake.releases.insert(
            INTEL_ORT_REPO.into(),
            vec![GithubRelease {
                tag: "v5.9".into(),
                url: "https://github.com/intel/onnxruntime/releases/tag/v5.9".into(),
                body: "Release for OnnxRuntime 1.24.1 and OpenVINO 2025.4.1".into(),
                assets: vec![],
            }],
        );
        // Newest first, as PyPI lists them.
        fake.wheels = vec![
            too_young.clone(),
            at_cutoff.clone(),
            unmatched.clone(),
            untracked.clone(),
            new_openvino.clone(),
            fake.wheels[0].clone(),
            old_openvino.clone(),
        ];
        let changes = discover_with(&fake, &mut data, today()).unwrap();
        assert_eq!(
            changes,
            format!(
                "- ledger: `{}` = `{}`, uploaded 2026-11-08, Intel release [v5.9](https://github.com/intel/onnxruntime/releases/tag/v5.9)\n\
                 - ledger: `{}` = `{}`, uploaded 2026-01-01, **no intel/onnxruntime release mentions ONNX Runtime 1.23.0; check before merging**\n\
                 - ledger: `{}` = `{}`, uploaded 2026-01-01, PyPI provenance published\n\
                 \nHeld back by the 7-day cooldown:\n\
                 - `{}` (uploaded 2026-11-09), recorded once it is 7 days old",
                at_cutoff.ledger_id(),
                at_cutoff.sha256,
                unmatched.ledger_id(),
                unmatched.sha256,
                new_openvino.ledger_id(),
                new_openvino.sha256,
                too_young.ledger_id(),
            )
        );
        let recorded = |w: &PypiFile| data.ledger.get(&w.ledger_id());
        assert_eq!(recorded(&at_cutoff).unwrap().first_seen, "2026-11-15");
        assert!(recorded(&new_openvino).unwrap().provenance);
        assert!(!recorded(&at_cutoff).unwrap().provenance);
        for absent in [&too_young, &untracked, &old_openvino] {
            assert!(recorded(absent).is_none(), "{}", absent.filename);
        }
    }

    #[test]
    fn a_held_wheel_alone_is_not_a_change() {
        let (mut fake, mut data) = settled();
        fake.wheels.insert(
            0,
            uploaded(wheel("openvino", "2026.5.0", "cp313", "0b"), "2026-11-14"),
        );
        assert_eq!(discover_with(&fake, &mut data, today()).unwrap(), "");
    }

    #[test]
    fn an_empty_or_malformed_upload_date_is_held_back() {
        for bad_date in ["", "not-a-date", "2026-99-99"] {
            let (mut fake, mut data) = settled();
            let w = uploaded(wheel("openvino", "2026.5.0", "cp313", "0b"), bad_date);
            fake.wheels.insert(0, w.clone());
            assert_eq!(discover_with(&fake, &mut data, today()).unwrap(), "");
            assert!(data.ledger.get(&w.ledger_id()).is_none(), "{bad_date:?}");
        }
    }

    #[test]
    fn discover_flags_provenance_that_appears_on_a_recorded_wheel() {
        let (mut fake, mut data) = settled();
        fake.provenance = vec![fake.wheels[0].filename.clone()];
        let changes = discover_with(&fake, &mut data, today()).unwrap();
        assert!(
            changes.starts_with("- **PyPI now publishes provenance for `pypi/openvino/"),
            "{changes}"
        );
        assert!(data.ledger.list[0].provenance);
        assert_eq!(discover_with(&fake, &mut data, today()).unwrap(), "");
    }

    #[test]
    fn discover_refuses_a_new_wheel_the_mirrors_disagree_on() {
        let (mut fake, mut data) = settled();
        let new = wheel("openvino", "2026.5.0", "cp313", "0b");
        fake.mirror_disagrees
            .insert(new.filename.clone(), "ee".repeat(32));
        fake.wheels.insert(0, new);
        let err = discover_with(&fake, &mut data, today()).unwrap_err();
        assert!(err.to_string().contains("SOURCES DISAGREE"), "{err}");
    }

    #[test]
    fn discover_records_driver_releases_platforms_and_floors() {
        let (mut fake, mut data) = settled();
        data.platforms
            .list
            .push(platform("0x643e", "LNL", "Lunar Lake"));
        fake.releases.get_mut(NPU_DRIVER_REPO).unwrap().extend([
            driver_release("v1.38.0", &["Lunar Lake", "Nova Lake"], "2026.3.1", "38"),
            driver_release("v1.35.0", &["Lunar Lake", "Wildcat Lake"], "2026.2.0", "35"),
            driver_release("nightly", &["Lunar Lake"], "2026.9.0", "99"),
        ]);
        fake.raw_files.insert(
            format!("{KERNEL_REPO}@{KERNEL_BRANCH}:{IVPU_HEADER}"),
            "#define PCI_DEVICE_ID_WCL 0xfd3e\n#define PCI_DEVICE_ID_LNL 0x643e\n\
             #define PCI_DEVICE_ID_NVL 0xd71d\n#define PCI_DEVICE_ID_ZZZ 0x1234\n#define IVPU_X 1\n"
                .into(),
        );
        let changes = discover_with(&fake, &mut data, today()).unwrap();
        assert_eq!(
            changes,
            format!(
                "- NPU driver 1.35.0: OpenVINO 2026.2.0, verified on Lunar Lake, Wildcat Lake\n\
                 - ledger: `github/intel/linux-npu-driver/v1.35.0/linux-npu-driver-v1.35.0-ubuntu2404.tar.gz` = `{}`\n\
                 - NPU driver 1.38.0: OpenVINO 2026.3.1, verified on Lunar Lake, Nova Lake\n\
                 - ledger: `github/intel/linux-npu-driver/v1.38.0/linux-npu-driver-v1.38.0-ubuntu2404.tar.gz` = `{}`\n\
                 - new NPU platform `0xd71d` (NVL) = Nova Lake\n\
                 - new NPU platform `0x1234` (ZZZ): **codename unknown, fill it in before merging**\n\
                 - Lunar Lake floor: OpenVINO 2026.2 with NPU driver 1.35.0\n\
                 - Nova Lake floor: OpenVINO 2026.3 with NPU driver 1.38.0",
                "35".repeat(32),
                "38".repeat(32),
            )
        );
        let versions: Vec<&str> = data
            .npu_drivers
            .list
            .iter()
            .map(|d| d.version.as_str())
            .collect();
        assert_eq!(versions, ["1.32.0", "1.35.0", "1.38.0"]);
        let floor = |pci_id: &str| {
            let p = data
                .platforms
                .list
                .iter()
                .find(|p| p.pci_id == pci_id)
                .unwrap();
            (p.min_openvino.clone(), p.min_npu_driver.clone())
        };
        assert_eq!(
            floor("0xfd3e"),
            (Some("2025.4".into()), Some("1.32.0".into()))
        );
        assert_eq!(
            floor("0x643e"),
            (Some("2026.2".into()), Some("1.35.0".into()))
        );
        assert_eq!(floor("0x1234"), (None, None));
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
