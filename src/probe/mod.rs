//! `razerctl firmware probe-bootloader` (DAN-1908 phase 2, DAN-1929 phase 2b, DAN-1935 phase 2d, DAN-1944 phase 2e, DAN-1948 phase 2f of DAN-1878):
//! put the mouse, from the dongle `1532:009F` or from its USB cable `1532:009E` ([`Start`]), into its bootloader, look at how the bootloader shows up on Linux, ask it the one question Razer's updater asks
//! (`10:80`), and, only if the answer is the expected one, take it out again. A mouse that is already in its bootloader
//! (`1532:110E` on hidraw, [`Origin::Bootloader`]) skips the first two steps. **It never erases and never writes**: the
//! only frames this module can emit are the four below, and everything else is refused structurally.
//!
//! * [`read_version_frame`] `00:87` and [`enter_frame`] `00:04` mode 1 go out through hidraw (`hw.rs`): [`permitted`]
//!   accepts exactly these two byte-for-byte, and [`hw::Guarded`] refuses anything else before it reaches the kernel.
//! * [`query_frame`] `10:80` and [`exit_frame`] `10:05` go out to the bootloader (`1532:110E`) one of two ways. The dongle's
//!   bootloader has no interrupt endpoint, so no driver binds it and the kernel makes no hidraw node: they go through usbfs
//!   (`usbfs.rs`). [`usbfs::permitted`] accepts exactly three control requests: the HID report descriptor read, the feature
//!   SET_REPORT (carrying `10:80` or `10:05`, byte for byte) and its GET_REPORT. [`usbfs::UsbGuarded`] refuses anything else
//!   before the ioctl. The mouse's bootloader has an interrupt IN endpoint, `usbhid` binds it and it has a hidraw node: they
//!   go through that node (`hidboot.rs`) as the same feature reports. [`hidboot::permitted`] accepts exactly these two
//!   frames, and [`hidboot::BootGuarded`] refuses anything else before the ioctl. On both paths `10:01` (erase), `10:02`
//!   (program), a class `0x0A` frame and every other class `0x10` frame cannot get out.
//! * `10:05` goes out only after a `10:80` reply with status 2 and a PID equal to the starting PID ([`gate`]; a run that
//!   starts from the bootloader accepts `009E` or `009F` and waits for that one); [`run`] sends nothing after any other reply.
//! * The DFU trait of phase 1 (`firmware::dfu::Transport`: erase, program, verify) is not implemented by anything in
//!   here, and nothing in `probe` names `firmware::dfu`, `firmware::flash` or the erase/program/class `0x0A` commands.
//!   `cargo test` fails (`tests/probe.rs`) if a real-transport module could reach them.
//! * The orchestration ([`run`]) is written against [`ProbeIo`]; the tests drive it with a simulated device only.
//!
//! Layout: `mod.rs` (frames, [`ProbeIo`], [`run`], the gate, the refusal rules), `report.rs` (the file the board pastes
//! onto DAN-1878), `sysfs.rs` (read-only look at `/sys/bus/usb/devices`), `usbfs.rs` (the control-transfer transport and
//! its allow-list), `hidboot.rs` (the bootloader's hidraw node and its allow-list), `hw.rs` (the real [`ProbeIo`]), `cli.rs`
//! (argument handling, refusals, output file).

pub mod cli;
pub mod hidboot;
pub mod hw;
pub mod report;
pub mod sysfs;
pub mod usbfs;

use crate::protocol::{Report, Status, REPORT_LEN};
use report::{
    BootloaderWait, Exchange, HidrawOpened, OpenFailure, Outcome, ProbeLog, ReturnWait, SendError,
    Step,
};
use std::time::{Duration, SystemTime};
use sysfs::{Interface0, UsbDevice};

pub const READ_VERSION: (u8, u8) = (0x00, 0x87);
pub const ENTER_MODE: (u8, u8) = (0x00, 0x04);
/// `10:80`: the query Razer's updater sends on every bootloader open (DAN-1932). It cannot erase or program.
pub const QUERY: (u8, u8) = (0x10, 0x80);
pub const EXIT: (u8, u8) = (0x10, 0x05);
/// Transaction id of every frame: the vendor updater never writes the byte (same as phase 1).
pub const TID: u8 = 0;

/// The dongle in normal mode (Viper Mini SE).
pub const NORMAL_ID: (u16, u16) = (0x1532, 0x009F);
/// The mouse on its USB cable (`PID2`, DAN-1932): the same probe with the other starting identity (DAN-1944).
pub const CABLE_ID: (u16, u16) = (0x1532, 0x009E);
/// The same dongle in its bootloader.
pub const BOOTLOADER_ID: (u16, u16) = (0x1532, 0x110E);
/// Which of the two normal-mode identities the run started from. The run only ever looks for, and only ever accepts a
/// `10:80` reply naming, this one (DAN-1944).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// `1532:009F`: the mouse through its dongle.
    Dongle,
    /// `1532:009E`: the mouse on its USB cable.
    Cable,
}

