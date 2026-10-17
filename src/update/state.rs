//! What the update check remembers between runs, under `~/.config/razerctl/`: when it last asked, how that went, and when
//! the user last said "Not now". That is what keeps the check to once in 24 hours and the banner to once a day. The latest
//! release response is cached beside it so a new window can show the dialog without asking again.

use super::release::Release;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const INTERVAL_SECS: u64 = 24 * 60 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The latest release is not newer than this build.
    UpToDate,
    /// A newer release was found (its details are in the cache).
    Available,
    Offline,
    RateLimited,
    /// Any other failure: a 404, a 5xx, a body that is not a release.
    Failed,
}

impl Outcome {
    fn word(self) -> &'static str {
        match self {
            Outcome::UpToDate => "up_to_date",
            Outcome::Available => "available",
            Outcome::Offline => "offline",
            Outcome::RateLimited => "rate_limited",
            Outcome::Failed => "failed",
        }
    }

    fn from_word(s: &str) -> Option<Outcome> {
        [
            Outcome::UpToDate,
            Outcome::Available,
            Outcome::Offline,
            Outcome::RateLimited,
            Outcome::Failed,
        ]
        .into_iter()
        .find(|o| o.word() == s)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    /// Unix seconds of the last check that was made, whatever its outcome.
    pub last_check: Option<u64>,
    pub outcome: Option<Outcome>,
    /// Unix seconds of the last "Not now".
    pub dismissed: Option<u64>,
}

impl State {
    /// `key = value` lines; unknown keys and bad values are skipped.
    pub fn parse(text: &str) -> State {
        let mut s = State::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim();
            match k.trim() {
                "last_check" => s.last_check = v.parse().ok(),
                "outcome" => s.outcome = Outcome::from_word(v),
                "dismissed" => s.dismissed = v.parse().ok(),
                _ => {}
            }
        }
        s
    }

    pub fn format(&self) -> String {
        let mut out = String::from("# razerctl update check: written by the GUI, safe to delete\n");
        if let Some(t) = self.last_check {
            out += &format!("last_check = {t}\n");
        }
        if let Some(o) = self.outcome {
            out += &format!("outcome = {}\n", o.word());
        }
        if let Some(t) = self.dismissed {
            out += &format!("dismissed = {t}\n");
        }
        out
    }

    /// A check is due when none was made, or the last one was 24 hours ago or more. A last check dated in the future (the
    /// clock was set back) counts as due, so a wrong clock cannot switch the check off for good.
    pub fn due(&self, now: u64) -> bool {
        match self.last_check {
            Some(t) if t <= now => now - t >= INTERVAL_SECS,
            _ => true,
        }
    }

    /// The banner is shown for an available release until the user says "Not now"; then it stays away for 24 hours.
    pub fn banner_wanted(&self, now: u64) -> bool {
        match self.dismissed {
            Some(t) if t <= now => now - t >= INTERVAL_SECS,
            Some(_) => true,
            None => true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub state: PathBuf,
    pub cache: PathBuf,
}

impl Paths {
    /// `~/.config/razerctl/update.state` and `update-latest.json`.
    pub fn default_paths() -> Paths {
        Paths::in_dir(crate::battery::config_path().parent().unwrap_or(Path::new("")))
    }

    pub fn in_dir(dir: impl AsRef<Path>) -> Paths {
        let dir = dir.as_ref();
        Paths {
            state: dir.join("update.state"),
            cache: dir.join("update-latest.json"),
        }
    }

    pub fn load(&self) -> State {
        fs::read_to_string(&self.state)
            .map(|t| State::parse(&t))
            .unwrap_or_default()
    }

    pub fn save(&self, s: &State) -> io::Result<()> {
        write_atomic(&self.state, &s.format())
    }

    pub fn save_cache(&self, body: &str) -> io::Result<()> {
        write_atomic(&self.cache, body)
    }

    pub fn cached_release(&self) -> Option<Release> {
        Release::parse(&fs::read_to_string(&self.cache).ok()?)
    }
}

/// Temp file in the same directory, then rename, so a reader never sees half a file.
fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_skips_what_it_does_not_understand() {
        let s = State {
            last_check: Some(1_760_000_000),
            outcome: Some(Outcome::RateLimited),
            dismissed: Some(1_760_000_100),
        };
        assert_eq!(State::parse(&s.format()), s);
        assert_eq!(State::parse(""), State::default());
        let junk = State::parse("last_check = soon\noutcome = great\nwhat\ndismissed=5\nextra = 1\n");
        assert_eq!(junk, State { last_check: None, outcome: None, dismissed: Some(5) });
    }

    #[test]
    fn a_check_is_due_after_a_day_and_after_a_clock_that_ran_backwards() {
        let s = State { last_check: Some(1000), ..State::default() };
        assert!(!s.due(1000));
        assert!(!s.due(1000 + INTERVAL_SECS - 1));
        assert!(s.due(1000 + INTERVAL_SECS));
        assert!(s.due(500), "last check in the future");
        assert!(State::default().due(0), "never checked");
    }

    #[test]
    fn not_now_silences_the_banner_for_a_day() {
        let s = State { dismissed: Some(1000), ..State::default() };
        assert!(!s.banner_wanted(1000));
        assert!(!s.banner_wanted(1000 + INTERVAL_SECS - 1));
        assert!(s.banner_wanted(1000 + INTERVAL_SECS));
        assert!(State::default().banner_wanted(0));
    }
}
