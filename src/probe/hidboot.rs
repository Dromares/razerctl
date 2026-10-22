//! The probe's third transport (DAN-1948, phase 2f of DAN-1878): the hidraw node of a `1532:110E` bootloader that `usbhid`
//! has bound. The mouse's bootloader (bcdDevice `0x0200`) has an interrupt IN endpoint, so the kernel binds `usbhid` to it
//! and makes it a hidraw node; the dongle's bootloader has none of that and keeps the usbfs path (`usbfs.rs`).
//!
//! **It can send two frames and nothing else**: `10:80` and `10:05`, as HID feature reports (`HIDIOCSFEATURE`, report id
//! `0x00` followed by the same 90 bytes the usbfs path sends), and read the answer with `HIDIOCGFEATURE`. The fence, in the
//! same three layers as `hw.rs` and `usbfs.rs`:
//!
//! 1. A bootloader node accepts five ioctls and no `write(2)`: `HIDIOCGRAWINFO`, `HIDIOCGRDESCSIZE`, `HIDIOCGRDESC`,
//!    `HIDIOCSFEATURE` and `HIDIOCGFEATURE`. They are the five methods of [`BootHid`]; `src/hidraw.rs` has no other ioctl
//!    and no output-report write, and `tests/probe.rs` reads both files for exactly that.
//! 2. [`open`] hands out a node only inside a [`BootGuarded`], whose `send_feature` refuses, before the ioctl, every
//!    payload that is not the `10:80` query frame or the `10:05` exit frame, byte for byte ([`permitted`]). So `10:01`
//!    (erase), `10:02` (program), a class `0x0A` frame, the normal-mode frames and every other class `0x10` frame cannot
//!    get out on this path either.
//! 3. Nothing here names the phase-1 DFU module, its trait or the erase/program/class `0x0A` commands; the fence tests read
//!    the source for all of it.

use super::hw::BootChannel;
use super::{exit_frame, query_frame};
use crate::hidraw::{Hidraw, RawInfo};
use crate::protocol::REPORT_LEN;
use std::io;
use std::path::Path;

/// The five operations a bootloader node supports. [`Hidraw`] is the real one; the tests put a recording node under
/// [`BootGuarded`] to show what does and does not get through.
pub trait BootHid {
    /// `HIDIOCGRAWINFO`.
    fn raw_info(&self) -> io::Result<RawInfo>;
    /// `HIDIOCGRDESCSIZE` and `HIDIOCGRDESC`.
    fn report_descriptor(&self) -> io::Result<Vec<u8>>;
    /// `HIDIOCSFEATURE`: report id 0 and these 90 bytes.
    fn send_feature(&self, payload: &[u8; REPORT_LEN]) -> io::Result<()>;
    /// `HIDIOCGFEATURE`: the number of bytes the kernel returned and the 91-byte buffer (report id 0 first).
    fn get_feature(&self) -> io::Result<(usize, [u8; REPORT_LEN + 1])>;
}

impl BootHid for Hidraw {
    fn raw_info(&self) -> io::Result<RawInfo> {
        Hidraw::raw_info(self)
    }
    fn report_descriptor(&self) -> io::Result<Vec<u8>> {
        Hidraw::report_descriptor(self)
    }
    fn send_feature(&self, payload: &[u8; REPORT_LEN]) -> io::Result<()> {
        Hidraw::send_feature(self, payload)
    }
    fn get_feature(&self) -> io::Result<(usize, [u8; REPORT_LEN + 1])> {
        Hidraw::get_feature_len(self)
    }
}

/// The hidraw allow-list of a bootloader node: the `10:80` query frame and the `10:05` exit frame, byte for byte.
pub fn permitted(bytes: &[u8; REPORT_LEN]) -> bool {
    *bytes == query_frame().to_bytes() || *bytes == exit_frame().to_bytes()
}

/// A bootloader node that can only carry the frames of [`permitted`].
pub struct BootGuarded<T: BootHid>(pub T);

impl<T: BootHid> BootHid for BootGuarded<T> {
    fn raw_info(&self) -> io::Result<RawInfo> {
        self.0.raw_info()
    }
    fn report_descriptor(&self) -> io::Result<Vec<u8>> {
        self.0.report_descriptor()
    }
    fn send_feature(&self, payload: &[u8; REPORT_LEN]) -> io::Result<()> {
        if !permitted(payload) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refused: the bootloader probe may only send 10:80 and 10:05 over the bootloader's hidraw node",
            ));
        }
        self.0.send_feature(payload)
    }
    fn get_feature(&self) -> io::Result<(usize, [u8; REPORT_LEN + 1])> {
        self.0.get_feature()
    }
}

/// Open the node read/write as the current user. Nothing is sent. The caller is never given a bare [`Hidraw`].
pub fn open(path: &Path) -> io::Result<BootGuarded<Hidraw>> {
    Ok(BootGuarded(Hidraw::open(path)?))
}

/// Read the feature report that answers the last SET, as the 90 bytes the device returned (the leading report id is
/// dropped). A reply of any other length is an error: nothing downstream reads fields out of a short buffer.
pub fn read_reply<T: BootHid>(io: &BootGuarded<T>) -> io::Result<Vec<u8>> {
    let (n, buf) = io.get_feature()?;
    if n != REPORT_LEN + 1 {
        return Err(io::Error::other(format!(
            "the HIDIOCGFEATURE returned {n} bytes, not {}",
            REPORT_LEN + 1
        )));
    }
    Ok(buf[1..].to_vec())
}

impl<T: BootHid> BootChannel for BootGuarded<T> {
    fn send_query(&self) -> io::Result<()> {
        self.send_feature(&query_frame().to_bytes())
    }
    fn send_exit(&self) -> io::Result<()> {
        self.send_feature(&exit_frame().to_bytes())
    }
    fn read_reply(&self) -> io::Result<Vec<u8>> {
        read_reply(self)
    }
}
