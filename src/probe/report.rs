//! The file `probe-bootloader` writes, built to be pasted onto DAN-1878 as it is: plain text, every observation with the
//! step and time it came from, and a first line that says how the run ended.

use super::sysfs::{Interface0, UsbDevice};
use super::usbfs::Setup;
use super::{
    exit_frame, query_frame, Build, Decoded, Start, BOOTLOADER_TIMEOUT, EXIT, EXIT_PAIRS, QUERY,
    QUERY_PAIRS, RETURN_TIMEOUT, UDEV_HINT,
};
use crate::protocol::REPORT_LEN;
use std::fmt::Write as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A write or read that failed, with the errno as the kernel gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendError {
    pub errno: Option<i32>,
    pub message: String,
}

impl From<&std::io::Error> for SendError {
    fn from(e: &std::io::Error) -> SendError {
        SendError {
            errno: e.raw_os_error(),
            message: e.to_string(),
        }
    }
}

impl From<std::io::Error> for SendError {
    fn from(e: std::io::Error) -> SendError {
        SendError::from(&e)
    }
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.errno {
            Some(n) => write!(f, "errno {n} ({})", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

#[derive(Debug, Default)]
pub struct BootloaderWait {
    /// The `1532:110E` device, read once it had settled; `None` = it never appeared.
    pub device: Option<UsbDevice>,
    /// When it was first seen.
    pub appeared_at: Option<SystemTime>,
    /// When the settle wait after that ended: the earliest moment the probe may open usbfs.
    pub settled_at: Option<SystemTime>,
    /// When it never appeared: one line per `1532` USB device listed at the timeout (VID:PID, port, bcdDevice, product), so
    /// a bootloader that enumerates under another PID still shows up. Empty when the device appeared.
    pub nodes: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ReturnWait {
    /// When the starting identity (`1532:009F` or `1532:009E`) was seen again (with no `110E` left).
    pub back_at: Option<SystemTime>,
    /// One line per Razer USB device present at the end of the wait.
    pub nodes: Vec<String>,
}

/// One GET_REPORT: when it was issued, and the 90 bytes it returned or why there were none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Get {
    pub at: SystemTime,
    pub reply: Result<Vec<u8>, SendError>,
}

/// One SET_REPORT and the GET_REPORTs that followed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pair {
    pub set_at: SystemTime,
    pub set: Result<(), SendError>,
    /// Empty if the SET_REPORT failed and no GET was tried.
    pub gets: Vec<Get>,
}

/// Everything one frame (`10:80` or `10:05`) put on the wire: up to three SET/GET pairs, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exchange {
    pub pairs: Vec<Pair>,
}

impl Exchange {
    /// The error of the last SET_REPORT, if it failed. After one the device may have left the bus, so nothing was retried.
    pub fn set_error(&self) -> Option<&SendError> {
        self.pairs.last()?.set.as_ref().err()
    }

    /// The last GET_REPORT of the last pair: the reply that decides what comes next.
    pub fn last_get(&self) -> Option<&Get> {
        self.pairs.last()?.gets.last()
    }

    /// How many SET_REPORTs went out.
    pub fn sets(&self) -> usize {
        self.pairs.len()
    }
}

/// Why the bootloader's usbfs node could not be opened. `errno` is recorded as the kernel gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenFailure {
    pub node: String,
    pub errno: Option<i32>,
    pub message: String,
}

impl OpenFailure {
    /// EACCES or EPERM: the user may not open the node, which the udev rule is for.
    pub fn is_permission_error(&self) -> bool {
        matches!(self.errno, Some(libc::EACCES) | Some(libc::EPERM))
    }
}

