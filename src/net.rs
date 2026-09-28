use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

/// Every host ovfetch will talk to. A URL from an API response (PyPI's JSON,
/// a mirror's href) is only data: if it names any other host, or a redirect
/// lands anywhere else, the request is refused before bytes are read.
const ALLOWED_HOSTS: &[&str] = &[
    "pypi.org",
    "files.pythonhosted.org",
    "pypi.tuna.tsinghua.edu.cn",
    "mirrors.aliyun.com",
    "mirror.sjtu.edu.cn",
    "mirrors.cloud.tencent.com",
    "pypi.mirrors.ustc.edu.cn",
    "api.github.com",
    "raw.githubusercontent.com",
];

/// Rejects anything but `https://<allowed host>/...`: no plain HTTP, no
/// userinfo, no explicit port, no host outside the list.
pub fn check_url(url: &str) -> Result<()> {
    let rest = url
        .strip_prefix("https://")
        .with_context(|| format!("refusing non-HTTPS URL {url}"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains(['@', ':']) || !ALLOWED_HOSTS.contains(&authority) {
        bail!("refusing {url}: {authority} is not an allowed host");
    }
    Ok(())
}

fn call(req: ureq::Request) -> Result<ureq::Response> {
    let url = req.url().to_owned();
    check_url(&url)?;
    let resp = req.call().with_context(|| format!("GET {url}"))?;
    check_url(resp.get_url()).with_context(|| format!("{url} redirected"))?;
    Ok(resp)
}

fn agent() -> ureq::Agent {
    // TLS is rustls with Mozilla's roots compiled in, so a spoofed DNS answer
    // still has to present a valid certificate for the allowed host.
    ureq::AgentBuilder::new()
        .https_only(true)
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(60))
        .user_agent(concat!("ovfetch/", env!("CARGO_PKG_VERSION")))
        .build()
}

pub fn get_text(url: &str) -> Result<String> {
    let mut req = agent().get(url);
    if let (true, Ok(token)) = (
        url.starts_with("https://api.github.com/"),
        std::env::var("GITHUB_TOKEN"),
    ) {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    call(req)?
        .into_string()
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
    let resp = call(agent().get(url))?;
    let mut reader = resp.into_reader();
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
        let resp = call(agent().get(&self.url).set("Range", &range)).map_err(io::Error::other)?;
        if resp.status() != 206 {
            return Err(io::Error::other(format!(
                "{} ignored the range request",
                self.url
            )));
        }
        let mut data = Vec::with_capacity(n as usize);
        resp.into_reader().take(n).read_to_end(&mut data)?;
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
    Ok(zip.file_names().map(str::to_owned).collect())
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
}
