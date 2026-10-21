//! `razerctl firmware probe-bootloader` (DAN-1908, phase 2 of DAN-1878): put the dongle into its bootloader, look at how
//! the bootloader shows up on Linux, and take it out again. **It never erases and never writes**: the only frames this
//! module can emit are the three below, and everything else is refused structurally.
//!
//! * [`read_version_frame`] `00:87`, [`enter_frame`] `00:04` mode 1, [`exit_frame`] `10:05`: [`permitted`] accepts exactly
//!   these three byte-for-byte, and [`hw::Guarded`], the only path from this module to a real device, refuses anything else
//!   before it reaches the kernel.
//! * The DFU trait of phase 1 (`firmware::dfu::Transport`: erase, program, verify) is not implemented by anything in
//!   here, and nothing in `probe` names `firmware::dfu`, `firmware::flash` or the erase/program/class `0x0A` commands.
//!   `tests/probe.rs` fails the build if a real-transport module could reach them.
//! * The orchestration ([`run`]) is written against [`ProbeIo`]; the tests drive it with a simulated device only.
//!
//! Layout: `mod.rs` (frames, the guard, [`ProbeIo`], [`run`], the refusal rules), `report.rs` (the file the board pastes
//! onto DAN-1878), `hw.rs` (the one real transport), `cli.rs` (argument handling, refusals, output file).

pub mod cli;
pub mod hw;
pub mod report;

use crate::protocol::{Report, Status, REPORT_LEN};
use report::{BootloaderWait, Outcome, ProbeLog, ReturnWait, Step};
use std::time::{Duration, SystemTime};

pub const READ_VERSION: (u8, u8) = (0x00, 0x87);
pub const ENTER_MODE: (u8, u8) = (0x00, 0x04);
pub const EXIT: (u8, u8) = (0x10, 0x05);
/// Transaction id of every frame: the vendor updater never writes the byte (same as phase 1).
pub const TID: u8 = 0;

/// The dongle in normal mode (Viper Mini SE).
pub const NORMAL_ID: (u16, u16) = (0x1532, 0x009F);
/// The same dongle in its bootloader.
pub const BOOTLOADER_ID: (u16, u16) = (0x1532, 0x110E);

/// How long step 3 waits for `1532:110E` before it gives up and tells the user to replug.
pub const BOOTLOADER_TIMEOUT: Duration = Duration::from_secs(20);
/// How long step 5 waits for `1532:009F` to come back.
pub const RETURN_TIMEOUT: Duration = Duration::from_secs(30);

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

/// The whole allow-list: the three probe frames, byte for byte.
pub fn permitted(bytes: &[u8; REPORT_LEN]) -> bool {
    [read_version_frame(), enter_frame(), exit_frame()]
        .iter()
        .any(|f| f.to_bytes() == *bytes)
}

/// What the follow-up read after a send produced: the device's answer, or why there was none.
pub type Answer = Result<Report, String>;