impl Start {
    pub fn of(id: (u16, u16)) -> Option<Start> {
        match id {
            NORMAL_ID => Some(Start::Dongle),
            CABLE_ID => Some(Start::Cable),
            _ => None,
        }
    }
    pub fn id(self) -> (u16, u16) {
        match self {
            Start::Dongle => NORMAL_ID,
            Start::Cable => CABLE_ID,
        }
    }
    pub fn pid(self) -> u16 {
        self.id().1
    }
    /// `1532:009F`, upper case, as the messages of probe v1 to v3 write the dongle.
    pub fn id_text(self) -> String {
        format!("{:04X}:{:04X}", self.id().0, self.id().1)
    }
    /// What the run calls the thing the user plugged in.
    pub fn what(self) -> &'static str {
        match self {
            Start::Dongle => "dongle",
            Start::Cable => "mouse",
        }
    }
    /// The line for the report head: which identity the run used.
    pub fn label(self) -> String {
        match self {
            Start::Dongle => format!("dongle {}", self.id_text()),
            Start::Cable => format!("cable {}", self.id_text()),
        }
    }
}

/// Where the run began: in normal mode on one of the two identities of [`Start`], or already in the mouse's bootloader
/// (`1532:110E` on hidraw, DAN-1948).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Normal(Start),
    /// Steps 1 and 2 are skipped. The identity to wait for in step 4 is not known until the `10:80` reply names it, so it
    /// starts as `None` and the run sets it when the gate opens.
    Bootloader(Option<Start>),
}

impl Origin {
    /// The identity step 4 waits for: the starting one, or, from the bootloader, the one the `10:80` reply named.
    pub fn target(self) -> Option<Start> {
        match self {
            Origin::Normal(s) => Some(s),
            Origin::Bootloader(t) => t,
        }
    }
    /// The line for the report head: which identity the run used.
    pub fn label(self) -> String {
        match self {
            Origin::Normal(s) => s.label(),
            Origin::Bootloader(_) => format!(
                "the bootloader {:04X}:{:04X} (the mouse was already in it)",
                BOOTLOADER_ID.0, BOOTLOADER_ID.1
            ),
        }
    }
    /// `1532:009F`, `1532:009E`, or, from the bootloader before the reply has named one, both.
    pub fn target_text(self) -> String {
        match self.target() {
            Some(s) => s.id_text(),
            None => format!("{} or {}", Start::Dongle.id_text(), Start::Cable.id_text()),
        }
    }
    /// What to tell the user when the device may still be in its bootloader, one line each. A dongle is bus-powered, so
    /// unplugging it resets it. The mouse is not: it runs on its battery, so a cable replug does not take it out of its
    /// bootloader (the board's run of 2026-10-22), and nothing here promises that it does.
    pub fn recovery(self) -> Vec<String> {
        match self {
            Origin::Normal(Start::Dongle) => vec![format!(
                "Unplug the dongle and plug it back in. The firmware was not erased or written, so the mouse should come back as {}.",
                Start::Dongle.id_text()
            )],
            Origin::Normal(Start::Cable) | Origin::Bootloader(_) => vec![
                "The mouse may still be in its bootloader (lsusb shows 1532:110e). A replug of the USB cable does not reset it: it runs on its battery. The firmware was not erased or written.".to_string(),
                "Switch the mouse off with the switch on its underside, wait 10 s and switch it on (not verified). If it is still in its bootloader, use Razer's updater on Windows, or in a VM with the USB port passed through.".to_string(),
            ],
        }
    }
}

/// How long step 3 waits for `1532:110E` before it gives up and tells the user to replug. The board's kernel log has it
/// enumerated about a second after the enter frame.
pub const BOOTLOADER_TIMEOUT: Duration = Duration::from_secs(10);
/// How long steps 2 and 3d wait for a device to leave the bus after a send that returned an error.
pub const LEAVE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long step 4 waits for the starting identity to come back, counted from the exit send. If `10:05` works it is back within
/// seconds. Nothing shows that the bootloader ever leaves by itself (the return on the board's first run was a replug),
/// so a timeout here means the device has to be replugged.
pub const RETURN_TIMEOUT: Duration = Duration::from_secs(30);
/// SET/GET pairs of the `10:80` query: a status other than 0, 1 or 2 (or five GETs without an answer) re-sends the SET.
pub const QUERY_PAIRS: usize = 3;
/// GETs per `10:80` SET: status 0 or 1, a failed GET or a reply to another frame is polled again.
pub const QUERY_GETS: usize = 5;
/// SET/GET pairs of the `10:05` exit: one GET per SET, and a failed GET or a status other than 2 re-sends the SET.
pub const EXIT_PAIRS: usize = 3;

