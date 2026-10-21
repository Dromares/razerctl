//! The one real transport of the bootloader probe. Everything that touches `/dev/hidraw*`, `/sys` and `/proc` for it is
//! in this file, and `tests/probe.rs` pins what it may contain.
//!
//! The fence, in three layers:
//!
//! 1. [`RealProbe`] implements [`ProbeIo`] and nothing else of ours. `ProbeIo` has no method that takes a frame, so a
//!    caller cannot ask it to send one; the three methods that write ([`ProbeIo::read_version`],
//!    [`ProbeIo::enter_bootloader`], [`ProbeIo::exit_bootloader`]) build their frame from the constructors in
//!    `probe/mod.rs`.
//! 2. Every write goes through [`Guarded`], whose `send_feature` refuses any 90-byte payload that is not one of the three
//!    probe frames ([`super::permitted`]). That is a runtime check at the last point before the kernel, so a frame
//!    built by hand in this file would still not get out.
//! 3. This file does not name the phase-1 DFU module, its trait or its erase/program constructors; the fence test reads
//!    the source for that.

use super::report::{BootloaderWait, Enumeration, OpenFailure, ReturnWait};
use super::{
    enter_frame, exit_frame, permitted, read_version_frame, Answer, ProbeIo, Traffic,
    BOOTLOADER_ID, NORMAL_ID,
};
use crate::hidraw::{self, transact_on, FeatureIo, Hidraw, HidrawNode};
use crate::protocol::{Report, Status, REPORT_LEN};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// How often the node list is polled while waiting for a device to appear.
const POLL: Duration = Duration::from_millis(100);
/// After `1532:110E` first shows up, wait this long for its other interfaces and the udev ACL before listing it.
const SETTLE: Duration = Duration::from_millis(1000);
/// Follow-up reads of the reply after a send: `ATTEMPTS` polls, `STEP` apart.
const REPLY_ATTEMPTS: usize = 5;
const REPLY_STEP: Duration = Duration::from_millis(20);

/// A feature-report channel that can only carry the probe's three frames. Generic so the tests can put a recording
/// channel under it and show what does and does not get through.
pub struct Guarded<T: FeatureIo>(pub T);

impl<T: FeatureIo> FeatureIo for Guarded<T> {
    fn send_feature(&self, payload: &[u8; REPORT_LEN]) -> io::Result<()> {
        if !permitted(payload) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refused: the bootloader probe may only send 00:87, 00:04 mode 1 and 10:05",
            ));
        }
        self.0.send_feature(payload)
    }
    fn get_feature(&self) -> io::Result<[u8; REPORT_LEN + 1]> {
        self.0.get_feature()
    }
}

