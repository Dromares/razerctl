//! The bootloader flash sequence of the vendor updater for the `Dev` images (`00:87` read version, `00:04` enter mode 1,
//! `10:01` erase, `10:02` program, `10:83` read back, `10:05` exit), as recovered statically in DAN-1879 and re-checked in
//! DAN-1885. It is written against the [`Transport`] trait and nothing else: **no implementation of that trait talks to
//! a device**, and none is allowed to without a new board decision (phase 3 of DAN-1878). The only implementations are
//! the simulated one behind `firmware flash --dry-run` and the recording ones in the tests. The phase-2 bootloader probe
//! is `crate::probe`: it does not use this trait and cannot send an erase or program frame.
//!
//! Quirks the vendor tool has and this code keeps on purpose (pinned by the golden tests): transaction id 0 (not razerctl's
//! usual 0x1f); the size byte of a Program frame is fixed at 8 whatever it carries; addresses are big-endian; every block
//! is read back and compared before `10:05`; and a mismatch means `10:05` is never sent.

use super::{Bootloader, FirmwareImage};
use crate::protocol::{Report, Status};

pub const READ_VERSION: (u8, u8) = (0x00, 0x87);
pub const ENTER_MODE: (u8, u8) = (0x00, 0x04);
pub const ERASE: (u8, u8) = (0x10, 0x01);
pub const PROGRAM: (u8, u8) = (0x10, 0x02);
pub const VERIFY: (u8, u8) = (0x10, 0x83);
pub const EXIT: (u8, u8) = (0x10, 0x05);

/// Transaction id of every frame (the vendor DLL never writes the byte).
pub const TID: u8 = 0;
/// Data bytes per Program packet and per read-back block (`PACKLEN`).
pub const PACKET: usize = 64;
/// Program frames carry this size byte, not the number of arguments.
pub const PROGRAM_SIZE_BYTE: u8 = 8;
/// A Program packet is sent once more if the device does not accept it (the vendor retries once).
const PROGRAM_TRIES: usize = 2;
/// A block is read back up to this many times before the verify counts as failed (the vendor allows 10).
const VERIFY_TRIES: usize = 10;
/// Where the data starts in a read-back reply's args: `[len][address u32 BE][data..]`.
const VERIFY_DATA_AT: usize = 5;

/// What the bootloader looked like when it appeared after `00:04`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootloaderSeen {
    pub vid: u16,
    pub pid: u16,
    /// `bcdDevice`, which carries the normal-mode PID of the device it belongs to.
    pub bcd: u16,
}