/// What to tell the user when the usbfs node of the bootloader cannot be opened.
pub const UDEV_HINT: &str = "Reinstall the udev rule, which now has a line for the bootloader (1532:110E): sudo cp udev/70-razerctl.rules /etc/udev/rules.d/ && sudo udevadm control --reload && sudo udevadm trigger";

/// `00:87`: read the version. Args 0..3 carry major, minor, patch (DAN-1879; not confirmed on a device).
pub fn read_version_frame() -> Report {
    Report::new(READ_VERSION, 8, TID)
}
/// `00:04` with args `[1, 0]`: switch to bootloader mode.
pub fn enter_frame() -> Report {
    Report::new(ENTER_MODE, 2, TID).with_args(&[1, 0])
}
/// `10:80`: ask the bootloader what it is. Size 9, no arguments; the reply carries its PID.
pub fn query_frame() -> Report {
    Report::new(QUERY, 9, TID)
}
/// `10:05`: leave the bootloader.
pub fn exit_frame() -> Report {
    Report::new(EXIT, 0, TID)
}

/// The hidraw allow-list: the two normal-mode frames, byte for byte. (`10:80` and `10:05` are not on it: the bootloader
/// has no hidraw node, so they only go out through [`usbfs`].)
pub fn permitted(bytes: &[u8; REPORT_LEN]) -> bool {
    [read_version_frame(), enter_frame()]
        .iter()
        .any(|f| f.to_bytes() == *bytes)
}

/// The fields of a 90-byte reply that the probe reads (no report id): status `[0]`, transaction id `[1]`, command class
/// and id `[6]`, `[7]`, and, in the answer to `10:80`, the bootloader's PID as `[16] | [15] << 8` (DAN-1932).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decoded {
    pub status: u8,
    pub txn: u8,
    pub class: u8,
    pub id: u8,
    pub pid: u16,
}

impl Decoded {
    /// `None` unless the reply is exactly [`REPORT_LEN`] bytes.
    pub fn of(raw: &[u8]) -> Option<Decoded> {
        if raw.len() != REPORT_LEN {
            return None;
        }
        Some(Decoded {
            status: raw[0],
            txn: raw[1],
            class: raw[6],
            id: raw[7],
            pid: u16::from(raw[16]) | u16::from(raw[15]) << 8,
        })
    }

    /// Transaction id 0 and the class/id of the frame that was sent: the reply is to that frame.
    pub fn echoes(&self, sent: (u8, u8)) -> bool {
        self.txn == TID && (self.class, self.id) == sent
    }
}

/// What the gate decides from the `10:80` exchange: `Ok(pid)` = status 2, txn 0, class/id `10:80` and a PID **equal to the
/// starting PID** in the **last** reply read, so `10:05` may follow; `Err(why)` = it may not, with the reason for the file.
/// (Probe v3 accepted either bootloader PID; v4 wants the one it started from, DAN-1944. A run that starts from the
/// bootloader has no starting PID: it accepts `009E` or `009F`, DAN-1948, and the return wait then looks for that one.)
pub fn gate(query: &Exchange, origin: Origin) -> Result<u16, String> {
    if let Some(e) = query.set_error() {
        return Err(format!("the 10:80 SET_REPORT failed: {e}"));
    }
    let Some(last) = query.last_get() else {
        return Err("no 10:80 GET_REPORT was read".into());
    };
    let raw = match &last.reply {
        Ok(raw) => raw,
        Err(e) => {
            return Err(format!(
                "the 10:80 got no reply: the last GET_REPORT failed with {e}"
            ))
        }
    };
    let Some(d) = Decoded::of(raw) else {
        return Err(format!(
            "the 10:80 reply is {} bytes, not {REPORT_LEN}",
            raw.len()
        ));
    };
    if d.status != 2 {
        return Err(format!(
            "the 10:80 reply has status {:#04x}, not 2",
            d.status
        ));
    }
    if !d.echoes(QUERY) {
        return Err(format!(
            "the 10:80 reply has txn {:#04x} and class/id {:02x}:{:02x}, not txn 0 and 10:80",
            d.txn, d.class, d.id
        ));
    }
    match origin {
        Origin::Normal(start) if d.pid != start.pid() => {
            return Err(format!(
                "the 10:80 reply names PID {:04x}, not {:04x} (the PID this run started from; nothing more is sent)",
                d.pid,
                start.pid()
            ));
        }
        Origin::Bootloader(_) if Start::of((NORMAL_ID.0, d.pid)).is_none() => {
            return Err(format!(
                "the 10:80 reply names PID {:04x}, not {:04x} or {:04x} (the run started from the bootloader, so it has no starting PID; nothing more is sent)",
                d.pid, CABLE_ID.1, NORMAL_ID.1
            ));
        }
        _ => {}
    }
    Ok(d.pid)
}

