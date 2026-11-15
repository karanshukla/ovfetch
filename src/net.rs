use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;
use url::Url;

/// Every host ovfetch will talk to. A URL from an API response (PyPI's JSON,
/// a mirror's href) is only data: if it names any other host, or a redirect
/// lands anywhere else, the request is refused before bytes are read.
const ALLOWED_HOSTS: &[&str] = &[
    "pypi.org",
    "files.pythonhosted.org",
    "pypi.tuna.tsinghua.edu.cn",
    "mirrors.aliyun.com",
    "api.github.com",
    "raw.githubusercontent.com",
    // Release assets: github.com redirects each download to this host.
    "github.com",
    "release-assets.githubusercontent.com",
];

/// Rejects anything but `https://<allowed host>/...`: no plain HTTP, no
/// userinfo, no explicit port, no host outside the list.
pub fn check_url(url: &str) -> Result<()> {
    allowed_host(url).map(|_| ())
}

/// The allowed host `url` names, or why it is refused.
fn allowed_host(url: &str) -> Result<String> {
    let parsed = Url::parse(url).with_context(|| format!("refusing {url}: not a URL"))?;
    if parsed.scheme() != "https" {
        bail!("refusing non-HTTPS URL {url}");
    }
    let host = parsed.host_str().unwrap_or_default();
    let has_userinfo = !parsed.username().is_empty() || parsed.password().is_some();
    if has_userinfo || parsed.port().is_some() || !ALLOWED_HOSTS.contains(&host) {
        bail!("refusing {url}: {host} is not an allowed host");
    }
    if !is_written_as_parsed(url, &parsed) {
        bail!("refusing {url}: it does not spell {host} the way it parses");
    }
    Ok(host.to_owned())
}

/// The request is sent from the string, not from `parsed`, so the two must
/// name the same origin: `HTTPS://`, `:443`, a backslash or a tab all parse
/// to an allowed host and are refused here.
fn is_written_as_parsed(url: &str, parsed: &Url) -> bool {
    let origin = &parsed[..url::Position::BeforePath];
    url.strip_prefix(origin)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '?', '#']))
}

const MAX_REDIRECTS: usize = 5;
/// Cap on API and index pages held in memory; the largest (a mirror's
/// simple-index page) is under 1 MiB.
const MAX_PAGE: u64 = 64 << 20;
/// Cap on a downloaded file; the largest onnxruntime-openvino wheel is ~80 MiB.
const MAX_DOWNLOAD: u64 = 512 << 20;
/// ureq's body timeout is one budget for the whole body, not per read: 20
/// minutes still fits the largest wheel at ~70 KiB/s.
const RECV_BODY: Duration = Duration::from_secs(20 * 60);