/// Send one probe frame, then read for its answer a few times. `Err` only if the frame could not be written; the
/// answer is best effort because a device that resets straight away often has none to give.
fn send_and_listen<T: FeatureIo>(io: &Guarded<T>, frame: &Report) -> Result<Answer, String> {
    io.send_feature(&frame.to_bytes()).map_err(|e| e.to_string())?;
    let sent = (frame.command_class, frame.command_id);
    let mut last = Err("no reply".to_string());
    for _ in 0..REPLY_ATTEMPTS {
        std::thread::sleep(REPLY_STEP);
        match io.get_feature().map_err(|e| e.to_string()).and_then(|raw| Report::parse(&raw)) {
            // busy / not-yet-answered / someone else's answer: keep listening
            Ok(r) if matches!(r.status_for(sent), Status::NewCommand | Status::Busy | Status::Mismatch) => {
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

/// Every Razer HID interface the kernel lists now, with its USB `bcdDevice` and report descriptor.
pub fn enumerate_razer() -> Vec<Enumeration> {
    let now = SystemTime::now();
    hidraw::enumerate()
        .unwrap_or_default()
        .iter()
        .map(|n| enumeration_of(n, now))
        .collect()
}

fn enumeration_of(n: &HidrawNode, seen_at: SystemTime) -> Enumeration {
    Enumeration {
        vid: n.vid,
        pid: n.pid,
        node: n.dev.display().to_string(),
        interface: n.interface,
        bcd_device: bcd_device(&n.sysfs),
        descriptor: fs::read(n.sysfs.join("report_descriptor")).unwrap_or_default(),
        seen_at,
    }
}

/// `.../usb1/1-3/1-3:1.2/0003:1532:110E.0004` -> `.../1-3/bcdDevice`.
fn bcd_device(sysfs: &Path) -> Option<u16> {
    let canon = fs::canonicalize(sysfs).ok()?;
    let usb = canon.parent()?.parent()?;
    u16::from_str_radix(fs::read_to_string(usb.join("bcdDevice")).ok()?.trim(), 16).ok()
}

fn key(e: &Enumeration) -> (String, u16, u16) {
    (e.node.clone(), e.vid, e.pid)
}

/// The real device: the dongle's control node, and later the node its bootloader appears on.
pub struct RealProbe {
    normal: Guarded<Hidraw>,
    node: HidrawNode,
    before: HashSet<(String, u16, u16)>,
    bootloader_nodes: Vec<Enumeration>,
    bootloader: Option<(String, Guarded<Hidraw>)>,
}

/// The nodes of `1532:009F` that exist now.
pub fn dongle_nodes() -> io::Result<Vec<HidrawNode>> {
    Ok(hidraw::enumerate()?
        .into_iter()
        .filter(|n| (n.vid, n.pid) == NORMAL_ID)
        .collect())
}

/// The node normal-mode commands go through: interface 0 if it has a feature report, else the first that has one.
pub fn control_node(nodes: &[HidrawNode]) -> Option<&HidrawNode> {
    nodes
        .iter()
        .find(|n| n.has_feature_reports && n.interface == Some(0))
        .or_else(|| nodes.iter().find(|n| n.has_feature_reports))
}

impl RealProbe {
    /// Open the control node of the dongle for read/write. Sends nothing.
    pub fn open(node: &HidrawNode) -> io::Result<RealProbe> {
        Ok(RealProbe {
            normal: Guarded(Hidraw::open(&node.dev)?),
            node: node.clone(),
            before: HashSet::new(),
            bootloader_nodes: Vec::new(),
            bootloader: None,
        })
    }
}

impl ProbeIo for RealProbe {
    fn now(&self) -> SystemTime {
        SystemTime::now()
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
        transact_on(&self.normal, &read_version_frame()).map_err(|e| e.to_string())
    }

    fn enter_bootloader(&mut self) -> Result<Answer, String> {
        // what exists now, so that step 3 can tell the bootloader's interfaces from the ones that were already there
        self.before = enumerate_razer().iter().map(key).collect();
        send_and_listen(&self.normal, &enter_frame())
    }

    fn wait_for_bootloader(&mut self, timeout: Duration) -> BootloaderWait {
        let start = Instant::now();
        let mut found: Vec<Enumeration> = Vec::new();
        let mut appeared_after = None;
        let mut settled_at: Option<Instant> = None;
        while start.elapsed() < timeout {
            for e in enumerate_razer() {
                let is_new = !self.before.contains(&key(&e));
                let is_boot = (e.vid, e.pid) == BOOTLOADER_ID;
                if (is_new || is_boot) && !found.iter().any(|f| key(f) == key(&e)) {
                    if is_boot && appeared_after.is_none() {
                        appeared_after = Some(start.elapsed());
                        settled_at = Some(Instant::now() + SETTLE);
                    }
                    found.push(e);
                }
            }
            if settled_at.is_some_and(|t| Instant::now() >= t) {
                break;
            }
            std::thread::sleep(POLL);
        }
        self.bootloader_nodes = found
            .iter()
            .filter(|e| (e.vid, e.pid) == BOOTLOADER_ID)
            .cloned()
            .collect();
        BootloaderWait {
            seen: found,
            appeared_after,
        }
    }

    fn open_bootloader(&mut self) -> Result<String, OpenFailure> {
        // the interface with a feature report, lowest number first; otherwise the first one listed
        let pick = self
            .bootloader_nodes
            .iter()
            .filter(|e| hidraw::descriptor_has_feature(&e.descriptor))
            .min_by_key(|e| e.interface.unwrap_or(u8::MAX))
            .or_else(|| self.bootloader_nodes.first())
            .cloned()
            .ok_or_else(|| OpenFailure {
                node: "(none)".into(),
                errno: None,
                message: "no 1532:110E node was recorded".into(),
            })?;
        match Hidraw::open(Path::new(&pick.node)) {
            Ok(h) => {
                let name = format!(
                    "{} (interface {})",
                    pick.node,
                    pick.interface.map_or("?".into(), |i| i.to_string())
                );
                self.bootloader = Some((pick.node, Guarded(h)));
                Ok(name)
            }
            Err(e) => Err(OpenFailure {
                node: pick.node,
                errno: e.raw_os_error(),
                message: e.to_string(),
            }),
        }
    }

    fn exit_bootloader(&mut self) -> Result<Answer, String> {
        let (node, io) = self
            .bootloader
            .as_ref()
            .ok_or("the bootloader node was never opened")?;
        send_and_listen(io, &exit_frame()).map_err(|e| format!("{node}: {e}"))
    }

    fn wait_for_return(&mut self, timeout: Duration) -> ReturnWait {
        let start = Instant::now();
        loop {
            let now = enumerate_razer();
            let back = now.iter().any(|e| (e.vid, e.pid) == NORMAL_ID)
                && !now.iter().any(|e| (e.vid, e.pid) == BOOTLOADER_ID);
            if back || start.elapsed() >= timeout {
                return ReturnWait {
                    after: back.then(|| start.elapsed()),
                    nodes: now
                        .iter()
                        .map(|e| {
                            format!(
                                "{:04x}:{:04x} {} interface {} bcdDevice {}",
                                e.vid,
                                e.pid,
                                e.node,
                                e.interface.map_or("?".into(), |i| i.to_string()),
                                e.bcd_device.map_or("?".into(), |b| format!("{b:04x}"))
                            )
                        })
                        .collect(),
                };
            }
            std::thread::sleep(POLL);
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