/// What the follow-up read after a send produced: the device's answer, or why there was none.
pub type Answer = Result<Report, String>;

/// Everything the probe does to the world. A real device is behind it only in [`hw`]; there is no method that takes a
/// frame or a control request, so an implementation cannot be asked to send anything but the probe's own.
pub trait ProbeIo {
    fn now(&self) -> SystemTime;
    /// Where the run began: the dongle `1532:009F`, the cable `1532:009E`, or the mouse already in its bootloader. Fixed for
    /// the whole run: the leave-the-bus wait and the gate use it.
    fn origin(&self) -> Origin;
    /// Which node and interface the normal-mode commands go through.
    fn describe_device(&self) -> String;
    /// Step 1: send `00:87` and return the reply (a failed exchange is an error).
    fn read_version(&mut self) -> Result<Report, String>;
    /// Step 2: send `00:04` mode 1. `Err` = the send returned an error (an `EPROTO` here can mean the device left the bus
    /// mid-transfer because it obeyed); `Ok` carries what the follow-up read gave.
    fn enter_bootloader(&mut self) -> Result<Answer, SendError>;
    /// Step 2, after a send that returned an error: wait up to `timeout` for the starting identity ([`ProbeIo::origin`]) to leave the bus. `Some` = it
    /// left, that long after the call.
    fn wait_for_dongle_gone(&mut self, timeout: Duration) -> Option<Duration>;
    /// Step 3: watch `/sys/bus/usb/devices` for `1532:110E` for at most `timeout`, then let it settle for at least 1000 ms
    /// (the wait Razer's updater makes before it opens the device). On a timeout `nodes` lists every `1532` device then present.
    fn wait_for_bootloader(&mut self, timeout: Duration) -> BootloaderWait;
    /// Step 3b: open the bootloader's usbfs node and claim interface 0. `Ok` names the node; `Err` says why not (the
    /// errno is recorded). Sends nothing.
    fn open_usbfs(&mut self, device: &UsbDevice) -> Result<String, OpenFailure>;
    /// Step 3b, when `usbhid` has bound interface 0 and it has one hidraw node: open that node (the one sysfs links to the
    /// device), check with `HIDIOCGRAWINFO` that it is `1532:110E`, and send nothing. `Err` says why not; nothing is detached.
    fn open_hidraw(&mut self, device: &UsbDevice) -> Result<HidrawOpened, OpenFailure>;
    /// Step 3b: read the HID report descriptor of interface 0 (a standard GET_DESCRIPTOR through usbfs, or
    /// `HIDIOCGRDESCSIZE`/`HIDIOCGRDESC` through the hidraw node, whichever was opened).
    fn read_report_descriptor(&mut self) -> Result<Vec<u8>, SendError>;
    /// Step 3c: send `10:80` as a feature report (usbfs SET_REPORT or `HIDIOCSFEATURE`) and read its answer, with the vendor's
    /// polling and re-sends.
    fn query_bootloader(&mut self) -> Exchange;
    /// Step 3d: send `10:05` as a feature SET_REPORT and read its GET_REPORT, re-sending up to three times.
    fn exit_bootloader(&mut self) -> Exchange;
    /// Step 3d, after an exit request that returned an error: wait up to `timeout` for `1532:110E` to leave the bus.
    fn wait_for_bootloader_gone(&mut self, timeout: Duration) -> Option<Duration>;
    /// Step 4: watch for `back` (the starting identity, or the one a bootloader start's `10:80` reply named) to come back (and
    /// no `110E` to be left) for at most `timeout`.
    fn wait_for_return(&mut self, back: Start, timeout: Duration) -> ReturnWait;
}

/// razerctl's own version and the commit it was built from, for the head of the report.
#[derive(Debug, Clone)]
pub struct Build {
    pub version: String,
    pub git_sha: String,
}

fn status_name(r: &Report, sent: (u8, u8)) -> String {
    format!("{:?} (status byte {:#04x})", r.status_for(sent), r.status)
}

