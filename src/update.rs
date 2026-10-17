//! In-app update check (DAN-1665 step 2, DAN-1669). Board decision D3: updates depend on how razerctl was installed, and are
//! **never silent**.
//!
//! - The check is one `GET /repos/Dromares/razerctl/releases/latest`, at most once in 24 hours, made by the GUI in the
//!   background ([`check_if_due`]). The CLI never checks on its own; `razerctl update` is the user asking.
//! - [`InstallType`] decides what happens next: an AppImage can replace itself after the user agrees ([`apply_appimage`]);
//!   a package-managed binary (under `/usr/`) is never touched and has the check off by default; anything else is told where
//!   to download.
//! - Offline, rate limited (403/429) or a 404 end as a line on the App tab and nothing else ([`Outcome`]).
//! - The AppImage is replaced only after its sha256 matches the release's `SHA256SUMS`. That catches a corrupt or truncated
//!   download, not a hijacked account (the sums come from the same place).
//!
//! HTTP is `ureq` 3 with rustls ([`net`]); hashing is [`crate::sha256`]; JSON is a small reader ([`json`]), so the feature adds
//! no serde. Every function takes its server as a [`Source`], so tests run against a local fixture.

mod apply;
mod install;
mod json;
mod net;
mod release;
mod state;
mod version;

pub use apply::{apply_appimage, fallback_dir, Applied, Stage, UpdateError};
pub use install::{classify, InstallType};
pub use net::{FetchError, Source, API_ENV, REPO};
pub use release::{sum_for, Asset, Release, SUMS_NAME};
pub use state::{now_secs, Outcome, Paths, State, INTERVAL_SECS};
pub use version::Version;

/// The version of this build.
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION is a semver version")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckResult {
    /// The latest release is not newer than `current`.
    UpToDate(Release),
    /// A newer release exists.
    Available(Release),
    Failed(FetchError),
}

impl CheckResult {
    pub fn outcome(&self) -> Outcome {
        match self {
            CheckResult::UpToDate(_) => Outcome::UpToDate,
            CheckResult::Available(_) => Outcome::Available,
            CheckResult::Failed(FetchError::Offline(_)) => Outcome::Offline,
            CheckResult::Failed(FetchError::RateLimited) => Outcome::RateLimited,
            CheckResult::Failed(_) => Outcome::Failed,
        }
    }
}

/// One request, no memory: what `razerctl update --check` and the GUI's *Check now* do. Returns the raw body too.
pub fn check(src: &Source, current: &Version) -> (CheckResult, Option<String>) {
    match net::fetch_latest(src) {
        Ok((r, body)) if r.version > *current => (CheckResult::Available(r), Some(body)),
        Ok((r, body)) => (CheckResult::UpToDate(r), Some(body)),
        Err(e) => (CheckResult::Failed(e), None),
    }
}

/// [`check`], then remembers it in `paths`: the time and outcome, and the release for the next window. A newer release than
/// the one the user last dismissed clears that dismissal, so *Not now* on 0.3.0 does not hide 0.3.1.
pub fn check_and_record(src: &Source, paths: &Paths, current: &Version, now: u64) -> CheckResult {
    let (result, body) = check(src, current);
    let before = paths.load();
    let previous_tag = paths.cached_release().map(|r| r.tag);
    let mut st = State {
        last_check: Some(now),
        outcome: Some(result.outcome()),
        dismissed: before.dismissed,
    };
    if let (Some(body), CheckResult::Available(r) | CheckResult::UpToDate(r)) = (&body, &result) {
        if previous_tag.as_deref() != Some(&r.tag) {
            st.dismissed = None;
        }
        let _ = paths.save_cache(body);
    }
    let _ = paths.save(&st);
    result
}

/// What the GUI does at start-up and once in a while: nothing when the check is off or was made less than 24 hours ago, else
/// [`check_and_record`].
pub fn check_if_due(
    src: &Source,
    paths: &Paths,
    current: &Version,
    now: u64,
    enabled: bool,
) -> Option<CheckResult> {
    (enabled && paths.load().due(now)).then(|| check_and_record(src, paths, current, now))
}

/// The release the last check found, if it is newer than `current` (so an update done by hand since then makes it vanish).
pub fn pending_release(paths: &Paths, current: &Version) -> Option<Release> {
    paths.cached_release().filter(|r| r.version > *current)
}

/// The user pressed *Not now*.
pub fn dismiss(paths: &Paths, now: u64) {
    let mut st = paths.load();
    st.dismissed = Some(now);
    let _ = paths.save(&st);
}
