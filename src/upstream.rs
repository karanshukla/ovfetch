//! Everything ovfetch asks the network, behind one trait, so resolving,
//! auditing and discovering can run against a fake in tests.

use crate::net;
use crate::sources::{self, GithubRelease, MirrorPages, PypiFile};
use crate::version::Ver;
use anyhow::Result;
use std::path::Path;

pub trait Upstream {
    /// Every non-yanked Linux x86_64 wheel for `project`, newest release first.
    fn pypi_wheels(&self, project: &str) -> Result<Vec<PypiFile>>;
    /// Each mirror's simple-index page for `project`.
    fn mirror_pages(&self, project: &str) -> MirrorPages;
    /// Filenames PyPI reports a provenance attestation for.
    fn pypi_provenance(&self, project: &str) -> Result<Vec<String>>;
    /// The OpenVINO an onnxruntime-openvino wheel bundles.
    fn bundled_openvino(&self, wheel: &PypiFile) -> Result<Option<Ver>>;
    fn github_releases(&self, repo: &str) -> Result<Vec<GithubRelease>>;
    fn github_release(&self, repo: &str, tag: &str) -> Result<GithubRelease>;
    fn github_raw(&self, repo: &str, git_ref: &str, path: &str) -> Result<String>;
    fn github_tag_commit(&self, repo: &str, tag: &str) -> Result<String>;
    /// False when GitHub no longer serves the repository itself.
    fn github_repo_exists(&self, repo: &str) -> Result<bool>;
    /// Writes `url` to `dest` and returns the sha256 of what it wrote.
    fn download(&self, url: &str, dest: &Path) -> Result<String>;
}

/// PyPI, its mirrors and GitHub, over HTTPS to the host allowlist.
pub struct Network;

impl Upstream for Network {
    fn pypi_wheels(&self, project: &str) -> Result<Vec<PypiFile>> {
        sources::pypi_wheels(project)
    }

    fn mirror_pages(&self, project: &str) -> MirrorPages {
        sources::mirror_pages(project)
    }

    fn pypi_provenance(&self, project: &str) -> Result<Vec<String>> {
        sources::pypi_provenance(project)
    }

    fn bundled_openvino(&self, wheel: &PypiFile) -> Result<Option<Ver>> {
        sources::bundled_openvino(wheel)
    }

    fn github_releases(&self, repo: &str) -> Result<Vec<GithubRelease>> {
        sources::github_releases(repo)
    }

    fn github_release(&self, repo: &str, tag: &str) -> Result<GithubRelease> {
        sources::github_release(repo, tag)
    }

    fn github_raw(&self, repo: &str, git_ref: &str, path: &str) -> Result<String> {
        sources::github_raw(repo, git_ref, path)
    }

    fn github_tag_commit(&self, repo: &str, tag: &str) -> Result<String> {
        sources::github_tag_commit(repo, tag)
    }

    fn github_repo_exists(&self, repo: &str) -> Result<bool> {
        sources::github_repo_exists(repo)
    }

