//! The one real transport of the bootloader probe. Everything that touches `/dev/hidraw*`, `/sys` and `/proc` for it is
//! in this file, and `tests/probe.rs` pins what it may contain. The USB side (`/sys/bus/usb`, `/dev/bus/usb`) is in
//! `sysfs.rs` and `usbfs.rs`; this file only calls them.
//!
//! The fence, in three layers:
//!
//! 1. [`RealProbe`] implements [`ProbeIo`] and nothing else of ours. `ProbeIo` has no method that takes a frame or a
//!    control request, so a caller cannot ask it to send one; the methods that write ([`ProbeIo::read_version`],
//!    [`ProbeIo::enter_bootloader`], [`ProbeIo::query_bootloader`], [`ProbeIo::exit_bootloader`]) build their frame from
//!    the constructors in `probe/mod.rs`.
//! 2. Every hidraw write goes through [`Guarded`], whose `send_feature` refuses any 90-byte payload that is not one of the
//!    two normal-mode probe frames ([`super::permitted`]); every usbfs request goes through
//!    [`super::usbfs::UsbGuarded`], which refuses all but its three. Both are runtime checks at the last point before the
//!    kernel, so a frame built by hand in this file would still not get out.
//! 3. This file does not name the phase-1 DFU module, its trait or its erase/program constructors; the fence test reads
//!    the source for that.

use super::report::{BootloaderWait, Exchange, Get, OpenFailure, Pair, ReturnWait, SendError};
use super::sysfs::{self, UsbDevice};
use super::usbfs::{self, ControlIo, UsbGuarded, Usbfs};
use super::{
    enter_frame, permitted, read_version_frame, Answer, Decoded, ProbeIo, Traffic, BOOTLOADER_ID,
    EXIT_PAIRS, QUERY, QUERY_GETS, QUERY_PAIRS, Start,
};
use crate::hidraw::{self, transact_on, FeatureIo, Hidraw, HidrawNode};
use crate::protocol::{Report, Status, REPORT_LEN};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// How often the device list is polled while waiting for a device to appear or leave.
const POLL: Duration = Duration::from_millis(100);
/// After `1532:110E` first shows up, wait this long for its interface directory and the udev ACL before reading it, and
/// before anything is sent to it: Razer's updater sleeps 1000 ms there (`Sleep(1000)`, DAN-1932); probe v2 waited 500 ms.
pub const SETTLE: Duration = Duration::from_millis(1000);
/// The pause Razer's sender makes before every SET_REPORT and every GET_REPORT.
const PRE_DELAY: Duration = Duration::from_millis(2);
/// A usbfs node that exists but is not yet readable is retried this long (udev applies the ACL just after the node
/// appears). Any other error, or the same error after this, is final.
const OPEN_RETRY: Duration = Duration::from_millis(2000);
/// Follow-up reads of the reply after a hidraw send: `ATTEMPTS` polls, `STEP` apart.
const REPLY_ATTEMPTS: usize = 5;
const REPLY_STEP: Duration = Duration::from_millis(20);

/// A feature-report channel that can only carry the probe's two normal-mode frames. Generic so the tests can put a
/// recording channel under it and show what does and does not get through.
pub struct Guarded<T: FeatureIo>(pub T);

impl<T: FeatureIo> FeatureIo for Guarded<T> {
    fn send_feature(&self, payload: &[u8; REPORT_LEN]) -> io::Result<()> {
        if !permitted(payload) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refused: the bootloader probe may only send 00:87 and 00:04 mode 1 over hidraw",
            ));
        }
        self.0.send_feature(payload)
    }
    fn get_feature(&self) -> io::Result<[u8; REPORT_LEN + 1]> {
        self.0.get_feature()
    }
}

