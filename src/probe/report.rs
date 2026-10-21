//! The file `probe-bootloader` writes, built to be pasted onto DAN-1878 as it is: plain text, every observation with the
//! step and time it came from, and a first line that says how the run ended.

use super::{Build, BOOTLOADER_TIMEOUT, REPLUG, RETURN_TIMEOUT};
use std::fmt::Write as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// One HID interface of a Razer device, as the kernel listed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enumeration {
    pub vid: u16,
    pub pid: u16,
    /// `/dev/hidrawN`.
    pub node: String,
    pub interface: Option<u8>,
    /// `bcdDevice` of the USB device the interface belongs to.
    pub bcd_device: Option<u16>,
    pub descriptor: Vec<u8>,
    pub seen_at: SystemTime,
}

#[derive(Debug, Default)]
pub struct BootloaderWait {
    /// Every node that appeared during the wait and was not there before it (the `110E` interfaces among them).
    pub seen: Vec<Enumeration>,
    /// How long after the wait began the first `1532:110E` node appeared.
    pub appeared_after: Option<Duration>,
}

#[derive(Debug, Default)]
pub struct ReturnWait {
    /// How long after `10:05` was sent `1532:009F` was back (and no `110E` node was left).
    pub after: Option<Duration>,
    /// One line per Razer node present at the end of the wait.
    pub nodes: Vec<String>,
}

/// Why the bootloader's node could not be opened. `errno` is recorded as the kernel gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenFailure {
    pub node: String,
    pub errno: Option<i32>,
    pub message: String,
}

impl std::fmt::Display for OpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.errno {
            Some(n) => write!(f, "open {} failed with errno {n} ({})", self.node, self.message),
            None => write!(f, "open {} failed ({})", self.node, self.message),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// All five steps ran and `1532:009F` came back by itself.
    Completed,
    ReadVersionFailed(String),
    EnterFailed(String),
    /// Step 3 timed out: nothing further was sent.
    BootloaderNeverAppeared,
    OpenFailed(String),
    ExitFailed(String),
    /// Step 5 timed out.
    DidNotReturn,
}

impl Outcome {
    /// After these the dongle may still be in its bootloader, so the user is told to replug.
    pub fn needs_replug(&self) -> bool {
        !matches!(
            self,
            Outcome::Completed | Outcome::ReadVersionFailed(_) | Outcome::EnterFailed(_)
        )
    }

    pub fn is_success(&self) -> bool {
        matches!(self, Outcome::Completed)
    }

