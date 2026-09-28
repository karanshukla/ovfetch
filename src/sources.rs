//! Where artifacts come from, and every independent place that states their hash.

use crate::net;
use crate::version::Ver;
use anyhow::{Context, Result};

/// PyPI plus independently operated mirrors. A mirror that disagrees with the
/// rest is the signal; one that is down or behind is not.
pub const PYPI_SIMPLE: &[&str] = &[
    "https://pypi.org/simple",
    "https://pypi.tuna.tsinghua.edu.cn/simple",
    "https://mirrors.aliyun.com/pypi/simple",
    "https://mirror.sjtu.edu.cn/pypi/web/simple",
    "https://mirrors.cloud.tencent.com/pypi/simple",
    "https://pypi.mirrors.ustc.edu.cn/simple",
];

#[derive(Debug, Clone)]
pub struct PypiFile {
    pub project: String,
    pub version: String,
    pub filename: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

impl PypiFile {
    pub fn ledger_id(&self) -> String {
        format!("pypi/{}/{}", self.project, self.filename)
    }
}

/// Every non-yanked Linux x86_64 wheel for `project`, newest release first.
pub fn pypi_wheels(project: &str) -> Result<Vec<PypiFile>> {
    let json = net::get_json(&format!("https://pypi.org/pypi/{project}/json"))?;
    let releases = json["releases"]
        .as_object()
        .context("PyPI response has no releases")?;
    let mut out = Vec::new();
    for (version, files) in releases {
        if Ver::parse(version).is_none() {
            continue;
        }
        for f in files.as_array().into_iter().flatten() {
            let filename = f["filename"].as_str().unwrap_or_default();
            let linux = filename.contains("manylinux") && filename.ends_with("x86_64.whl");
            if !linux || f["yanked"].as_bool().unwrap_or(false) {
                continue;
            }
            out.push(PypiFile {
                project: project.to_owned(),
                version: version.clone(),
                filename: filename.to_owned(),
                url: f["url"].as_str().unwrap_or_default().to_owned(),
                size: f["size"].as_u64().unwrap_or(0),
                sha256: f["digests"]["sha256"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            });
        }
    }
    out.sort_by(|a, b| {
        Ver::parse(&b.version)
            .cmp(&Ver::parse(&a.version))
            .then(b.filename.cmp(&a.filename))
    });
    Ok(out)
}

/// Highest CPython tag among one release's wheels. Per-tag wheels are separate
/// builds, so one tag is chosen consistently rather than mixing.
pub fn pick_wheel(wheels: &[PypiFile], version: &str) -> Option<PypiFile> {
    wheels
        .iter()
        .filter(|w| w.version == version)
        .max_by_key(|w| cp_tag(&w.filename))
        .cloned()
}

fn cp_tag(filename: &str) -> u32 {
    filename
        .split('-')
        .find_map(|p| p.strip_prefix("cp").and_then(|n| n.parse().ok()))
        .unwrap_or(0)
}

/// OpenVINO version bundled in an onnxruntime-openvino wheel, read from the
/// zip index alone (`libopenvino.so.2025.4.1`).
pub fn bundled_openvino(wheel: &PypiFile) -> Result<Option<Ver>> {
    let names = net::remote_zip_names(&wheel.url, wheel.size)?;
    let suffixes: Vec<&str> = names
        .iter()
        .filter_map(|n| n.rsplit('/').next()?.strip_prefix("libopenvino.so."))
        .collect();
    Ok(suffixes
        .iter()
        .find(|s| s.matches('.').count() == 2)
        .and_then(|s| Ver::parse(s))
        .or_else(|| suffixes.iter().find_map(|s| soname_version(s))))
}

/// Wheels before 1.24 carry only the soname: `2530` is 2025.3.0.
fn soname_version(soname: &str) -> Option<Ver> {
    let d: Vec<u64> = soname
        .chars()
        .map(|c| c.to_digit(10).map(u64::from))
        .collect::<Option<_>>()?;
    match d.as_slice() {
        [y1, y2, minor, patch] => Some(Ver(vec![2000 + y1 * 10 + y2, *minor, *patch])),
        _ => None,
    }
}

#[derive(Debug)]
pub struct Claim {
    pub source: String,
    pub digest: Option<String>,
    pub error: Option<String>,
}

/// Each mirror's simple-index page for a project, fetched once and reused for
/// every file in it.
pub type MirrorPages = Vec<(&'static str, Result<String, String>)>;

pub fn mirror_pages(project: &str) -> MirrorPages {
    PYPI_SIMPLE
        .iter()
        .map(|base| {
            (
                *base,
                net::get_text(&format!("{base}/{project}/")).map_err(|e| format!("{e:#}")),
            )
        })
        .collect()
}

/// PyPI's JSON API plus what every simple-index mirror says `file`'s sha256 is.
pub fn pypi_claims(file: &PypiFile, pages: &MirrorPages) -> Vec<Claim> {
    let mut claims = vec![Claim {
        source: "pypi.org/pypi JSON".into(),
        digest: Some(file.sha256.clone()),
        error: None,
    }];
    let marker = format!("{}#sha256=", file.filename);
    for (base, page) in pages {
        let (digest, error) = match page {
            Ok(page) => match page.find(&marker) {
                Some(i) => (
                    Some(page[i + marker.len()..].chars().take(64).collect()),
                    None,
                ),
                None => (None, Some("file not listed (mirror behind?)".to_owned())),
            },
            Err(e) => (None, Some(e.clone())),
        };
        claims.push(Claim {
            source: base.to_string(),
            digest,
            error,
        });
    }
    claims
}

/// Download URLs for `file` on each mirror that listed it, for spreading
/// downloads across sources. Relative hrefs are resolved against the page.
pub fn pypi_download_urls(file: &PypiFile, pages: &MirrorPages) -> Vec<String> {
    let mut urls: Vec<String> = Some(file.url.clone())
        .filter(|u| !u.is_empty())
        .into_iter()
        .collect();
    for (base, page) in pages {
        let page_url = format!("{base}/{}/", file.project);
        let Ok(page) = page else { continue };
        let Some(end) = page.find(&format!("{}#sha256=", file.filename)) else {
            continue;
        };
        let Some(start) = page[..end].rfind("href=\"") else {
            continue;
        };
        let href = &page[start + 6..end + file.filename.len()];
        let url = resolve_href(&page_url, href);
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    urls
}

fn resolve_href(page: &str, href: &str) -> String {
    if href.starts_with("http") {
        return href.to_owned();
    }
    if let Some(rest) = href.strip_prefix('/') {
        let origin: String = page.splitn(4, '/').take(3).collect::<Vec<_>>().join("/");
        return format!("{origin}/{rest}");
    }
    let mut base: Vec<&str> = page.trim_end_matches('/').split('/').collect();
    for part in href.split('/') {
        match part {
            ".." => {
                base.pop();
            }
            "." => {}
            p => base.push(p),
        }
    }
    base.join("/")
}

#[derive(Debug, Clone)]
pub struct GithubAsset {
    pub name: String,
    pub sha256: String,
}

#[derive(Debug, Clone)]
pub struct GithubRelease {
    pub tag: String,
    pub body: String,
    pub assets: Vec<GithubAsset>,
}

pub fn github_releases(repo: &str) -> Result<Vec<GithubRelease>> {
    let mut out = Vec::new();
    for page in 1..=5 {
        let json = net::get_json(&format!(
            "https://api.github.com/repos/{repo}/releases?per_page=100&page={page}"
        ))?;
        let list = json.as_array().context("releases is not a list")?;
        if list.is_empty() {
            break;
        }
        for r in list {
            if r["draft"].as_bool().unwrap_or(false) || r["prerelease"].as_bool().unwrap_or(false) {
                continue;
            }
            out.push(GithubRelease {
                tag: r["tag_name"].as_str().unwrap_or_default().to_owned(),
                body: r["body"].as_str().unwrap_or_default().to_owned(),
                assets: r["assets"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|a| {
                        Some(GithubAsset {
                            name: a["name"].as_str()?.to_owned(),
                            sha256: a["digest"].as_str()?.strip_prefix("sha256:")?.to_owned(),
                        })
                    })
                    .collect(),
            });
        }
    }
    Ok(out)
}

pub fn github_raw(repo: &str, git_ref: &str, path: &str) -> Result<String> {
    net::get_text(&format!(
        "https://raw.githubusercontent.com/{repo}/{git_ref}/{path}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_mirror_hrefs() {
        assert_eq!(
            resolve_href(
                "https://m.example/pypi/simple/pkg/",
                "../../packages/ab/cd/f.whl"
            ),
            "https://m.example/pypi/packages/ab/cd/f.whl"
        );
        assert_eq!(
            resolve_href("https://m.example/simple/pkg/", "/files/f.whl"),
            "https://m.example/files/f.whl"
        );
    }

    #[test]
    fn reads_openvino_from_a_bare_soname() {
        assert_eq!(soname_version("2530").unwrap().to_string(), "2025.3.0");
        assert_eq!(soname_version("2621").unwrap().to_string(), "2026.2.1");
        assert!(soname_version("12").is_none());
    }

    #[test]
    fn picks_the_highest_cpython_tag() {
        let w = |f: &str| PypiFile {
            project: "p".into(),
            version: "1.0".into(),
            filename: f.into(),
            url: String::new(),
            size: 0,
            sha256: String::new(),
        };
        let wheels = [
            w("p-1.0-cp311-cp311-manylinux_2_28_x86_64.whl"),
            w("p-1.0-cp313-cp313-manylinux_2_28_x86_64.whl"),
        ];
        assert!(
            pick_wheel(&wheels, "1.0")
                .unwrap()
                .filename
                .contains("cp313")
        );
    }
}