/// Send one probe frame, then read for its answer a few times. `Err` only if the frame could not be written (with the
/// errno); the answer is best effort because a device that resets straight away often has none to give.
pub fn send_and_listen<T: FeatureIo>(io: &Guarded<T>, frame: &Report) -> Result<Answer, SendError> {
    io.send_feature(&frame.to_bytes())?;
    let sent = (frame.command_class, frame.command_id);
    let mut last = Err("no reply".to_string());
    for _ in 0..REPLY_ATTEMPTS {
        std::thread::sleep(REPLY_STEP);
        match io
            .get_feature()
            .map_err(|e| e.to_string())
            .and_then(|raw| Report::parse(&raw))
        {
            // busy / not-yet-answered / someone else's answer: keep listening
            Ok(r)
                if matches!(
                    r.status_for(sent),
                    Status::NewCommand | Status::Busy | Status::Mismatch
                ) =>
            {
                last = Ok(r)
            }
            Ok(r) => return Ok(Ok(r)),
            Err(e) => {
                last = Err(e);
                break;
            }
        }
    }
    Ok(last)
}

/// Step 1 on any guarded channel: `00:87`, with the busy / collision handling every razerctl read has.
pub fn read_version_via<T: FeatureIo>(io: &Guarded<T>) -> Result<Report, String> {
    transact_on(io, &read_version_frame()).map_err(|e| e.to_string())
}

/// Step 2: `00:04` mode 1.
pub fn enter_via<T: FeatureIo>(io: &Guarded<T>) -> Result<Answer, SendError> {
    send_and_listen(io, &enter_frame())
}

/// One reply read for the report: the 90 bytes, or the error (the errno is kept).
fn read_get<T: ControlIo>(io: &UsbGuarded<T>) -> Get {
    std::thread::sleep(PRE_DELAY);
    let at = SystemTime::now();
    Get {
        at,
        reply: usbfs::read_reply(io).map_err(SendError::from),
    }
}

/// One SET_REPORT, after the vendor sender's pre-delay. `gets` is empty; the caller fills it.
fn send_set<T: ControlIo>(io: &UsbGuarded<T>, send: fn(&UsbGuarded<T>) -> io::Result<()>) -> Pair {
    std::thread::sleep(PRE_DELAY);
    let set_at = SystemTime::now();
    Pair {
        set_at,
        set: send(io).map_err(SendError::from),
        gets: Vec::new(),
    }
}

/// What a `10:80` reply means for the polling loop (the vendor sender, DAN-1932).
enum Poll {
    /// Status 2, txn 0, class/id `10:80`: this is the answer.
    Answer,
    /// Status 0 or 1, or a status 2 for another frame: ask again.
    Again,
    /// Any other status: send the SET again.
    Resend,
}

fn poll_query(raw: &[u8]) -> Poll {
    match Decoded::of(raw) {
        Some(d) if d.status == 2 && d.echoes(QUERY) => Poll::Answer,
        Some(d) if !matches!(d.status, 0..=2) => Poll::Resend,
        _ => Poll::Again,
    }
}

/// Step 3c on any guarded usbfs channel: `10:80` as a feature SET_REPORT, then up to [`QUERY_GETS`] GET_REPORTs, each after
/// [`PRE_DELAY`]. Status 0 or 1, a failed GET and a reply to another frame are polled again; any other status re-sends the
/// SET at once, and so do five GETs without an answer. At most [`QUERY_PAIRS`] SET/GET pairs. A SET that fails ends it
/// (the device may have left the bus). The caller decides what the last reply means ([`super::gate`]).
pub fn query_via<T: ControlIo>(io: &UsbGuarded<T>) -> Exchange {
    let mut x = Exchange::default();
    for _ in 0..QUERY_PAIRS {
        let mut pair = send_set(io, usbfs::send_query);
        if pair.set.is_err() {
            x.pairs.push(pair);
            return x;
        }
        let mut answered = false;
        for _ in 0..QUERY_GETS {
            let get = read_get(io);
            let next = match &get.reply {
                Ok(raw) => poll_query(raw),
                Err(_) => Poll::Again,
            };
            pair.gets.push(get);
            match next {
                Poll::Answer => {
                    answered = true;
                    break;
                }
                Poll::Again => {}
                Poll::Resend => break,
            }
        }
        x.pairs.push(pair);
        if answered {
            break;
        }
    }
    x
}