impl std::fmt::Display for OpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.errno {
            Some(n) => write!(
                f,
                "open {} failed with errno {n} ({})",
                self.node, self.message
            ),
            None => write!(f, "open {} failed ({})", self.node, self.message),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Every step ran and the starting identity came back.
    Completed,
    ReadVersionFailed(String),
    EnterFailed(String),
    /// Step 3 timed out: nothing further was sent.
    BootloaderNeverAppeared,
    /// Step 3b: the usbfs node could not be opened, or interface 0 could not be claimed. Nothing was sent. `permission`: the
    /// error was EACCES or EPERM, so the udev rule is the thing to fix.
    UsbfsUnavailable {
        why: String,
        permission: bool,
    },
    /// Step 3b: a kernel driver owns interface 0. Not detached; nothing was sent.
    DriverBound(String),
    /// Step 3c: the `10:80` query got no answer the gate accepts (no reply, a status other than 2, a different frame
    /// echoed, or a PID other than the starting one), so `10:05` was **not** sent and nothing further was.
    NoExit(String),
    /// Step 3d: the exit request failed and `1532:110E` did not leave the bus.
    ExitFailed(String),
    /// Step 4 timed out.
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

    /// What the user should do next, one line each.
    pub fn next_steps(&self, start: Start) -> Vec<String> {
        let mut out = Vec::new();
        if matches!(
            self,
            Outcome::UsbfsUnavailable {
                permission: true,
                ..
            }
        ) {
            out.push(UDEV_HINT.to_string());
        }
        if self.needs_replug() {
            out.push(start.replug());
        }
        out
    }