fn agent() -> ureq::Agent {
    // TLS is rustls with Mozilla's roots compiled in, so a spoofed DNS answer
    // still has to present a valid certificate for the allowed host.
    // Redirects are followed by hand so every hop is checked before any
    // connection to it is made.
    ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .http_status_as_error(false)
        .timeout_resolve(Some(Duration::from_secs(10)))
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .timeout_recv_body(Some(RECV_BODY))
        .user_agent(concat!("ovfetch/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// GET with the allowlist enforced on the URL and on every redirect target.
/// `Authorization` is only sent to the host it was meant for.
fn get(url: &str, headers: &[(&str, &str)]) -> Result<ureq::http::Response<ureq::Body>> {
    let agent = agent();
    let first_host = allowed_host(url)?;
    let mut url = url.to_owned();
    for _ in 0..=MAX_REDIRECTS {
        let same_host = allowed_host(&url)? == first_host;
        let mut req = agent.get(&url);
        for (k, v) in headers {
            if same_host || !k.eq_ignore_ascii_case("authorization") {
                req = req.header(*k, *v);
            }
        }
        let resp = req.call().with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if status.is_redirection() {
            let location = resp
                .headers()
                .get("location")
                .and_then(|l| l.to_str().ok())
                .with_context(|| format!("{url} redirected without a Location"))?;
            url = resolve_location(&url, location)?;
            continue;
        }
        if !status.is_success() {
            bail!("GET {url}: HTTP {status}");
        }
        return Ok(resp);
    }
    bail!("more than {MAX_REDIRECTS} redirects from {url}")
}

/// Where a `Location` header points. An absolute or scheme-relative one is
/// kept as written, so `check_url` judges exactly what the server sent; a
/// path is resolved against `from`.
fn resolve_location(from: &str, location: &str) -> Result<String> {
    if location.contains("://") {
        return Ok(location.to_owned());
    }
    if let Some(host_and_path) = location.strip_prefix("//") {
        return Ok(format!("https://{host_and_path}"));
    }
    let resolved = Url::parse(from)
        .and_then(|from| from.join(location))
        .with_context(|| format!("{from} redirected to {location:?}, which is not a URL"))?;
    Ok(resolved.into())
}

pub fn get_json_accept(url: &str, accept: &str) -> Result<serde_json::Value> {
    let text = get(url, &[("Accept", accept)])?
        .body_mut()
        .with_config()
        .limit(MAX_PAGE)
        .read_to_string()
        .with_context(|| format!("reading {url}"))?;
    serde_json::from_str(&text).with_context(|| format!("parsing JSON from {url}"))
}

pub fn get_text(url: &str) -> Result<String> {
    let token = std::env::var("GITHUB_TOKEN")
        .ok()
        .filter(|_| url.starts_with("https://api.github.com/"));
    let auth = token.map(|t| format!("Bearer {t}"));
    let headers: Vec<(&str, &str)> = auth.iter().map(|a| ("Authorization", a.as_str())).collect();
    get(url, &headers)?
        .body_mut()
        .with_config()
        .limit(MAX_PAGE)
        .read_to_string()
        .with_context(|| format!("reading {url}"))
}

pub fn get_json(url: &str) -> Result<serde_json::Value> {
    serde_json::from_str(&get_text(url)?).with_context(|| format!("parsing JSON from {url}"))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Streams `url` to `dest`, hashing as it goes. Returns the hex sha256.
pub fn download(url: &str, dest: &Path) -> Result<String> {
    eprintln!("downloading {url}");
    let mut reader = get(url, &[])?
        .into_body()
        .into_with_config()
        .limit(MAX_DOWNLOAD)
        .reader();
    let mut file = std::fs::File::create(dest)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
    }
    Ok(hex::encode(hasher.finalize()))
}

/// A remote file read through HTTP range requests, so a wheel's zip index can
/// be listed without downloading the wheel.
pub struct RangeReader {
    url: String,
    size: u64,
    pos: u64,
}

impl RangeReader {
    pub fn new(url: &str, size: u64) -> Self {
        Self {
            url: url.to_owned(),
            size,
            pos: 0,
        }
    }
}

impl Read for RangeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = (buf.len() as u64).min(self.size.saturating_sub(self.pos));
        if n == 0 {
            return Ok(0);
        }
        let range = format!("bytes={}-{}", self.pos, self.pos + n - 1);
        let resp = get(&self.url, &[("Range", &range)]).map_err(io::Error::other)?;
        if resp.status() != 206 {
            return Err(io::Error::other(format!(
                "{} ignored the range request",
                self.url
            )));
        }
        let mut data = Vec::with_capacity(n as usize);
        resp.into_body()
            .into_reader()
            .take(n)
            .read_to_end(&mut data)?;
        buf[..data.len()].copy_from_slice(&data);
        self.pos += data.len() as u64;
        Ok(data.len())
    }
}

impl Seek for RangeReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(o) => o as i64,
            SeekFrom::Current(o) => self.pos as i64 + o,
            SeekFrom::End(o) => self.size as i64 + o,
        };
        if pos < 0 {
            return Err(io::Error::other("seek before start"));
        }
        self.pos = pos as u64;
        Ok(self.pos)
    }
}