/// Step 3d on any guarded usbfs channel: `10:05` as a feature SET_REPORT, then one GET_REPORT, each after [`PRE_DELAY`].
/// A failed GET or a status other than 2 re-sends the same SET, up to [`EXIT_PAIRS`] SET/GET pairs; the result is logged and
/// never an error of its own, as in the vendor's updater. A SET that fails ends it: the device leaving the bus in the
/// middle of the transfer is the shape of the exit working, which the caller checks for.
pub fn exit_via<T: ControlIo>(io: &UsbGuarded<T>) -> Exchange {
    let mut x = Exchange::default();
    for _ in 0..EXIT_PAIRS {
        let mut pair = send_set(io, usbfs::send_exit);
        if pair.set.is_err() {
            x.pairs.push(pair);
            return x;
        }
        let get = read_get(io);
        let accepted = matches!(&get.reply, Ok(raw) if raw.first() == Some(&2));
        pair.gets.push(get);
        x.pairs.push(pair);
        if accepted {
            break;
        }
    }
    x
}

/// Where the probe looks: the kernel's USB device list and the usbfs nodes. The tests point both at a fixture tree.
#[derive(Debug, Clone)]
pub struct Roots {
    pub sysfs: PathBuf,
    pub dev: PathBuf,
}

impl Roots {
    pub fn system() -> Roots {
        Roots {
            sysfs: PathBuf::from(sysfs::DEFAULT_ROOT),
            dev: PathBuf::from("/dev/bus/usb"),
        }
    }
}

/// The real device: the dongle's control node, and later the usbfs node its bootloader appears on.
pub struct RealProbe {
    normal: Guarded<Hidraw>,
    node: HidrawNode,
    roots: Roots,
    usb: Option<UsbGuarded<Usbfs>>,
    start: Start,
}

/// The nodes of `1532:009F` and `1532:009E` that exist now.
pub fn probe_nodes() -> io::Result<Vec<HidrawNode>> {
    Ok(hidraw::enumerate()?
        .into_iter()
        .filter(|n| Start::of((n.vid, n.pid)).is_some())
        .collect())
}

/// Which identity the run starts from: exactly one of the dongle (`009F`) and the cable (`009E`) must be present. Both, or
/// neither, is a refusal that says which were found; nothing is sent then.
pub fn select_start(nodes: &[HidrawNode]) -> Result<Start, String> {
    let has = |s: Start| nodes.iter().any(|n| (n.vid, n.pid) == s.id());
    match (has(Start::Dongle), has(Start::Cable)) {
        (true, false) => Ok(Start::Dongle),
        (false, true) => Ok(Start::Cable),
        (true, true) => Err("refusing: both the dongle 1532:009F and the mouse on its USB cable 1532:009E are connected (found 1532:009F and 1532:009E); unplug one of them so the probe knows which to use. Nothing was sent".into()),
        (false, false) => Err("refusing: neither the Razer dongle 1532:009F nor the mouse on its USB cable 1532:009E is connected (found neither); nothing was sent".into()),
    }
}

/// The node normal-mode commands go through: interface 0 if it has a feature report, else the first that has one.
pub fn control_node(nodes: &[HidrawNode]) -> Option<&HidrawNode> {
    nodes
        .iter()
        .find(|n| n.has_feature_reports && n.interface == Some(0))
        .or_else(|| nodes.iter().find(|n| n.has_feature_reports))
}

/// Poll `done` every [`POLL`] until it is true or `timeout` has passed; the time it took if it became true.
fn poll_until(timeout: Duration, mut done: impl FnMut() -> bool) -> Option<Duration> {
    let start = Instant::now();
    loop {
        if done() {
            return Some(start.elapsed());
        }
        if start.elapsed() >= timeout {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

impl RealProbe {
    /// Open the control node of the dongle for read/write. Sends nothing.
    pub fn open(node: &HidrawNode, roots: Roots) -> io::Result<RealProbe> {
        let start = Start::of((node.vid, node.pid)).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{:04x}:{:04x} is neither 1532:009F nor 1532:009E", node.vid, node.pid),
            )
        })?;
        Ok(RealProbe {
            normal: Guarded(Hidraw::open(&node.dev)?),
            node: node.clone(),
            roots,
            usb: None,
            start,
        })
    }

    fn present(&self, id: (u16, u16)) -> bool {
        sysfs::present(&self.roots.sysfs, id)
    }

    /// One line per Razer USB device the kernel lists now.
    fn razer_listing(&self) -> Vec<String> {
        sysfs::scan(&self.roots.sysfs, SystemTime::now())
            .iter()
            .filter(|d| d.vid == hidraw::RAZER_VID)
            .map(UsbDevice::summary)
            .collect()
    }
}