fn args_hex(r: &Report, n: usize) -> String {
    r.args[..n]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run the steps. `checkpoint` is called with the log after every step, so a file written from it is current even
/// if the process dies in the next step. Whatever happens, the returned log has an [`Outcome`].
pub fn run<I: ProbeIo>(
    io: &mut I,
    build: &Build,
    checkpoint: &mut dyn FnMut(&ProbeLog),
) -> ProbeLog {
    let mut origin = io.origin();
    let mut log = ProbeLog::new(build.clone(), io.now(), io.describe_device(), origin);
    checkpoint(&log);

    // 1 and 2 talk to the normal-mode node; a run that starts from the bootloader has none and skips both (DAN-1948)
    if let Origin::Normal(start) = origin {
        // 1. read the version
        let at = io.now();
        let (detail, version) = match io.read_version() {
            Err(e) => {
                log.push(Step::new(
                    "1",
                    "read version (00:87)",
                    at,
                    io.now(),
                    format!("FAILED: {e}"),
                ));
                return finish(log, Outcome::ReadVersionFailed(e), checkpoint);
            }
            Ok(r) if r.status_for(READ_VERSION) != Status::Success => {
                let why = status_name(&r, READ_VERSION);
                log.push(Step::new(
                    "1",
                    "read version (00:87)",
                    at,
                    io.now(),
                    format!("FAILED: the device answered {why}"),
                ));
                return finish(log, Outcome::ReadVersionFailed(why), checkpoint);
            }
            Ok(r) => {
                let v = format!("{}.{:02}.{:02}", r.args[0], r.args[1], r.args[2]);
                (
                    format!("ok, version {v} (reply args {})", args_hex(&r, 8)),
                    v,
                )
            }
        };
        log.version_read = Some(version);
        log.push(Step::new("1", "read version (00:87)", at, io.now(), detail));
        checkpoint(&log);

        // 2. enter the bootloader. A send that returns an error counts as "entered" only if the starting identity then leaves the bus:
        //    on the board's first run the kernel log showed the disconnect landing in the middle of the transfer, and that
        //    cut-off transfer was the error (EPROTO).
        let at = io.now();
        log.entered_at = Some(at);
        let detail = match io.enter_bootloader() {
        Err(e) => match io.wait_for_dongle_gone(LEAVE_TIMEOUT) {
            Some(after) => format!(
                "the send returned {e}; {sid} left the bus {} later, so the {what} DID enter the bootloader (the disconnect cut the transfer short)",
                report::secs(after),
                sid = start.id_text(),
                what = start.what()
            ),
            None => {
                log.push(Step::new(
                    "2",
                    "enter bootloader (00:04 mode 1)",
                    at,
                    io.now(),
                    format!(
                        "FAILED: the send returned {e}, and {} did not leave the bus within {} s, so it did not enter",
                        start.id_text(),
                        LEAVE_TIMEOUT.as_secs()
                    ),
                ));
                return finish(log, Outcome::EnterFailed(e.to_string()), checkpoint);
            }
        },
        Ok(Ok(r))
            if matches!(
                r.status_for(ENTER_MODE),
                Status::Failed | Status::NotSupported | Status::Timeout
            ) =>
        {
            let why = status_name(&r, ENTER_MODE);
            log.push(Step::new(
                "2",
                "enter bootloader (00:04 mode 1)",
                at,
                io.now(),
                format!("sent; the device answered {why}"),
            ));
            return finish(log, Outcome::EnterFailed(why), checkpoint);
        }
        Ok(Ok(r)) => format!(
            "sent without error; reply {} args {}",
            status_name(&r, ENTER_MODE),
            args_hex(&r, 2)
        ),
        // a device that resets straight away often has no reply to give
        Ok(Err(e)) => format!("sent without error; no reply read ({e})"),
    };
        log.push(Step::new(
            "2",
            "enter bootloader (00:04 mode 1)",
            at,
            io.now(),
            detail,
        ));
        checkpoint(&log);
    } else {
        let at = io.now();
        for (id, what) in [
            ("1", "read version (00:87)"),
            ("2", "enter bootloader (00:04 mode 1)"),
        ] {
            log.push(Step::new(
                id,
                what,
                at,
                at,
                "SKIPPED: the run started from the bootloader; nothing is sent to a normal-mode node".into(),
            ));
        }
        checkpoint(&log);
    }

    // 3. wait for 1532:110E in the kernel's USB device list (a run that starts from the bootloader finds it at once and
    //    still lets it settle for 1000 ms, counted from when it was found)
    let at = io.now();
    let wait = io.wait_for_bootloader(BOOTLOADER_TIMEOUT);
    let detail = match (&wait.device, wait.appeared_at) {
        (Some(d), Some(seen)) => format!(
            "1532:110E listed as {} {} after the wait began{}",
            d.name,
            report::secs(seen.duration_since(at).unwrap_or_default()),
            match wait.settled_at {
                Some(t) => format!(
                    "; settled {} later, at {}, before usbfs is opened",
                    report::secs(t.duration_since(seen).unwrap_or_default()),
                    report::utc(t)
                ),
                None => String::new(),
            }
        ),
        _ => format!(
            "TIMED OUT: no 1532:110E within {} s. Nothing further was sent",
            BOOTLOADER_TIMEOUT.as_secs()
        ),
    };
    log.push(Step::new(
        "3",
        "wait for 1532:110E in /sys/bus/usb/devices",
        at,
        io.now(),
        detail,
    ));
    log.bootloader = wait.device.clone();
    log.appeared_at = wait.appeared_at;
    log.settled_at = wait.settled_at;
    log.timeout_nodes = wait.nodes;
    checkpoint(&log);
    let Some(device) = wait.device else {
        return finish(log, Outcome::BootloaderNeverAppeared, checkpoint);
    };

    // 3b. open the bootloader's node. Interface 0 with no driver (the dongle's bootloader): usbfs. Interface 0 bound to `usbhid`
    //     with exactly one hidraw node (the mouse's bootloader): that node. Any other driver, or `usbhid` without exactly one
    //     hidraw node, means nothing is sent, and nothing is detached.
    let at = io.now();
    let via_hidraw = match &device.interface0 {
        Interface0::Bound(driver) if driver == "usbhid" && device.hidraw.len() == 1 => true,
        Interface0::Bound(driver) => {
            let why = format!("the kernel driver `{driver}` is bound to interface 0");
            let nodes = if driver == "usbhid" {
                format!(
                    "it has {} hidraw node(s), not exactly one, ",
                    device.hidraw.len()
                )
            } else {
                String::new()
            };
            log.push(Step::new(
                "3b",
                "usbfs: open the node, read the report descriptor",
                at,
                io.now(),
                format!("REFUSED: {why}; {nodes}it is not detached and nothing is sent"),
            ));
            return finish(log, Outcome::DriverBound(why), checkpoint);
        }
        _ => false,
    };
    let what = if via_hidraw {
        "hidraw: open the node, read the report descriptor"
    } else {
        "usbfs: open the node, read the report descriptor"
    };
    let opened = if via_hidraw {
        match io.open_hidraw(&device) {
            Err(f) => {
                log.push(Step::new(
                    "3b",
                    what,
                    at,
                    io.now(),
                    format!("FAILED: {f}. Not escalated; nothing was sent to the bootloader"),
                ));
                let (why, permission) = (f.to_string(), f.is_permission_error());
                log.hidraw = Some(Err(f));
                return finish(
                    log,
                    Outcome::HidrawUnavailable { why, permission },
                    checkpoint,
                );
            }
            Ok(o) => {
                let text = format!(
                    "opened {}, HIDIOCGRAWINFO says {:04x}:{:04x}; nothing sent",
                    o.node, o.info.vendor, o.info.product
                );
                log.hidraw = Some(Ok(o));
                text
            }
        }
    } else {
        match io.open_usbfs(&device) {
            Err(f) => {
                log.push(Step::new(
                    "3b",
                    what,
                    at,
                    io.now(),
                    format!("FAILED: {f}. Not escalated; nothing was sent to the bootloader"),
                ));
                let (why, permission) = (f.to_string(), f.is_permission_error());
                log.usbfs = Some(Err(f));
                return finish(
                    log,
                    Outcome::UsbfsUnavailable { why, permission },
                    checkpoint,
                );
            }
            Ok(node) => {
                log.usbfs = Some(Ok(node));
                "opened the node, interface 0 claimed".to_string()
            }
        }
    };
    let descriptor = io.read_report_descriptor();
    let detail = match &descriptor {
        Ok(b) => format!("{opened}; the report descriptor is {} bytes", b.len()),
        // not needed for the exit: carry on
        Err(e) => format!("{opened}; reading the report descriptor FAILED: {e}"),
    };
    log.report_descriptor = Some(descriptor);
    log.push(Step::new("3b", what, at, io.now(), detail));
    checkpoint(&log);

    // 3c. the vendor's own first question, `10:80`, on this handle. Only a reply with status 2, txn 0, class/id 10:80 and
    //     a PID equal to the starting PID opens the gate; any other reply (or none) ends the run here with nothing further sent.
    let at = io.now();
    log.query_at = Some(at);
    let query = io.query_bootloader();
    let gets: usize = query.pairs.iter().map(|p| p.gets.len()).sum();
    let counts = format!(
        "{} SET_REPORT(s) of up to {QUERY_PAIRS}, {gets} GET_REPORT(s)",
        query.sets()
    );
    let verdict = gate(&query, origin);
    log.query = Some(query);
    log.gate = Some(verdict.clone());
    let pid = match verdict {
        Ok(pid) => pid,
        Err(why) => {
            log.push(Step::new(
                "3c",
                "query the bootloader (10:80)",
                at,
                io.now(),
                format!("{counts}; GATE CLOSED: {why}. 10:05 is NOT sent and nothing further is"),
            ));
            return finish(log, Outcome::NoExit(why), checkpoint);
        }
    };
    // the gate only opens for a PID that names an identity; from the bootloader that is the one step 4 waits for
    let Some(target) = Start::of((NORMAL_ID.0, pid)) else {
        let why = format!("the 10:80 reply names PID {pid:04x}, which is neither mouse identity");
        return finish(log, Outcome::NoExit(why), checkpoint);
    };
    let which = match origin {
        Origin::Normal(_) => "the starting PID",
        Origin::Bootloader(_) => "a mouse PID: step 4 waits for it",
    };
    if let Origin::Bootloader(_) = origin {
        origin = Origin::Bootloader(Some(target));
    }
    log.origin = origin;
    log.push(Step::new(
        "3c",
        "query the bootloader (10:80)",
        at,
        io.now(),
        format!(
            "{counts}; the reply is status 2, txn 0, class/id 10:80, PID {pid:04x} ({which}): gate open, 10:05 follows at once on the same handle"
        ),
    ));

    // 3d. exit, straight away: no checkpoint and no other call between the query and this (the vendor sends `10:05` within
    //     milliseconds of the `10:80` round trip, on the same handle). As in step 2, an error on the send is the expected
    //     shape if the device leaves the bus in the middle of it, so it counts as sent only if 1532:110E then leaves.
    let at = io.now();
    log.exit_sent_at = Some(at);
    let exit = io.exit_bootloader();
    let tries = if exit.sets() > 1 {
        format!(
            "{} SET_REPORTs of up to {EXIT_PAIRS}; the last: ",
            exit.sets()
        )
    } else {
        String::new()
    };
    let detail = match exit.set_error() {
        None => match exit.last_get() {
            Some(g) => match &g.reply {
                Ok(raw) => match Report::parse(raw) {
                    Ok(r) => format!(
                        "{tries}SET_REPORT accepted; GET_REPORT answered {} args {}",
                        status_name(&r, EXIT),
                        args_hex(&r, 2)
                    ),
                    Err(e) => format!("{tries}SET_REPORT accepted; GET_REPORT read {e}"),
                },
                Err(e) => format!("{tries}SET_REPORT accepted; GET_REPORT read nothing ({e})"),
            },
            None => format!("{tries}SET_REPORT accepted"),
        },
        Some(e) => match io.wait_for_bootloader_gone(LEAVE_TIMEOUT) {
            Some(after) => format!(
                "{tries}SET_REPORT returned {e}; 1532:110E left the bus {} later, so the exit took effect",
                report::secs(after)
            ),
            None => {
                let why = e.to_string();
                log.push(Step::new(
                    "3d",
                    "exit bootloader (10:05)",
                    at,
                    io.now(),
                    format!(
                        "FAILED: {tries}SET_REPORT returned {e}, and 1532:110E did not leave the bus within {} s",
                        LEAVE_TIMEOUT.as_secs()
                    ),
                ));
                log.exit = Some(exit);
                return finish(log, Outcome::ExitFailed(why), checkpoint);
            }
        },
    };
    log.exit = Some(exit);
    log.push(Step::new(
        "3d",
        "exit bootloader (10:05)",
        at,
        io.now(),
        detail,
    ));
    checkpoint(&log);

    // 4. wait for the starting identity (from the bootloader: the one the 10:80 reply named), counted from the exit send
    let at = io.now();
    let back = io.wait_for_return(target, RETURN_TIMEOUT);
    let detail = match back.back_at {
        Some(t) => format!(
            "{} listed again {} after the 10:05 send",
            target.id_text(),
            report::secs(
                t.duration_since(log.exit_sent_at.unwrap_or(at))
                    .unwrap_or_default()
            )
        ),
        None => format!(
            "TIMED OUT: no {} within {} s of 10:05, so {}",
            target.id_text(),
            RETURN_TIMEOUT.as_secs(),
            match origin {
                Origin::Normal(Start::Dongle) => "it must be replugged",
                _ => "the mouse may still be in its bootloader (a replug of the USB cable does not reset it)",
            }
        ),
    };
    log.push(Step::new(
        "4",
        &format!("wait for {}", target.id_text()),
        at,
        io.now(),
        detail,
    ));
    log.returned_at = back.back_at;
    log.after_nodes = back.nodes;
    let outcome = if back.back_at.is_some() {
        Outcome::Completed
    } else {
        Outcome::DidNotReturn
    };
    finish(log, outcome, checkpoint)
}

fn finish(mut log: ProbeLog, outcome: Outcome, checkpoint: &mut dyn FnMut(&ProbeLog)) -> ProbeLog {
    log.outcome = Some(outcome);
    checkpoint(&log);
    log
}

/// Other programs that would talk to the device while the probe does, as found by the caller (see `hw::collect_traffic`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Traffic {
    /// `(pid, command line)` of other razerctl processes: the remap or battery daemon, the GUI, another CLI call.
    pub razerctl: Vec<(u32, String)>,
    /// `(pid, command line)` of an openrazer daemon.
    pub openrazer_daemons: Vec<(u32, String)>,
    /// `(node, driver)`: the device's HID nodes bound to an openrazer kernel driver (`razermouse`, ...).
    pub openrazer_drivers: Vec<(String, String)>,
    /// `(pid, command line, node)`: processes that hold one of the device's nodes open.
    pub holders: Vec<(u32, String, String)>,
}