pub fn remote_zip_names(url: &str, size: u64) -> Result<Vec<String>> {
    let reader = io::BufReader::with_capacity(1 << 20, RangeReader::new(url, size));
    let zip =
        zip::ZipArchive::new(reader).with_context(|| format!("reading zip index of {url}"))?;
    zip.file_names()
        .map(|name| Ok(name?.into_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::check_url;

    #[test]
    fn only_https_to_allowed_hosts() {
        assert!(check_url("https://files.pythonhosted.org/packages/x.whl").is_ok());
        assert!(check_url("http://files.pythonhosted.org/packages/x.whl").is_err());
        assert!(check_url("https://files.pythonhosted.org.evil.example/x.whl").is_err());
        assert!(check_url("https://pypi.org@evil.example/x.whl").is_err());
        assert!(check_url("https://pypi.org:8443/x.whl").is_err());
    }

    #[test]
    fn redirect_locations_resolve_before_the_allowlist_check() {
        use super::resolve_location;
        let from = "https://pypi.org/simple/p/";
        let resolve = |location: &str| resolve_location(from, location).unwrap();
        assert_eq!(resolve("/x"), "https://pypi.org/x");
        assert_eq!(resolve("//evil.example/x"), "https://evil.example/x");
        assert!(check_url(&resolve("//evil.example/x")).is_err());
        assert!(check_url(&resolve("http://pypi.org/x")).is_err());
    }

    #[test]
    fn a_url_to_an_allowed_host_passes_in_each_form() {
        for url in [
            "https://pypi.org/simple/openvino/",
            "https://pypi.org",
            "https://pypi.org?format=json",
            "https://pypi.org#files",
            "https://api.github.com/repos/intel/linux-npu-driver/releases?per_page=100",
            "https://release-assets.githubusercontent.com/x?sig=a%2Bb&se=2026-01-01T00%3A00%3A00Z",
        ] {
            check_url(url).unwrap_or_else(|e| panic!("{url}: {e}"));
        }
    }

    #[test]
    fn anything_but_https_is_refused() {
        for url in [
            "http://pypi.org/x",
            "ftp://pypi.org/x",
            "file:///etc/passwd",
            "HTTPS://pypi.org/x",
            "https:/pypi.org/x",
            "https:pypi.org/x",
            "//pypi.org/x",
            "pypi.org/x",
            " https://pypi.org/x",
            "",
        ] {
            assert!(check_url(url).is_err(), "{url:?}");
        }
    }

    #[test]
    fn userinfo_and_ports_are_refused() {
        for url in [
            "https://user@pypi.org/x",
            "https://user:secret@pypi.org/x",
            "https://pypi.org@evil.example/x",
            "https://pypi.org:443/x",
            "https://pypi.org:8443/x",
            "https://pypi.org:/x",
        ] {
            assert!(check_url(url).is_err(), "{url:?}");
        }
    }

    #[test]
    fn a_host_that_only_resembles_an_allowed_one_is_refused() {
        for url in [
            "https://evil.example/x",
            "https://pypi.org.evil.example/x",
            "https://evil-pypi.org/x",
            "https://sub.pypi.org/x",
            "https://PYPI.ORG/x",
            "https://pypi.org./x",
            "https://pypi%2Eorg/x",
            "https://pypi.org\\@evil.example/x",
            "https://evil.example\\.pypi.org/x",
            "https://pypi.org\t/x",
            "https://py\npi.org/x",
            "https://pypi.org /x",
            "https://[::1]/x",
            "https://127.0.0.1/x",
            "https:///pypi.org/x",
        ] {
            assert!(check_url(url).is_err(), "{url:?}");
        }
    }

    fn redirect(from: &str, location: &str) -> anyhow::Result<String> {
        let to = super::resolve_location(from, location)?;
        check_url(&to)?;
        Ok(to)
    }

    #[test]
    fn a_redirect_stays_on_the_host_or_names_an_allowed_one() {
        let from = "https://github.com/intel/linux-npu-driver/releases/download/v1/x.tar.gz";
        assert_eq!(redirect(from, "/y").unwrap(), "https://github.com/y");
        assert_eq!(
            redirect(from, "https://release-assets.githubusercontent.com/x?sig=1").unwrap(),
            "https://release-assets.githubusercontent.com/x?sig=1"
        );
        assert_eq!(
            redirect(from, "//release-assets.githubusercontent.com/x").unwrap(),
            "https://release-assets.githubusercontent.com/x"
        );
    }

    #[test]
    fn a_redirect_anywhere_else_is_refused() {
        let from = "https://pypi.org/simple/p/";
        for location in [
            "https://evil.example/x",
            "//evil.example/x",
            "http://pypi.org/x",
            "HTTPS://pypi.org/x",
            "//PYPI.ORG/x",
            "ftp://pypi.org/x",
            "https://user@pypi.org/x",
            "//user@pypi.org/x",
            "https://pypi.org:8443/x",
            "//pypi.org:8443/x",
            "https://pypi.org.evil.example/x",
            "/login?next=https://pypi.org/x",
        ] {
            assert!(redirect(from, location).is_err(), "{location:?}");
        }
    }

    #[test]
    fn every_phase_of_a_request_is_bounded() {
        let t = super::agent().config().timeouts();
        assert!(t.resolve.is_some());
        assert!(t.connect.is_some());
        assert!(t.recv_response.is_some());
        assert!(t.recv_body.is_some());
    }
}
