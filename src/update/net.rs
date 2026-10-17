//! The only code that talks to the network: `ureq` 3 with rustls (board-approved crate, DAN-1669). Everything here takes the
//! base URL from a [`Source`], so the tests point it at a local fixture and never touch the network.

use super::release::Release;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const GITHUB_API: &str = "https://api.github.com";
pub const REPO: &str = "Dromares/razerctl";
/// Testing hook: `RAZERCTL_UPDATE_API=http://127.0.0.1:PORT` points the check at a local fixture (see [`Source::from_env`]).
pub const API_ENV: &str = "RAZERCTL_UPDATE_API";

/// Largest thing downloaded: the release JSON and `SHA256SUMS` are small, the AppImage is ~10 MB.
const MAX_TEXT: u64 = 1 << 20;
pub const MAX_APPIMAGE: u64 = 256 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    api: String,
    repo: String,
    /// Asset URLs must be `https://`. Off only for a loopback fixture.
    https_only: bool,
}

impl Source {
    pub fn github() -> Source {
        Source {
            api: GITHUB_API.into(),
            repo: REPO.into(),
            https_only: true,
        }
    }

    /// A plain-HTTP server on this machine, for tests. Anything else is refused by `from_env`.
    pub fn loopback(base: &str) -> Source {
        Source {
            api: base.trim_end_matches('/').into(),
            repo: REPO.into(),
            https_only: false,
        }
    }

    /// GitHub, unless `RAZERCTL_UPDATE_API` names a loopback `http://` base (`127.0.0.1` or `localhost`) or another
    /// `https://` base. Any other value is ignored, so the variable cannot downgrade a real check to plain HTTP.
    pub fn from_env() -> Source {
        match std::env::var(API_ENV) {
            Ok(v) if is_loopback_http(&v) => Source::loopback(&v),
            Ok(v) if v.starts_with("https://") => Source {
                api: v.trim_end_matches('/').into(),
                ..Source::github()
            },
            _ => Source::github(),
        }
    }

    pub fn latest_url(&self) -> String {
        format!("{}/repos/{}/releases/latest", self.api, self.repo)
    }

    fn allows(&self, url: &str) -> bool {
        url.starts_with("https://") || (!self.https_only && url.starts_with("http://"))
    }
}

fn is_loopback_http(v: &str) -> bool {
    ["http://127.0.0.1:", "http://localhost:", "http://127.0.0.1/", "http://localhost/"]
        .iter()
        .any(|p| v.starts_with(p))
}

/// Why a fetch failed. The GUI shows `Offline`, `RateLimited`, `NotFound` and `Other` only as a line on the App tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchError {
    /// No answer: DNS, connect, TLS or timeout.
    Offline(String),
    /// 403 or 429: GitHub's rate limit.
    RateLimited,
    /// 404: no such release (or a private repo).
    NotFound,
    /// Any other status, an unreadable body, or a refused URL.
    Other(String),
    Cancelled,
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Offline(e) => write!(f, "no answer from the server ({e})"),
            FetchError::RateLimited => f.write_str("GitHub asked razerctl to wait (rate limit)"),
            FetchError::NotFound => f.write_str("no release found (404)"),
            FetchError::Other(e) => f.write_str(e),
            FetchError::Cancelled => f.write_str("cancelled"),
        }
    }
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .user_agent(concat!("razerctl/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

fn get(
    src: &Source,
    url: &str,
    timeout: Duration,
) -> Result<ureq::http::Response<ureq::Body>, FetchError> {
    if !src.allows(url) {
        return Err(FetchError::Other(format!("refusing a non-https URL: {url}")));
    }
    let resp = agent(timeout)
        .get(url)
        .header("Accept", "application/vnd.github+json, application/octet-stream")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| match e {
            ureq::Error::Io(_)
            | ureq::Error::Timeout(_)
            | ureq::Error::HostNotFound
            | ureq::Error::ConnectionFailed
            | ureq::Error::Tls(_)
            | ureq::Error::Rustls(_) => FetchError::Offline(e.to_string()),
            e => FetchError::Other(e.to_string()),
        })?;
    match resp.status().as_u16() {
        200 => Ok(resp),
        403 | 429 => Err(FetchError::RateLimited),
        404 => Err(FetchError::NotFound),
        s => Err(FetchError::Other(format!("server answered {s}"))),
    }
}

/// A small text body (the release JSON, `SHA256SUMS`).
pub fn fetch_text(src: &Source, url: &str) -> Result<String, FetchError> {
    let mut resp = get(src, url, Duration::from_secs(15))?;
    resp.body_mut()
        .with_config()
        .limit(MAX_TEXT)
        .read_to_string()
        .map_err(|e| FetchError::Other(format!("unreadable response: {e}")))
}

/// `GET releases/latest`. Returns the release and the raw body (the caller caches it).
pub fn fetch_latest(src: &Source) -> Result<(Release, String), FetchError> {
    let body = fetch_text(src, &src.latest_url())?;
    match Release::parse(&body) {
        Some(r) => Ok((r, body)),
        None => Err(FetchError::Other("the answer is not a release".into())),
    }
}

/// Streams `url` to `dest` (replacing it), calling `progress(done, total)` as it goes. `total` is the server's length or 0.
/// Stops with `Cancelled` and removes `dest` when `cancel` is set. Larger than `max` bytes is an error.
pub fn download(
    src: &Source,
    url: &str,
    dest: &Path,
    max: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<(), FetchError> {
    let result = download_inner(src, url, dest, max, cancel, progress);
    if result.is_err() {
        let _ = fs::remove_file(dest);
    }
    result
}

fn download_inner(
    src: &Source,
    url: &str,
    dest: &Path,
    max: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<(), FetchError> {
    let resp = get(src, url, Duration::from_secs(20 * 60))?;
    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    if total > max {
        return Err(FetchError::Other(format!("the file is {total} bytes, over the {max} limit")));
    }
    let io_err = |e: io::Error| FetchError::Other(format!("{}: {e}", dest.display()));
    let mut out = fs::File::create(dest).map_err(io_err)?;
    let mut body = resp.into_body().into_reader();
    let mut buf = vec![0u8; 64 * 1024];
    let mut done = 0u64;
    progress(0, total);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(FetchError::Cancelled);
        }
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(FetchError::Offline(e.to_string())),
        };
        done += n as u64;
        if done > max {
            return Err(FetchError::Other(format!("the download passed the {max} byte limit")));
        }
        out.write_all(&buf[..n]).map_err(io_err)?;
        progress(done, total);
    }
    if total != 0 && done != total {
        return Err(FetchError::Offline(format!("cut off after {done} of {total} bytes")));
    }
    out.sync_all().map_err(io_err)
}