    fn headline(&self, start: Start) -> String {
        match self {
            Outcome::Completed => {
                format!(
                "COMPLETED: every step ran and {} is listed again (timing below)",
                start.id_text()
            )
            }
            Outcome::ReadVersionFailed(w) => {
                format!("STOPPED at step 1 (read version): {w}. Nothing was sent after it")
            }
            Outcome::EnterFailed(w) => {
                format!("STOPPED at step 2 (enter bootloader): {w}. Nothing was sent after it")
            }
            Outcome::BootloaderNeverAppeared => format!(
                "STEP 3 TIMED OUT: 1532:110E did not appear in /sys/bus/usb/devices within {} s. Nothing further was sent",
                BOOTLOADER_TIMEOUT.as_secs()
            ),
            Outcome::UsbfsUnavailable { why: w, .. } => {
                format!("STOPPED at step 3b (usbfs): {w}. Nothing was sent to the bootloader")
            }
            Outcome::DriverBound(w) => format!(
                "STOPPED at step 3b (usbfs): {w}. Not detached; nothing was sent to the bootloader"
            ),
            Outcome::NoExit(w) => format!(
                "STOPPED at step 3c (10:80 query): {w}. 10:05 was NOT sent and nothing further was sent"
            ),
            Outcome::ExitFailed(w) => format!(
                "STOPPED at step 3d (exit): 10:05 could not be sent: {w}; 1532:110E is still on the bus"
            ),
            Outcome::DidNotReturn => format!(
                "STEP 4 TIMED OUT: {} did not come back within {} s of 10:05",
                start.id_text(),
                RETURN_TIMEOUT.as_secs()
            ),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Step {
    /// `1`, `2`, `3`, `3b`, `3c`, `3d`, `4`.
    pub id: &'static str,
    pub what: String,
    pub started: SystemTime,
    pub ended: SystemTime,
    pub result: String,
}

impl Step {
    pub fn new(
        id: &'static str,
        what: &str,
        started: SystemTime,
        ended: SystemTime,
        result: String,
    ) -> Step {
        Step {
            id,
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
    /// The identity the run started from: the dongle `1532:009F` or the cable `1532:009E`.
    pub start: Start,
    pub started: SystemTime,
    pub device: String,
    pub steps: Vec<Step>,
    /// Step 1: `major.minor.patch` as the reply carried it.
    pub version_read: Option<String>,
    /// Step 2: when `00:04` was sent, the anchor of "since bootloader entry".
    pub entered_at: Option<SystemTime>,
    /// Step 3: the `1532:110E` device as the kernel listed it, and when it was first seen.
    pub bootloader: Option<UsbDevice>,
    pub appeared_at: Option<SystemTime>,
    /// Step 3b: the usbfs node that was opened, or why it could not be.
    pub usbfs: Option<Result<String, OpenFailure>>,
    /// Step 3b: the HID report descriptor of interface 0, or the errno of the read.
    pub report_descriptor: Option<Result<Vec<u8>, SendError>>,
    /// Step 3: when the settle wait after `1532:110E` appeared ended.
    pub settled_at: Option<SystemTime>,
    /// Step 3: the `1532` devices listed when `1532:110E` never appeared.
    pub timeout_nodes: Vec<String>,
    /// Step 3c: the `10:80` query, every SET/GET attempt, and what the gate made of it: `Ok(pid)` = `10:05` may go, `Err` =
    /// the reason it did not.
    pub query: Option<Exchange>,
    pub query_at: Option<SystemTime>,
    pub gate: Option<Result<u16, String>>,
    /// Step 3d: when `10:05` was sent, and what came of it.
    pub exit_sent_at: Option<SystemTime>,
    pub exit: Option<Exchange>,
    /// Step 4.
    pub returned_at: Option<SystemTime>,
    pub after_nodes: Vec<String>,
    pub outcome: Option<Outcome>,
}

fn secs_between(from: SystemTime, to: SystemTime) -> f64 {
    to.duration_since(from).unwrap_or_default().as_secs_f64()
}

impl ProbeLog {
    pub fn new(build: Build, started: SystemTime, device: String, start: Start) -> ProbeLog {
        ProbeLog {
            build,
            start,
            started,
            device,
            steps: Vec::new(),
            version_read: None,
            entered_at: None,
            bootloader: None,
            appeared_at: None,
            usbfs: None,
            report_descriptor: None,
            settled_at: None,
            timeout_nodes: Vec::new(),
            query: None,
            query_at: None,
            gate: None,
            exit_sent_at: None,
            exit: None,
            returned_at: None,
            after_nodes: Vec::new(),
            outcome: None,
        }
    }

    pub fn push(&mut self, s: Step) {
        self.steps.push(s);
    }

    fn has_step(&self, id: &str) -> bool {
        self.steps.iter().any(|s| s.id == id)
    }

    pub fn render(&self) -> String {
        let mut o = String::new();
        let _ = writeln!(o, "razerctl bootloader probe v4 (DAN-1944, for DAN-1878)");
        let _ = writeln!(
            o,
            "razerctl {} git {}",
            self.build.version, self.build.git_sha
        );
        let _ = writeln!(o, "started {}", utc(self.started));
        let _ = writeln!(o, "started from {}", self.start.label());
        let _ = writeln!(o, "device  {}", self.device);
        let _ = writeln!(
            o,
            "RESULT  {}",
            match &self.outcome {
                Some(r) => r.headline(self.start),
                None => "INCOMPLETE: the program stopped before it finished (this is a checkpoint)"
                    .into(),
            }
        );
        if let Some(r) = &self.outcome {
            for n in r.next_steps(self.start) {
                let _ = writeln!(o, "NEXT    {n}");
            }
        }
        let _ = writeln!(o, "\nsteps (UTC)");
        for s in &self.steps {
            let _ = writeln!(
                o,
                "  {}. {}  {} -> {}\n       {}",
                s.id,
                s.what,
                utc(s.started),
                utc(s.ended),
                s.result
            );
        }
        if let Some(v) = &self.version_read {
            let _ = writeln!(o, "\nversion read in step 1: {v}");
        }
        if !self.has_step("3") {
            return o;
        }
        self.render_bootloader(&mut o);
        self.render_usbfs(&mut o);
        self.render_query(&mut o);
        self.render_exit(&mut o);
        if self.has_step("4") {
            self.render_return(&mut o);
        }
        o
    }

    fn render_bootloader(&self, o: &mut String) {
        let _ = writeln!(o, "\n1532:110E in /sys/bus/usb/devices (step 3)");
        let Some(d) = &self.bootloader else {
            let _ = writeln!(o, "  never listed");
            let _ = writeln!(
                o,
                "  Razer USB devices (1532) listed at the timeout ({})",
                self.timeout_nodes.len()
            );
            for n in &self.timeout_nodes {
                let _ = writeln!(o, "    {n}");
            }
            return;
        };
        let _ = writeln!(
            o,
            "  device {}  bus {}  device number {}  seen {}",
            d.name,
            d.bus.map_or("?".into(), |n| n.to_string()),
            d.dev.map_or("?".into(), |n| n.to_string()),
            utc(d.seen_at)
        );
        if let Some(s) = self.settled_at {
            let _ = writeln!(
                o,
                "  settled {} after it was first seen, at {}; usbfs was opened after this",
                self.appeared_at.map_or("?".into(), |a| secs(
                    s.duration_since(a).unwrap_or_default()
                )),
                utc(s)
            );
        }
        let _ = writeln!(
            o,
            "  bcdDevice {}  product {}",
            d.bcd_device.map_or("?".into(), |b| format!("{b:04x}")),
            d.product
                .as_deref()
                .map_or("?".into(), |p| format!("\"{p}\""))
        );
        let _ = writeln!(
            o,
            "  driver bound to interface 0: {}",
            match &d.interface0 {
                Interface0::Missing => "interface 0 is not listed".to_string(),
                Interface0::Unbound => "none".to_string(),
                Interface0::Bound(n) => format!("`{n}`"),
            }
        );
        match &d.descriptors {
            Ok(b) => {
                let _ = writeln!(o, "  kernel descriptors ({} bytes): {}", b.len(), hex(b));
            }
            Err(e) => {
                let _ = writeln!(o, "  kernel descriptors: could not be read ({e})");
            }
        }
    }

    fn render_usbfs(&self, o: &mut String) {
        match &self.usbfs {
            Some(Ok(node)) => {
                let _ = writeln!(o, "\nusbfs (step 3b): opened {node} as the current user, root was not needed, interface 0 claimed");
            }
            Some(Err(f)) => {
                let need_root = f.is_permission_error();
                let _ = writeln!(
                    o,
                    "\nusbfs (step 3b): FAILED, {f}; {}. Not escalated, nothing was sent.",
                    if need_root {
                        "the udev rule does not let this user open it (is the 1532:110E line of udev/70-razerctl.rules installed?)"
                    } else {
                        "this is not a permission error"
                    }
                );
            }
            None => return,
        }
        match &self.report_descriptor {
            Some(Ok(b)) => {
                let _ = writeln!(
                    o,
                    "HID report descriptor of interface 0, GET_DESCRIPTOR {} ({} bytes): {}",
                    setup_hex(&Setup::get_report_descriptor()),
                    b.len(),
                    hex(b)
                );
            }
            Some(Err(e)) => {
                let _ = writeln!(
                    o,
                    "HID report descriptor of interface 0, GET_DESCRIPTOR {}: FAILED, {e}",
                    setup_hex(&Setup::get_report_descriptor())
                );
            }
            None => {}
        }
    }

    /// The `10:80` query (step 3c): every SET/GET attempt, what the reply decodes to, and the gate's decision.
    fn render_query(&self, o: &mut String) {
        let Some(x) = &self.query else { return };
        let _ = writeln!(
            o,
            "\nquery 10:80 (step 3c), sent {}: payload ({} bytes) {}",
            self.query_at.map_or("?".into(), utc),
            REPORT_LEN,
            hex(&query_frame().to_bytes())
        );
        self.render_exchange(o, "query 10:80", QUERY, QUERY_PAIRS, x);
        match &self.gate {
            Some(Ok(pid)) => {
                let _ = writeln!(
                    o,
                    "gate: OPEN. The reply is status 2, txn 0, class/id 10:80 and PID {pid:04x}, so 10:05 was sent next"
                );
            }
            Some(Err(why)) => {
                let _ = writeln!(o, "gate: CLOSED. {why}. 10:05 was NOT sent");
            }
            None => {}
        }
    }

    fn render_exit(&self, o: &mut String) {
        let Some(x) = &self.exit else { return };
        let _ = writeln!(
            o,
            "\nexit 10:05 (step 3d), sent {}: payload ({} bytes) {}",
            self.exit_sent_at.map_or("?".into(), utc),
            REPORT_LEN,
            hex(&exit_frame().to_bytes())
        );
        self.render_exchange(o, "exit 10:05", EXIT, EXIT_PAIRS, x);
    }

    /// One line per SET_REPORT and per GET_REPORT, in the order they went out.
    fn render_exchange(
        &self,
        o: &mut String,
        label: &str,
        sent: (u8, u8),
        max_pairs: usize,
        x: &Exchange,
    ) {
        let set = setup_hex(&Setup::set_report_feature());
        let get = setup_hex(&Setup::get_report_feature());
        for (i, p) in x.pairs.iter().enumerate() {
            let n = i + 1;
            match &p.set {
                Ok(()) => {
                    let _ = writeln!(
                        o,
                        "{label}, SET_REPORT {set}: accepted (attempt {n} of up to {max_pairs}, at {})",
                        utc(p.set_at)
                    );
                }
                Err(e) => {
                    let _ = writeln!(
                        o,
                        "{label}, SET_REPORT {set}: {e} (attempt {n} of up to {max_pairs}, at {})",
                        utc(p.set_at)
                    );
                }
            }
            for (j, g) in p.gets.iter().enumerate() {
                let at = utc(g.at);
                match &g.reply {
                    Ok(raw) => {
                        let d = Decoded::of(raw);
                        let _ = match &d {
                            Some(d) => writeln!(
                                o,
                                "status read, GET_REPORT {get}: command {:02x}:{:02x} status byte {:#04x} (read {} of attempt {n}, at {at})",
                                d.class,
                                d.id,
                                d.status,
                                j + 1
                            ),
                            None => writeln!(
                                o,
                                "status read, GET_REPORT {get}: a reply of {} bytes, not {REPORT_LEN} (read {} of attempt {n}, at {at})",
                                raw.len(),
                                j + 1
                            ),
                        };
                        let _ = writeln!(o, "  reply ({} bytes): {}", raw.len(), hex(raw));
                        if let Some(d) = d {
                            let _ = writeln!(
                                o,
                                "  decoded: status {:#04x} txn {:#04x} class/id {:02x}:{:02x} PID {:04x}{}",
                                d.status,
                                d.txn,
                                d.class,
                                d.id,
                                d.pid,
                                if d.echoes(sent) {
                                    ""
                                } else {
                                    " (txn/class/id are not those of the frame sent)"
                                }
                            );
                        }
                    }
                    Err(e) => {
                        let _ = writeln!(
                            o,
                            "status read, GET_REPORT {get}: {e} (read {} of attempt {n}, at {at})",
                            j + 1
                        );
                    }
                }
            }
        }
    }

    fn render_return(&self, o: &mut String) {
        let _ = writeln!(
            o,
            "\n{} after 10:05: {}",
            self.start.id_text(),
            match self.returned_at {
                Some(back) => {
                    let mut s = String::new();
                    if let Some(x) = self.exit_sent_at {
                        let _ = write!(s, "{:.2} s after the 10:05 send", secs_between(x, back));
                    }
                    if let Some(e) = self.entered_at {
                        let _ = write!(
                            s,
                            ", {:.2} s after the enter-bootloader send (step 2)",
                            secs_between(e, back)
                        );
                    }
                    if let Some(a) = self.appeared_at {
                        let _ = write!(
                            s,
                            ", {:.2} s after 1532:110E appeared (step 3); back at {}",
                            secs_between(a, back),
                            utc(back)
                        );
                    }
                    format!("came back {s}")
                }
                None => format!("did not come back within {} s", RETURN_TIMEOUT.as_secs()),
            }
        );
        let _ = writeln!(
            o,
            "Razer USB devices at the end of step 4 ({})",
            self.after_nodes.len()
        );
        for n in &self.after_nodes {
            let _ = writeln!(o, "  {n}");
        }
    }
}

fn setup_hex(s: &Setup) -> String {
    hex(&s.to_bytes())
}

pub fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
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

/// A duration for the step detail lines: `2.50 s`.
pub fn secs(d: Duration) -> String {
    format!("{:.2} s", d.as_secs_f64())
}