/// What a frame that was asked for without an open usbfs node produced: a failed SET, nothing on the wire.
fn never_opened() -> Exchange {
    Exchange {
        pairs: vec![Pair {
            set_at: SystemTime::now(),
            set: Err(SendError {
                errno: None,
                message: "the usbfs node was never opened".into(),
            }),
            gets: Vec::new(),
        }],
    }
}

fn open_failure(node: &Path, e: &io::Error) -> OpenFailure {
    OpenFailure {
        node: node.display().to_string(),
        errno: e.raw_os_error(),
        message: e.to_string(),
    }
}

impl ProbeIo for RealProbe {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn start(&self) -> Start {
        self.start
    }

    fn describe_device(&self) -> String {
        format!(
            "{:04x}:{:04x} {} interface {} ({})",
            self.node.vid,
            self.node.pid,
            self.node.dev.display(),
            self.node.interface.map_or("?".into(), |i| i.to_string()),
            self.node.name
        )
    }

    fn read_version(&mut self) -> Result<Report, String> {
        read_version_via(&self.normal)
    }

    fn enter_bootloader(&mut self) -> Result<Answer, SendError> {
        enter_via(&self.normal)
    }

    fn wait_for_dongle_gone(&mut self, timeout: Duration) -> Option<Duration> {
        poll_until(timeout, || !self.present(self.start.id()))
    }

    fn wait_for_bootloader(&mut self, timeout: Duration) -> BootloaderWait {
        let found = poll_until(timeout, || self.present(BOOTLOADER_ID));
        if found.is_none() {
            return BootloaderWait {
                nodes: self.razer_listing(),
                ..BootloaderWait::default()
            };
        }
        let appeared_at = SystemTime::now();
        // the interface directory and the driver link come a moment after the device itself, and the vendor waits 1000 ms
        std::thread::sleep(SETTLE);
        let settled_at = SystemTime::now();
        BootloaderWait {
            device: sysfs::find(&self.roots.sysfs, BOOTLOADER_ID, settled_at),
            appeared_at: Some(appeared_at),
            settled_at: Some(settled_at),
            nodes: Vec::new(),
        }
    }

