//! `razerctl firmware probe-bootloader` (DAN-1908 phase 2, DAN-1929 phase 2b of DAN-1878): put the dongle into its
//! bootloader, look at how the bootloader shows up on Linux, and take it out again. **It never erases and never writes**:
//! the only frames this module can emit are the three below, and everything else is refused structurally.
//!
//! * [`read_version_frame`] `00:87` and [`enter_frame`] `00:04` mode 1 go out through hidraw (`hw.rs`): [`permitted`]
//!   accepts exactly these two byte-for-byte, and [`hw::Guarded`] refuses anything else before it reaches the kernel.
//! * [`exit_frame`] `10:05` goes out through usbfs (`usbfs.rs`), because the bootloader (`1532:110E`) has no interrupt
//!   endpoint and the kernel makes no hidraw node for it. [`usbfs::permitted`] accepts exactly three control requests: the
//!   HID report descriptor read, the `10:05` feature SET_REPORT and its GET_REPORT. [`usbfs::UsbGuarded`] refuses
//!   anything else before the ioctl.
//! * The DFU trait of phase 1 (`firmware::dfu::Transport`: erase, program, verify) is not implemented by anything in
//!   here, and nothing in `probe` names `firmware::dfu`, `firmware::flash` or the erase/program/class `0x0A` commands.
//!   `cargo test` fails (`tests/probe.rs`) if a real-transport module could reach them.
//! * The orchestration ([`run`]) is written against [`ProbeIo`]; the tests drive it with a simulated device only.
//!
//! Layout: `mod.rs` (frames, [`ProbeIo`], [`run`], the refusal rules), `report.rs` (the file the board pastes onto
//! DAN-1878), `sysfs.rs` (read-only look at `/sys/bus/usb/devices`), `usbfs.rs` (the control-transfer transport and its
//! allow-list), `hw.rs` (the real [`ProbeIo`]), `cli.rs` (argument handling, refusals, output file).

pub mod cli;
pub mod hw;
pub mod report;
pub mod sysfs;
pub mod usbfs;

use crate::protocol::{Report, Status, REPORT_LEN};
use report::{
    BootloaderWait, ExitResult, OpenFailure, Outcome, ProbeLog, ReturnWait, SendError, Step,
};
use std::time::{Duration, SystemTime};
use sysfs::{Interface0, UsbDevice};

pub const READ_VERSION: (u8, u8) = (0x00, 0x87);
pub const ENTER_MODE: (u8, u8) = (0x00, 0x04);
pub const EXIT: (u8, u8) = (0x10, 0x05);
/// Transaction id of every frame: the vendor updater never writes the byte (same as phase 1).
pub const TID: u8 = 0;

/// The dongle in normal mode (Viper Mini SE).
pub const NORMAL_ID: (u16, u16) = (0x1532, 0x009F);
/// The same dongle in its bootloader.
pub const BOOTLOADER_ID: (u16, u16) = (0x1532, 0x110E);

/// How long step 3 waits for `1532:110E` before it gives up and tells the user to replug. The board's kernel log has it
/// enumerated about a second after the enter frame.
pub const BOOTLOADER_TIMEOUT: Duration = Duration::from_secs(10);
/// How long steps 2 and 3c wait for a device to leave the bus after a send that returned an error.
pub const LEAVE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long step 4 waits for `1532:009F` to come back, counted from the exit send. If `10:05` works it is back within
/// seconds. Nothing shows that the bootloader ever leaves by itself (the return on the board's first run was a replug),
/// so a timeout here means the dongle has to be replugged.
pub const RETURN_TIMEOUT: Duration = Duration::from_secs(30);

/// What to tell the user when the usbfs node of the bootloader cannot be opened.
pub const UDEV_HINT: &str = "Reinstall the udev rule, which now has a line for the bootloader (1532:110E): sudo cp udev/70-razerctl.rules /etc/udev/rules.d/ && sudo udevadm control --reload && sudo udevadm trigger";
pub const REPLUG: &str = "Unplug the dongle and plug it back in. The firmware was not erased or written, so the mouse should come back as 1532:009F.";

/// `00:87`: read the version. Args 0..3 carry major, minor, patch (DAN-1879; not confirmed on a device).
pub fn read_version_frame() -> Report {
    Report::new(READ_VERSION, 8, TID)
}
/// `00:04` with args `[1, 0]`: switch to bootloader mode.
pub fn enter_frame() -> Report {
    Report::new(ENTER_MODE, 2, TID).with_args(&[1, 0])
}
/// `10:05`: leave the bootloader.
pub fn exit_frame() -> Report {
    Report::new(EXIT, 0, TID)
}