/// Why the probe must not start, one line each; empty = nothing in the way.
pub fn refusals(t: &Traffic) -> Vec<String> {
    let mut out = Vec::new();
    for (pid, cmd) in &t.razerctl {
        out.push(format!("another razerctl is running: pid {pid} `{cmd}` (stop the remap/battery services and close the GUI)"));
    }
    for (pid, cmd) in &t.openrazer_daemons {
        out.push(format!(
            "openrazer-daemon is running: pid {pid} `{cmd}` (stop it first)"
        ));
    }
    for (node, driver) in &t.openrazer_drivers {
        out.push(format!("the openrazer kernel driver `{driver}` is bound to {node} (unbind it or unload the module first)"));
    }
    for (pid, cmd, node) in &t.holders {
        out.push(format!("pid {pid} `{cmd}` has {node} open"));
    }
    out
}

/// The text printed when `--yes-enter-bootloader` is missing: what would happen, and what could go wrong.
pub fn plan_text() -> String {
    format!(
        "razerctl firmware probe-bootloader would, on the Razer dongle 1532:009F, on the mouse on its USB cable 1532:009E, or on a mouse that is already in its bootloader 1532:110E (exactly one of them must be connected):\n\
         \x20 1. read its version (00:87)                       [skipped when the mouse is already in its bootloader]\n\
         \x20 2. send enter-bootloader (00:04, mode 1); an error on that send counts as \"entered\" only if the device then leaves the bus\n\
         \x20    [skipped when the mouse is already in its bootloader]\n\
         \x20 3. wait up to {} s for the device to be listed as 1532:110E in /sys/bus/usb/devices, and write down what the kernel\n\
         \x20    says about it (bus and device number, bcdDevice, product, driver on interface 0, hidraw node, cached descriptors)\n\
         \x20 3b. after letting it settle for at least {} ms, open it and read its HID report descriptor, without detaching anything:\n\
         \x20    if no driver owns interface 0 (the dongle's bootloader), through its usbfs node (needs the udev rule); if usbhid owns it and\n\
         \x20    it has one hidraw node (the mouse's bootloader), through that node (read-only ioctls; no write to it)\n\
         \x20 3c. send the query Razer's updater sends first (10:80) as a HID feature report and read the answer (up to 5\n\
         \x20    reads, up to 3 sends); it asks the bootloader for its PID and changes nothing\n\
         \x20 3d. only if the reply has status 2 and the starting PID (009F for the dongle, 009E for the cable; 009E or 009F when the mouse\n\
         \x20    was already in its bootloader): send exit (10:05) straight away on the same handle (up to 3 sends); with any other reply\n\
         \x20    nothing more is sent and you are told how to get the mouse back\n\
         \x20 4. wait up to {} s for that identity to come back, and write down how long it took, from 10:05 and from entry\n\
         It never erases or writes firmware (no 10:01, 10:02 or class 0x0A frame exists in this command; the only frames it sends are\n\
         00:87, 00:04, 10:80 and 10:05). While it runs the mouse stops working. Expected worst case: the device stays in its\n\
         bootloader. Unplugging the dongle and plugging it back in resets the dongle; a replug of the mouse's USB cable does NOT reset the\n\
         mouse, which runs on its battery. Then switch the mouse off with the switch on its underside, wait 10 s and switch it on (not\n\
         verified), or use Razer's updater on Windows or in a VM. If the node cannot be opened, or a kernel driver other than usbhid owns\n\
         interface 0, nothing is sent to the bootloader.\n\
         Nothing was sent. To run it, add --yes-enter-bootloader.",
        BOOTLOADER_TIMEOUT.as_secs(),
        hw::SETTLE.as_millis(),
        RETURN_TIMEOUT.as_secs()
    )
}