    fn open_usbfs(&mut self, device: &UsbDevice) -> Result<String, OpenFailure> {
        let (Some(bus), Some(dev)) = (device.bus, device.dev) else {
            return Err(OpenFailure {
                node: "(unknown)".into(),
                errno: None,
                message: format!("{} has no busnum/devnum in sysfs", device.name),
            });
        };
        let path = sysfs::usbfs_path(&self.roots.dev, bus, dev);
        let start = Instant::now();
        let usb = loop {
            match Usbfs::open(&path) {
                Ok(u) => break u,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
                    ) && start.elapsed() < OPEN_RETRY =>
                {
                    std::thread::sleep(POLL)
                }
                Err(e) => return Err(open_failure(&path, &e)),
            }
        };
        usb.claim_interface(usbfs::INTERFACE)
            .map_err(|e| open_failure(&path, &e))?;
        self.usb = Some(usb);
        Ok(path.display().to_string())
    }

    fn read_report_descriptor(&mut self) -> Result<Vec<u8>, SendError> {
        let usb = self.usb.as_ref().ok_or(SendError {
            errno: None,
            message: "the usbfs node was never opened".into(),
        })?;
        usbfs::read_report_descriptor(usb).map_err(SendError::from)
    }

    fn query_bootloader(&mut self) -> Exchange {
        match &self.usb {
            Some(usb) => query_via(usb),
            None => never_opened(),
        }
    }

    fn exit_bootloader(&mut self) -> Exchange {
        match &self.usb {
            Some(usb) => exit_via(usb),
            None => never_opened(),
        }
    }

    fn wait_for_bootloader_gone(&mut self, timeout: Duration) -> Option<Duration> {
        poll_until(timeout, || !self.present(BOOTLOADER_ID))
    }

    fn wait_for_return(&mut self, timeout: Duration) -> ReturnWait {
        let mut back_at = None;
        poll_until(timeout, || {
            let now = sysfs::scan(&self.roots.sysfs, SystemTime::now());
            let back = now.iter().any(|d| (d.vid, d.pid) == self.start.id())
                && !now.iter().any(|d| (d.vid, d.pid) == BOOTLOADER_ID);
            if back {
                back_at = Some(SystemTime::now());
            }
            back
        });
        ReturnWait {
            back_at,
            nodes: self.razer_listing(),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Is anything else talking to the device? Read-only look at /proc and /sys.

fn cmdline_of(pid_dir: &Path) -> String {
    let raw = fs::read(pid_dir.join("cmdline")).unwrap_or_default();
    let text: Vec<&str> = raw
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| std::str::from_utf8(s).ok())
        .collect();
    if text.is_empty() {
        fs::read_to_string(pid_dir.join("comm"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    } else {
        text.join(" ")
    }
}

/// Look for other users of the device. How each kind is detected:
///
/// * another razerctl (daemon, GUI, a second CLI call): a process under `proc_root` whose `comm` is `razerctl` or starts
///   with `razerctl-`, other than `me`;
/// * openrazer-daemon: a process whose `comm` starts with `openrazer-daem` or whose command line names
///   `openrazer-daemon`;
/// * openrazer kernel driver: the `driver` link of one of the device's HID nodes under `/sys` points at a driver named
///   `razer*` (`razermouse`, ...);
/// * anything else with the device open: a `/proc/<pid>/fd/*` link that points at one of its `/dev/hidrawN` nodes.
///   Processes of other users have unreadable `fd` directories; their number is returned in `unchecked`.
pub fn collect_traffic(proc_root: &Path, nodes: &[HidrawNode], me: u32) -> (Traffic, usize) {
    let mut t = Traffic::default();
    let mut unchecked = 0;
    for n in nodes {
        let driver = fs::read_link(n.sysfs.join("driver"))
            .ok()
            .and_then(|d| d.file_name().map(|s| s.to_string_lossy().into_owned()));
        if let Some(d) = driver.filter(|d| d.starts_with("razer")) {
            t.openrazer_drivers.push((n.dev.display().to_string(), d));
        }
    }
    let held: Vec<PathBuf> = nodes.iter().map(|n| n.dev.clone()).collect();
    let Ok(dir) = fs::read_dir(proc_root) else {
        return (t, unchecked);
    };
    let mut pids: Vec<(u32, PathBuf)> = dir
        .flatten()
        .filter_map(|e| Some((e.file_name().to_str()?.parse().ok()?, e.path())))
        .collect();
    pids.sort();
    for (pid, p) in pids {
        if pid == me {
            continue;
        }
        let comm = fs::read_to_string(p.join("comm"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let cmd = cmdline_of(&p);
        if comm == "razerctl" || comm.starts_with("razerctl-") {
            t.razerctl.push((pid, cmd.clone()));
        }
        if comm.starts_with("openrazer-daem") || cmd.contains("openrazer-daemon") {
            t.openrazer_daemons.push((pid, cmd.clone()));
        }
        match fs::read_dir(p.join("fd")) {
            Err(_) => unchecked += 1,
            Ok(fds) => {
                let mut mine: Vec<String> = fds
                    .flatten()
                    .filter_map(|f| fs::read_link(f.path()).ok())
                    .filter(|l| held.contains(l))
                    .map(|l| l.display().to_string())
                    .collect();
                mine.sort();
                mine.dedup();
                for node in mine {
                    t.holders.push((pid, cmd.clone(), node));
                }
            }
        }
    }
    (t, unchecked)
}