/// The hidraw allow-list: the two normal-mode frames, byte for byte. (`10:05` is not on it: the bootloader has no hidraw
/// node, so it only goes out through [`usbfs`].)
pub fn permitted(bytes: &[u8; REPORT_LEN]) -> bool {
    [read_version_frame(), enter_frame()]
        .iter()
        .any(|f| f.to_bytes() == *bytes)
}

/// What the follow-up read after a send produced: the device's answer, or why there was none.
pub type Answer = Result<Report, String>;

/// Everything the probe does to the world. A real device is behind it only in [`hw`]; there is no method that takes a
/// frame or a control request, so an implementation cannot be asked to send anything but the probe's own.
pub trait ProbeIo {
    fn now(&self) -> SystemTime;
    /// Which node and interface the normal-mode commands go through.
    fn describe_device(&self) -> String;
    /// Step 1: send `00:87` and return the reply (a failed exchange is an error).
    fn read_version(&mut self) -> Result<Report, String>;
    /// Step 2: send `00:04` mode 1. `Err` = the send returned an error (an `EPROTO` here can mean the device left the bus
    /// mid-transfer because it obeyed); `Ok` carries what the follow-up read gave.
    fn enter_bootloader(&mut self) -> Result<Answer, SendError>;
    /// Step 2, after a send that returned an error: wait up to `timeout` for `1532:009F` to leave the bus. `Some` = it
    /// left, that long after the call.
    fn wait_for_dongle_gone(&mut self, timeout: Duration) -> Option<Duration>;
    /// Step 3: watch `/sys/bus/usb/devices` for `1532:110E` for at most `timeout`.
    fn wait_for_bootloader(&mut self, timeout: Duration) -> BootloaderWait;
    /// Step 3b: open the bootloader's usbfs node and claim interface 0. `Ok` names the node; `Err` says why not (the
    /// errno is recorded). Sends nothing.
    fn open_usbfs(&mut self, device: &UsbDevice) -> Result<String, OpenFailure>;
    /// Step 3b: read the HID report descriptor of interface 0 (a standard GET_DESCRIPTOR).
    fn read_report_descriptor(&mut self) -> Result<Vec<u8>, SendError>;
    /// Step 3c: send `10:05` as a feature SET_REPORT, then read its GET_REPORT.
    fn exit_bootloader(&mut self) -> ExitResult;
    /// Step 3c, after an exit request that returned an error: wait up to `timeout` for `1532:110E` to leave the bus.
    fn wait_for_bootloader_gone(&mut self, timeout: Duration) -> Option<Duration>;
    /// Step 4: watch for `1532:009F` to come back (and no `110E` to be left) for at most `timeout`.
    fn wait_for_return(&mut self, timeout: Duration) -> ReturnWait;
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
    let mut log = ProbeLog::new(build.clone(), io.now(), io.describe_device());
    checkpoint(&log);

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