/// Everything the probe does to the world. A real device is behind it only in [`hw`]; there is no method that takes a
/// frame, so an implementation cannot be asked to send anything but the three probe frames.
pub trait ProbeIo {
    fn now(&self) -> SystemTime;
    /// Which node and interface the normal-mode commands go through.
    fn describe_device(&self) -> String;
    /// Step 1: send `00:87` and return the reply (a failed exchange is an error).
    fn read_version(&mut self) -> Result<Report, String>;
    /// Step 2: send `00:04` mode 1. `Err` = it could not be written; `Ok` carries what the follow-up read gave.
    fn enter_bootloader(&mut self) -> Result<Answer, String>;
    /// Step 3: watch for `1532:110E` for at most `timeout`.
    fn wait_for_bootloader(&mut self, timeout: Duration) -> BootloaderWait;
    /// Step 4: open the bootloader's node for writing. `Ok` names it; `Err` says why not (the errno is recorded).
    fn open_bootloader(&mut self) -> Result<String, report::OpenFailure>;
    /// Step 5a: send `10:05` through the node opened in step 4.
    fn exit_bootloader(&mut self) -> Result<Answer, String>;
    /// Step 5b: watch for `1532:009F` to come back for at most `timeout`.
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

/// Run the five steps. `checkpoint` is called with the log after every step, so a file written from it is current even
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
            log.push(Step::new(1, "read version (00:87)", at, io.now(), format!("FAILED: {e}")));
            return finish(log, Outcome::ReadVersionFailed(e), checkpoint);
        }
        Ok(r) if r.status_for(READ_VERSION) != Status::Success => {
            let why = status_name(&r, READ_VERSION);
            log.push(Step::new(1, "read version (00:87)", at, io.now(), format!("FAILED: the device answered {why}")));
            return finish(log, Outcome::ReadVersionFailed(why), checkpoint);
        }
        Ok(r) => {
            let v = format!("{}.{:02}.{:02}", r.args[0], r.args[1], r.args[2]);
            (format!("ok, version {v} (reply args {})", args_hex(&r, 8)), v)
        }
    };
    log.version_read = Some(version);
    log.push(Step::new(1, "read version (00:87)", at, io.now(), detail));
    checkpoint(&log);

    // 2. enter the bootloader
    let at = io.now();
    let detail = match io.enter_bootloader() {
        Err(e) => {
            log.push(Step::new(2, "enter bootloader (00:04 mode 1)", at, io.now(), format!("FAILED to send: {e}")));
            return finish(log, Outcome::EnterFailed(e), checkpoint);
        }
        Ok(Ok(r)) if matches!(r.status_for(ENTER_MODE), Status::Failed | Status::NotSupported | Status::Timeout) => {
            let why = status_name(&r, ENTER_MODE);
            log.push(Step::new(2, "enter bootloader (00:04 mode 1)", at, io.now(), format!("sent; the device answered {why}")));
            return finish(log, Outcome::EnterFailed(why), checkpoint);
        }
        Ok(Ok(r)) => format!("sent; reply {} args {}", status_name(&r, ENTER_MODE), args_hex(&r, 2)),
        // a device that resets straight away often has no reply to give
        Ok(Err(e)) => format!("sent; no reply read ({e})"),
    };
    log.push(Step::new(2, "enter bootloader (00:04 mode 1)", at, io.now(), detail));
    checkpoint(&log);

    // 3. wait for 1532:110E
    let at = io.now();
    let wait = io.wait_for_bootloader(BOOTLOADER_TIMEOUT);
    let appeared = wait.seen.iter().any(|e| (e.vid, e.pid) == BOOTLOADER_ID);
    let detail = match wait.appeared_after {
        Some(d) if appeared => format!("1532:110E appeared {:.2} s after the wait began", d.as_secs_f64()),
        _ => format!(
            "TIMED OUT: no 1532:110E within {} s. Nothing further was sent",
            BOOTLOADER_TIMEOUT.as_secs()
        ),
    };
    log.push(Step::new(3, "wait for 1532:110E", at, io.now(), detail));
    log.enumerations = wait.seen;
    log.appeared_after = wait.appeared_after;
    checkpoint(&log);
    if !appeared {
        return finish(log, Outcome::BootloaderNeverAppeared, checkpoint);
    }

    // 4. open the node the bootloader showed up on (node, interface, bcdDevice and descriptor are the enumerations
    //    above; this step records whether the node can be opened without root)
    let at = io.now();
    match io.open_bootloader() {
        Err(f) => {
            let detail = format!("FAILED: {f}. Not escalated; nothing was sent to the bootloader");
            log.push(Step::new(4, "open the 1532:110E node", at, io.now(), detail));
            let why = f.to_string();
            log.open = Some(Err(f));
            return finish(log, Outcome::OpenFailed(why), checkpoint);
        }
        Ok(node) => {
            let detail = format!("ok, opened {node} as the current user (no root needed)");
            log.push(Step::new(4, "open the 1532:110E node", at, io.now(), detail));
            log.open = Some(Ok(node));
        }
    }
    checkpoint(&log);

    // 5. exit, and wait for 1532:009F
    let at = io.now();
    let detail = match io.exit_bootloader() {
        Err(e) => {
            log.push(Step::new(5, "exit bootloader (10:05)", at, io.now(), format!("FAILED to send: {e}")));
            return finish(log, Outcome::ExitFailed(e), checkpoint);
        }
        Ok(Ok(r)) => format!("sent; reply {} args {}", status_name(&r, EXIT), args_hex(&r, 2)),
        Ok(Err(e)) => format!("sent; no reply read ({e})"),
    };
    log.push(Step::new(5, "exit bootloader (10:05)", at, io.now(), detail));
    checkpoint(&log);

    let at = io.now();
    let back = io.wait_for_return(RETURN_TIMEOUT);
    let detail = match back.after {
        Some(d) => format!("1532:009F came back by itself {:.2} s after 10:05 was sent", d.as_secs_f64()),
        None => format!(
            "TIMED OUT: no 1532:009F within {} s of 10:05, so it did NOT come back by itself",
            RETURN_TIMEOUT.as_secs()
        ),
    };
    log.push(Step::new(5, "wait for 1532:009F", at, io.now(), detail));
    log.returned_after = back.after;
    log.after_nodes = back.nodes;
    let outcome = if back.after.is_some() {
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
        out.push(format!("openrazer-daemon is running: pid {pid} `{cmd}` (stop it first)"));
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
         \x20 2. send enter-bootloader (00:04, mode 1)\n\
         \x20 3. wait up to {} s for the dongle to reappear as 1532:110E\n\
         \x20 4. record its hidraw node, interface, bcdDevice and report descriptor, and whether the node opens without root\n\
         \x20 5. send exit (10:05) and wait up to {} s for 1532:009F to come back\n\
         It never erases or writes firmware (no 10:01, 10:02 or class 0x0A frame exists in this command). While it runs the\n\
         mouse stops working. Expected worst case: the dongle stays in bootloader mode until you unplug it and plug it back\n\
         in. Whether a replug always recovers it has not been confirmed on Linux; if it does not, Razer's own updater on\n\
         Windows is the fallback (that it can talk to a dongle in bootloader mode is inferred, not tested).\n\
         Nothing was sent. To run it, add --yes-enter-bootloader.",
        BOOTLOADER_TIMEOUT.as_secs(),
        RETURN_TIMEOUT.as_secs()
    )
}

pub const REPLUG: &str = "Unplug the dongle and plug it back in. The firmware was not erased or written, so the mouse should come back as 1532:009F.";