/// How the flash sequence reaches a device. The real world is not behind it yet.
pub trait Transport {
    /// Normal-mode (VID, PID) of the device on the other end.
    fn device_id(&self) -> (u16, u16);
    /// Send one frame (as a 91-byte feature report) and return the reply, after the vendor sender's own busy retries.
    fn exchange(&mut self, frame: &Report) -> Result<Report, String>;
    /// Block until a bootloader enumerates after `00:04`, and say what it is.
    fn wait_for_bootloader(&mut self, expect: &Bootloader) -> Result<BootloaderSeen, String>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum DfuError {
    /// Nothing was sent: the plan does not fit the device.
    Refused(String),
    /// The transport failed.
    Transport { step: &'static str, msg: String },
    /// The device answered a step with something other than success.
    Rejected { step: &'static str, status: String },
    /// Read-back differs from what was written. The device is left in its bootloader; `10:05` was not sent.
    VerifyMismatch { address: u32 },
}

impl std::fmt::Display for DfuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DfuError::Refused(m) => write!(f, "refused, nothing sent: {m}"),
            DfuError::Transport { step, msg } => write!(f, "{step}: {msg}"),
            DfuError::Rejected { step, status } => write!(f, "{step}: the device answered {status}"),
            DfuError::VerifyMismatch { address } => write!(
                f,
                "read-back differs at {address:#010x}; the bootloader was NOT told to exit (10:05 not sent)"
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u8, pub u8, pub u8);

impl Version {
    /// `1.05.00`, the form the updater's config uses.
    pub fn parse(s: &str) -> Option<Version> {
        let mut it = s.trim().split('.');
        let v = Version(
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
        );
        it.next().is_none().then_some(v)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{:02}.{:02}", self.0, self.1, self.2)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Flash even when the device already runs this version or a newer one.
    pub allow_reflash: bool,
}

/// What a completed run reports.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    pub device_version: Version,
    pub programmed: usize,
    pub verified: usize,
}

fn big_endian(a: u32) -> [u8; 4] {
    a.to_be_bytes()
}

pub fn read_version_frame() -> Report {
    Report::new(READ_VERSION, 8, TID)
}
pub fn enter_frame() -> Report {
    Report::new(ENTER_MODE, 2, TID).with_args(&[1, 0])
}
/// `[start u32 BE][end u32 BE]`, `end` being one past the last byte of the image.
pub fn erase_frame(start: u32, end: u32) -> Report {
    let mut a = [0u8; 8];
    a[..4].copy_from_slice(&big_endian(start));
    a[4..].copy_from_slice(&big_endian(end));
    Report::new(ERASE, 8, TID).with_args(&a)
}
/// `[len][address u32 BE][data]`, `data` padded with 0xFF to a whole packet. The size byte is fixed at 8.
pub fn program_frame(address: u32, data: &[u8]) -> Report {
    assert!(
        data.len() <= PACKET,
        "a Program packet carries at most {PACKET} bytes"
    );
    let mut a = [0xFFu8; 1 + 4 + PACKET];
    a[0] = PACKET as u8;
    a[1..5].copy_from_slice(&big_endian(address));
    a[5..5 + data.len()].copy_from_slice(data);
    Report::new(PROGRAM, PROGRAM_SIZE_BYTE, TID).with_args(&a)
}
/// `[len][address u32 BE]`.
pub fn verify_frame(address: u32) -> Report {
    let mut a = [0u8; 5];
    a[0] = PACKET as u8;
    a[1..].copy_from_slice(&big_endian(address));
    Report::new(VERIFY, 5, TID).with_args(&a)
}
pub fn exit_frame() -> Report {
    Report::new(EXIT, 0, TID)
}

struct Run<'a, T: Transport> {
    t: &'a mut T,
}

impl<T: Transport> Run<'_, T> {
    /// One exchange that has to end in success.
    fn call(&mut self, step: &'static str, frame: &Report) -> Result<Report, DfuError> {
        let reply = self
            .t
            .exchange(frame)
            .map_err(|msg| DfuError::Transport { step, msg })?;
        match reply.status_for((frame.command_class, frame.command_id)) {
            Status::Success if reply.transaction_id == frame.transaction_id => Ok(reply),
            Status::Success => Err(DfuError::Rejected {
                step,
                status: format!(
                    "success for transaction {:#04x}, not {:#04x}",
                    reply.transaction_id, frame.transaction_id
                ),
            }),
            other => Err(DfuError::Rejected {
                step,
                status: format!("{other:?}"),
            }),
        }
    }
}

/// Run the whole sequence for `image` against `t`. Every refusal that can be decided before the device is touched
/// happens before the first frame; the checks that need the device (version, bootloader identity) happen before `10:01`.
pub fn flash<T: Transport>(
    t: &mut T,
    image: &FirmwareImage,
    opts: &Options,
) -> Result<Outcome, DfuError> {
    let bl = image.bootloader.as_ref().ok_or_else(|| {
        DfuError::Refused(format!(
            "image {} is not written through the bootloader (it is a class 0x0A stream)",
            image.name
        ))
    })?;
    if image.image.data.is_empty() {
        return Err(DfuError::Refused(format!("image {} is empty", image.name)));
    }
    let want = Version::parse(&image.version).ok_or_else(|| {
        DfuError::Refused(format!("image version '{}' is not X.YY.ZZ", image.version))
    })?;
    let (vid, pid) = t.device_id();
    if (vid, pid) != (image.vid, image.pid) {
        return Err(DfuError::Refused(format!(
            "image {} is for {:04x}:{:04x}, the device is {vid:04x}:{pid:04x}",
            image.name, image.vid, image.pid
        )));
    }
    let end = u32::try_from(image.image.end())
        .map_err(|_| DfuError::Refused("image runs past the 32-bit address space".into()))?;

    let mut run = Run { t };

    // 00:87. Args 0..3 carry major, minor, patch of the application image (DAN-1879; not checked on a device).
    let reply = run.call("read version", &read_version_frame())?;
    let have = Version(reply.args[0], reply.args[1], reply.args[2]);
    if have >= want && !opts.allow_reflash {
        return Err(DfuError::Refused(format!(
            "the device already runs {have}, the image is {want} (pass --allow-reflash to write it anyway)"
        )));
    }

    // 00:04 [1, 0], then wait for the bootloader and check whose it is before anything is erased.
    run.call("enter bootloader", &enter_frame())?;
    let seen = run
        .t
        .wait_for_bootloader(bl)
        .map_err(|msg| DfuError::Transport {
            step: "wait for bootloader",
            msg,
        })?;
    if (seen.vid, seen.pid) != (bl.vid, bl.pid) || seen.bcd != bl.bcd {
        return Err(DfuError::Refused(format!(
            "the bootloader that appeared is {:04x}:{:04x} bcdDevice {:04x}, image {} needs {:04x}:{:04x} bcdDevice {:04x}; \
             nothing was erased, the device is still in its bootloader",
            seen.vid, seen.pid, seen.bcd, image.name, bl.vid, bl.pid, bl.bcd
        )));
    }

    run.call("erase", &erase_frame(image.image.start, end))?;

    let mut programmed = 0;
    for (i, chunk) in image.image.data.chunks(PACKET).enumerate() {
        let frame = program_frame(image.image.start + (i * PACKET) as u32, chunk);
        let mut result = run.call("program", &frame);
        for _ in 1..PROGRAM_TRIES {
            if result.is_ok() {
                break;
            }
            result = run.call("program", &frame);
        }
        result?;
        programmed += 1;
    }

    let mut verified = 0;
    for (i, chunk) in image.image.data.chunks(PACKET).enumerate() {
        let address = image.image.start + (i * PACKET) as u32;
        let mut want_block = [0xFFu8; PACKET];
        want_block[..chunk.len()].copy_from_slice(chunk);
        let mut ok = false;
        for _ in 0..VERIFY_TRIES {
            let reply = run.call("verify", &verify_frame(address))?;
            if reply.args[VERIFY_DATA_AT..VERIFY_DATA_AT + PACKET] == want_block {
                ok = true;
                break;
            }
        }
        if !ok {
            return Err(DfuError::VerifyMismatch { address });
        }
        verified += 1;
    }

    run.call("exit bootloader", &exit_frame())?;
    Ok(Outcome {
        device_version: have,
        programmed,
        verified,
    })
}