    // 2. enter the bootloader. A send that returns an error counts as "entered" only if 1532:009F then leaves the bus:
    //    on the board's first run the kernel log showed the disconnect landing in the middle of the transfer, and that
    //    cut-off transfer was the error (EPROTO).
    let at = io.now();
    log.entered_at = Some(at);
    let detail = match io.enter_bootloader() {
        Err(e) => match io.wait_for_dongle_gone(LEAVE_TIMEOUT) {
            Some(after) => format!(
                "the send returned {e}; 1532:009F left the bus {} later, so the dongle DID enter the bootloader (the disconnect cut the transfer short)",
                report::secs(after)
            ),
            None => {
                log.push(Step::new(
                    "2",
                    "enter bootloader (00:04 mode 1)",
                    at,
                    io.now(),
                    format!(
                        "FAILED: the send returned {e}, and 1532:009F did not leave the bus within {} s, so it did not enter",
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

    // 3. wait for 1532:110E in the kernel's USB device list (it has no hidraw node)
    let at = io.now();
    let wait = io.wait_for_bootloader(BOOTLOADER_TIMEOUT);
    let detail = match (&wait.device, wait.appeared_at) {
        (Some(d), Some(seen)) => format!(
            "1532:110E listed as {} {} after the wait began",
            d.name,
            report::secs(seen.duration_since(at).unwrap_or_default())
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
    checkpoint(&log);
    let Some(device) = wait.device else {
        return finish(log, Outcome::BootloaderNeverAppeared, checkpoint);
    };

    // 3b. usbfs: a kernel driver on interface 0 or a node that will not open means nothing is sent
    let at = io.now();
    if let Interface0::Bound(driver) = &device.interface0 {
        let why = format!("the kernel driver `{driver}` is bound to interface 0");
        log.push(Step::new(
            "3b",
            "usbfs: open the node, read the report descriptor",
            at,
            io.now(),
            format!("REFUSED: {why}; it is not detached and nothing is sent"),
        ));
        return finish(log, Outcome::DriverBound(why), checkpoint);
    }
    match io.open_usbfs(&device) {
        Err(f) => {
            log.push(Step::new(
                "3b",
                "usbfs: open the node, read the report descriptor",
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
        Ok(node) => log.usbfs = Some(Ok(node)),
    }
    let descriptor = io.read_report_descriptor();
    let detail = match &descriptor {
        Ok(b) => format!(
            "opened the node, interface 0 claimed; the report descriptor is {} bytes",
            b.len()
        ),
        // not needed for the exit: carry on
        Err(e) => format!(
            "opened the node, interface 0 claimed; reading the report descriptor FAILED: {e}"
        ),
    };
    log.report_descriptor = Some(descriptor);
    log.push(Step::new(
        "3b",
        "usbfs: open the node, read the report descriptor",
        at,
        io.now(),
        detail,
    ));
    checkpoint(&log);

    // 3c. exit. As in step 2, an error on the send is the expected shape if the device leaves the bus in the middle of
    //     it, so it counts as sent only if 1532:110E then leaves.
    let at = io.now();
    log.exit_sent_at = Some(at);
    let exit = io.exit_bootloader();
    let detail = match &exit.sent {
        Ok(()) => match &exit.status {
            Some(Ok(r)) => format!(
                "SET_REPORT accepted; GET_REPORT answered {} args {}",
                status_name(r, EXIT),
                args_hex(r, 2)
            ),
            Some(Err(e)) => format!("SET_REPORT accepted; GET_REPORT read nothing ({e})"),
            None => "SET_REPORT accepted".into(),
        },
        Err(e) => match io.wait_for_bootloader_gone(LEAVE_TIMEOUT) {
            Some(after) => format!(
                "SET_REPORT returned {e}; 1532:110E left the bus {} later, so the exit took effect",
                report::secs(after)
            ),
            None => {
                log.push(Step::new(
                    "3c",
                    "exit bootloader (10:05)",
                    at,
                    io.now(),
                    format!(
                        "FAILED: SET_REPORT returned {e}, and 1532:110E did not leave the bus within {} s",
                        LEAVE_TIMEOUT.as_secs()
                    ),
                ));
                let why = e.to_string();
                log.exit = Some(exit);
                return finish(log, Outcome::ExitFailed(why), checkpoint);
            }
        },
    };
    log.exit = Some(exit);
    log.push(Step::new(
        "3c",
        "exit bootloader (10:05)",
        at,
        io.now(),
        detail,
    ));
    checkpoint(&log);

    // 4. wait for 1532:009F, counted from the exit send
    let at = io.now();
    let back = io.wait_for_return(RETURN_TIMEOUT);
    let detail = match back.back_at {
        Some(t) => format!(
            "1532:009F listed again {} after the 10:05 send",
            report::secs(
                t.duration_since(log.exit_sent_at.unwrap_or(at))
                    .unwrap_or_default()
            )
        ),
        None => format!(
            "TIMED OUT: no 1532:009F within {} s of 10:05, so it must be replugged",
            RETURN_TIMEOUT.as_secs()
        ),
    };
    log.push(Step::new("4", "wait for 1532:009F", at, io.now(), detail));
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
        "razerctl firmware probe-bootloader would, on the Razer dongle 1532:009F:\n\
         \x20 1. read its version (00:87)\n\
         \x20 2. send enter-bootloader (00:04, mode 1); an error on that send counts as \"entered\" only if the dongle then leaves the bus\n\
         \x20 3. wait up to {} s for the dongle to be listed as 1532:110E in /sys/bus/usb/devices, and write down what the kernel\n\
         \x20    says about it (bus and device number, bcdDevice, product, driver on interface 0, cached descriptors)\n\
         \x20 3b. open its usbfs node (needs the udev rule), claim interface 0 and read its HID report descriptor\n\
         \x20 3c. send exit (10:05) as a HID feature SET_REPORT and read its GET_REPORT\n\
         \x20 4. wait up to {} s for 1532:009F to come back, and write down how long it took, from 10:05 and from entry\n\
         It never erases or writes firmware (no 10:01, 10:02 or class 0x0A frame exists in this command). While it runs the\n\
         mouse stops working. Expected worst case: the dongle stays in bootloader mode until you unplug it and plug it back\n\
         in. If the node cannot be opened,\n\
         or a kernel driver owns interface 0, nothing is sent to the bootloader.\n\
         Nothing was sent. To run it, add --yes-enter-bootloader.",
        BOOTLOADER_TIMEOUT.as_secs(),
        RETURN_TIMEOUT.as_secs()
    )
}