    fn headline(&self) -> String {
        match self {
            Outcome::Completed => "COMPLETED: all five steps ran and 1532:009F came back by itself".into(),
            Outcome::ReadVersionFailed(w) => format!("STOPPED at step 1 (read version): {w}. Nothing was sent after it"),
            Outcome::EnterFailed(w) => format!("STOPPED at step 2 (enter bootloader): {w}. Nothing was sent after it"),
            Outcome::BootloaderNeverAppeared => format!(
                "STEP 3 TIMED OUT: 1532:110E did not appear within {} s. Nothing further was sent",
                BOOTLOADER_TIMEOUT.as_secs()
            ),
            Outcome::OpenFailed(w) => format!("STOPPED at step 4: {w}. 10:05 was not sent"),
            Outcome::ExitFailed(w) => format!("STOPPED at step 5: 10:05 could not be sent: {w}"),
            Outcome::DidNotReturn => format!(
                "STEP 5 TIMED OUT: 1532:009F did not come back within {} s of 10:05",
                RETURN_TIMEOUT.as_secs()
            ),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Step {
    pub n: u8,
    pub what: String,
    pub started: SystemTime,
    pub ended: SystemTime,
    pub result: String,
}

impl Step {
    pub fn new(n: u8, what: &str, started: SystemTime, ended: SystemTime, result: String) -> Step {
        Step {
            n,
            what: what.into(),
            started,
            ended,
            result,
        }
    }
}

#[derive(Debug)]
pub struct ProbeLog {
    pub build: Build,
    pub started: SystemTime,
    pub device: String,
    pub steps: Vec<Step>,
    /// Step 1: `major.minor.patch` as the reply carried it.
    pub version_read: Option<String>,
    /// Step 3: every node that appeared.
    pub enumerations: Vec<Enumeration>,
    pub appeared_after: Option<Duration>,
    /// Step 4: the node that was opened, or why it could not be.
    pub open: Option<Result<String, OpenFailure>>,
    /// Step 5.
    pub returned_after: Option<Duration>,
    pub after_nodes: Vec<String>,
    pub outcome: Option<Outcome>,
}

impl ProbeLog {
    pub fn new(build: Build, started: SystemTime, device: String) -> ProbeLog {
        ProbeLog {
            build,
            started,
            device,
            steps: Vec::new(),
            version_read: None,
            enumerations: Vec::new(),
            appeared_after: None,
            open: None,
            returned_after: None,
            after_nodes: Vec::new(),
            outcome: None,
        }
    }

    pub fn push(&mut self, s: Step) {
        self.steps.push(s);
    }

    pub fn render(&self) -> String {
        let mut o = String::new();
        let _ = writeln!(o, "razerctl bootloader probe (DAN-1908, for DAN-1878)");
        let _ = writeln!(o, "razerctl {} git {}", self.build.version, self.build.git_sha);
        let _ = writeln!(o, "started {}", utc(self.started));
        let _ = writeln!(o, "device  {}", self.device);
        let _ = writeln!(
            o,
            "RESULT  {}",
            match &self.outcome {
                Some(r) => r.headline(),
                None => "INCOMPLETE: the program stopped before it finished (this is a checkpoint)".into(),
            }
        );
        if self.outcome.as_ref().is_some_and(Outcome::needs_replug) {
            let _ = writeln!(o, "NEXT    {REPLUG}");
        }
        let _ = writeln!(o, "\nsteps (UTC)");
        for s in &self.steps {
            let _ = writeln!(
                o,
                "  {}. {}  {} -> {}\n       {}",
                s.n,
                s.what,
                utc(s.started),
                utc(s.ended),
                s.result
            );
        }
        if let Some(v) = &self.version_read {
            let _ = writeln!(o, "\nversion read in step 1: {v}");
        }
        if !self.steps.iter().any(|s| s.n == 3) {
            return o;
        }
        let _ = writeln!(o, "\nenumerations observed in step 3 ({})", self.enumerations.len());
        if self.enumerations.is_empty() {
            let _ = writeln!(o, "  none");
        }
        for (i, e) in self.enumerations.iter().enumerate() {
            let _ = writeln!(
                o,
                "  #{} {:04x}:{:04x}  hidraw node {}  interface {}  bcdDevice {}  seen {}",
                i + 1,
                e.vid,
                e.pid,
                e.node,
                e.interface.map_or("?".into(), |n| n.to_string()),
                e.bcd_device.map_or("?".into(), |b| format!("{b:04x}")),
                utc(e.seen_at)
            );
            let _ = writeln!(
                o,
                "     report descriptor ({} bytes): {}",
                e.descriptor.len(),
                hex(&e.descriptor)
            );
        }
        match &self.open {
            Some(Ok(node)) => {
                let _ = writeln!(o, "\nopen of the 1532:110E node {node}: succeeded as the current user, root was not needed");
            }
            Some(Err(f)) => {
                let need_root = matches!(f.errno, Some(libc::EACCES) | Some(libc::EPERM));
                let _ = writeln!(
                    o,
                    "\nopen of the 1532:110E node {}: FAILED, errno {} ({}); {}. Not escalated.",
                    f.node,
                    f.errno.map_or("none".into(), |n| n.to_string()),
                    f.message,
                    if need_root {
                        "the current udev rule does not let this user open it, so root (or a udev rule for 1532:110E) would be needed"
                    } else {
                        "this is not a permission error"
                    }
                );
            }
            None => {}
        }
        if self.steps.iter().any(|s| s.n == 5 && s.what.starts_with("wait")) {
            let _ = writeln!(
                o,
                "\n1532:009F after 10:05: {}",
                match self.returned_after {
                    Some(d) => format!("came back by itself after {:.2} s", d.as_secs_f64()),
                    None => format!("did not come back within {} s", RETURN_TIMEOUT.as_secs()),
                }
            );
            let _ = writeln!(o, "Razer nodes at the end of step 5 ({})", self.after_nodes.len());
            for n in &self.after_nodes {
                let _ = writeln!(o, "  {n}");
            }
        }
        o
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

/// `2026-10-21T09:53:07.123Z`.
pub fn utc(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let (secs, ms) = (d.as_secs() as i64, d.subsec_millis());
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // civil-from-days (H. Hinnant), valid for the whole proleptic Gregorian range
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}