    fn download(&self, url: &str, dest: &Path) -> Result<String> {
        net::download(url, dest)
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use anyhow::{Context, bail};
    use std::cell::RefCell;
    use std::collections::HashMap;

    const MIRRORS: [&str; 2] = [
        "https://pypi.org/simple",
        "https://mirrors.aliyun.com/pypi/simple",
    ];

    /// An upstream that answers from what a test put in it, and fails for
    /// anything it was not given.
    #[derive(Default)]
    pub(crate) struct Fake {
        pub(crate) wheels: Vec<PypiFile>,
        /// Wheel filename to the OpenVINO it bundles.
        pub(crate) bundled: HashMap<String, String>,
        /// Wheel filename to the digest the last mirror lists instead of PyPI's.
        pub(crate) mirror_disagrees: HashMap<String, String>,
        pub(crate) provenance: Vec<String>,
        pub(crate) releases: HashMap<String, Vec<GithubRelease>>,
        pub(crate) raw_files: HashMap<String, String>,
        /// `repo tag` to the commit it points at.
        pub(crate) tags: HashMap<String, String>,
        /// Repositories GitHub no longer serves.
        pub(crate) repos_gone: std::collections::HashSet<String>,
        /// URL to the sha256 a download of it yields.
        pub(crate) downloads: HashMap<String, String>,
        pub(crate) downloaded: RefCell<Vec<String>>,
    }

    /// A wheel as PyPI's JSON would describe it. `seed` is two hex digits that
    /// stand for its whole sha256.
    pub(crate) fn wheel(project: &str, version: &str, cp: &str, seed: &str) -> PypiFile {
        let filename = format!(
            "{}-{version}-{cp}-{cp}-manylinux_2_28_x86_64.whl",
            project.replace('-', "_")
        );
        PypiFile {
            project: project.into(),
            version: version.into(),
            url: format!("https://files.pythonhosted.org/packages/{filename}"),
            filename,
            size: 1,
            sha256: seed.repeat(32),
            uploaded: "2026-01-01".into(),
        }
    }

    impl Fake {
        pub(crate) fn with_wheels(wheels: Vec<PypiFile>) -> Self {
            Fake {
                wheels,
                ..Fake::default()
            }
        }

        /// Adds an onnxruntime-openvino wheel that bundles `openvino`.
        pub(crate) fn bundling(mut self, wheel: PypiFile, openvino: &str) -> Self {
            self.bundled.insert(wheel.filename.clone(), openvino.into());
            self.wheels.push(wheel);
            self
        }
    }

    impl Upstream for Fake {
        fn pypi_wheels(&self, project: &str) -> Result<Vec<PypiFile>> {
            Ok(self
                .wheels
                .iter()
                .filter(|w| w.project == project)
                .cloned()
                .collect())
        }

        fn mirror_pages(&self, project: &str) -> MirrorPages {
            MIRRORS
                .iter()
                .enumerate()
                .map(|(i, base)| {
                    let last = i == MIRRORS.len() - 1;
                    let page: String = self
                        .wheels
                        .iter()
                        .filter(|w| w.project == project)
                        .map(|w| {
                            let digest = self
                                .mirror_disagrees
                                .get(&w.filename)
                                .filter(|_| last)
                                .unwrap_or(&w.sha256);
                            format!(
                                "<a href=\"../../packages/{0}#sha256={digest}\">{0}</a>\n",
                                w.filename
                            )
                        })
                        .collect();
                    (*base, Ok(page))
                })
                .collect()
        }

        fn pypi_provenance(&self, _project: &str) -> Result<Vec<String>> {
            Ok(self.provenance.clone())
        }

        fn bundled_openvino(&self, wheel: &PypiFile) -> Result<Option<Ver>> {
            Ok(self
                .bundled
                .get(&wheel.filename)
                .and_then(|v| Ver::parse(v)))
        }

        fn github_releases(&self, repo: &str) -> Result<Vec<GithubRelease>> {
            self.releases
                .get(repo)
                .cloned()
                .with_context(|| format!("no fake releases for {repo}"))
        }

        fn github_release(&self, repo: &str, tag: &str) -> Result<GithubRelease> {
            self.github_releases(repo)?
                .into_iter()
                .find(|r| r.tag == tag)
                .with_context(|| format!("no fake release {repo} {tag}"))
        }

        fn github_raw(&self, repo: &str, git_ref: &str, path: &str) -> Result<String> {
            self.raw_files
                .get(&format!("{repo}@{git_ref}:{path}"))
                .cloned()
                .with_context(|| format!("no fake file {repo}@{git_ref}:{path}"))
        }

        fn github_tag_commit(&self, repo: &str, tag: &str) -> Result<String> {
            match self.tags.get(&format!("{repo} {tag}")) {
                Some(commit) => Ok(commit.clone()),
                None => Err(crate::net::HttpStatus {
                    url: format!("https://api.github.com/repos/{repo}/git/ref/tags/{tag}"),
                    code: 404,
                    text: "404 Not Found".into(),
                }
                .into()),
            }
        }

        fn github_repo_exists(&self, repo: &str) -> Result<bool> {
            Ok(!self.repos_gone.contains(repo))
        }

        fn download(&self, url: &str, _dest: &Path) -> Result<String> {
            self.downloaded.borrow_mut().push(url.to_owned());
            match self.downloads.get(url) {
                Some(digest) => Ok(digest.clone()),
                None => bail!("no fake download for {url}"),
            }
        }
    }
}
