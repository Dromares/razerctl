//! The probe's second transport (DAN-1929, phase 2b of DAN-1878): USB control transfers on endpoint 0 through Linux usbfs
//! (`/dev/bus/usb/BBB/DDD`). The bootloader (`1532:110E`) has no interrupt endpoint, so the kernel makes no hidraw node
//! for it and the hidraw transport cannot reach it. HID feature reports are plain control requests, which usbfs can send.
//!
//! **It can send three requests and nothing else.** The fence, in the same three layers as `hw.rs`:
//!
//! 1. The only ioctls in this file are [`USBDEVFS_CONTROL`] and [`USBDEVFS_CLAIMINTERFACE`], both issued from the single
//!    private [`Usbfs::ioctl`]. No reset, no set-configuration, no driver disconnect or detach, no bulk or interrupt
//!    transfer exists here. `tests/probe.rs` reads this file for exactly that.
//! 2. [`Usbfs`] cannot be built except as the inside of a [`UsbGuarded`] (see [`Usbfs::open`]), and the guard refuses,
//!    before the ioctl, every control request that is not in [`permitted`]: matched on (bmRequestType, bRequest, wValue,
//!    wIndex) and wLength, and for the one that writes, on the payload being the `10:05` exit frame byte for byte.
//!    Interface claims are limited to interface 0.
//! 3. Nothing here names the phase-1 DFU module, its trait or the erase/program/class `0x0A` commands, and no DFU trait is
//!    implemented for the usbfs types. The fence tests read the source for all of it.

use super::exit_frame;
use crate::protocol::{Report, REPORT_LEN};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// The interface every request addresses: the bootloader's interface 0.
pub const INTERFACE: u8 = 0;
/// How much of the HID report descriptor one GET_DESCRIPTOR asks for; a shorter descriptor comes back short.
pub const REPORT_DESCRIPTOR_MAX: u16 = 1024;
/// Per-request timeout of the ioctl.
pub const CONTROL_TIMEOUT_MS: u32 = 1000;

const HID_REPORT_ID: u16 = 0x00;
const HID_FEATURE: u16 = 0x03;
/// wValue of a feature report 0: type in the high byte, report id in the low byte.
const FEATURE_REPORT_0: u16 = (HID_FEATURE << 8) | HID_REPORT_ID;

/// One control request's setup packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl Setup {
    /// Standard GET_DESCRIPTOR, interface recipient, HID report descriptor type: a read.
    pub const fn get_report_descriptor() -> Setup {
        Setup {
            request_type: 0x81,
            request: 0x06,
            value: 0x2200,
            index: INTERFACE as u16,
            length: REPORT_DESCRIPTOR_MAX,
        }
    }
    /// HID SET_REPORT, feature report 0, interface 0: the one request that writes.
    pub const fn set_report_feature() -> Setup {
        Setup {
            request_type: 0x21,
            request: 0x09,
            value: FEATURE_REPORT_0,
            index: INTERFACE as u16,
            length: REPORT_LEN as u16,
        }
    }
    /// HID GET_REPORT, feature report 0, interface 0: reads the status of the SET_REPORT before it.
    pub const fn get_report_feature() -> Setup {
        Setup {
            request_type: 0xA1,
            request: 0x01,
            value: FEATURE_REPORT_0,
            index: INTERFACE as u16,
            length: REPORT_LEN as u16,
        }
    }

    /// The eight bytes on the wire.
    pub fn to_bytes(&self) -> [u8; 8] {
        let (v, i, l) = (
            self.value.to_le_bytes(),
            self.index.to_le_bytes(),
            self.length.to_le_bytes(),
        );
        [
            self.request_type,
            self.request,
            v[0],
            v[1],
            i[0],
            i[1],
            l[0],
            l[1],
        ]
    }

    /// Direction bit of bmRequestType: the device sends the data.
    pub fn is_read(&self) -> bool {
        self.request_type & 0x80 != 0
    }
}

/// The whole allow-list. A request is permitted when (bmRequestType, bRequest, wValue, wIndex) and wLength are one of
/// the three below, and, for the one that writes, `data` is the `10:05` exit frame byte for byte. For the two reads
/// `data` is only the buffer the answer lands in.
pub fn permitted(setup: &Setup, data: &[u8]) -> bool {
    let key = (
        setup.request_type,
        setup.request,
        setup.value,
        setup.index,
        setup.length,
    );
    let k = |s: Setup| (s.request_type, s.request, s.value, s.index, s.length);
    if data.len() != setup.length as usize {
        return false;
    }
    if key == k(Setup::get_report_descriptor()) || key == k(Setup::get_report_feature()) {
        return true;
    }
    key == k(Setup::set_report_feature()) && data == exit_frame().to_bytes()
}

/// A device handle that can carry control requests and claim an interface. The real one is [`Usbfs`]; the tests put a
/// recording device under [`UsbGuarded`] to show what does and does not get through.
pub trait ControlIo {
    fn claim_interface(&self, interface: u8) -> io::Result<()>;
    /// One control transfer. For a read `data` is the buffer to fill; for a write it is the payload. Returns the number
    /// of bytes transferred.
    fn control(&self, setup: &Setup, data: &mut [u8]) -> io::Result<usize>;
}

/// A control channel that can only carry the three requests of [`permitted`].
pub struct UsbGuarded<T: ControlIo>(pub T);

impl<T: ControlIo> ControlIo for UsbGuarded<T> {
    fn claim_interface(&self, interface: u8) -> io::Result<()> {
        if interface != INTERFACE {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refused: the bootloader probe may only claim interface 0",
            ));
        }
        self.0.claim_interface(interface)
    }
    fn control(&self, setup: &Setup, data: &mut [u8]) -> io::Result<usize> {
        if !permitted(setup, data) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refused: the bootloader probe may only read the HID report descriptor, send 10:05 as a feature SET_REPORT and read its GET_REPORT",
            ));
        }
        self.0.control(setup, data)
    }
}

/// Read the HID report descriptor of interface 0 (a standard GET_DESCRIPTOR; the device changes nothing).
pub fn read_report_descriptor<T: ControlIo>(io: &UsbGuarded<T>) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; REPORT_DESCRIPTOR_MAX as usize];
    let n = io.control(&Setup::get_report_descriptor(), &mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

/// Send the exit frame (`10:05`) as a feature SET_REPORT.
pub fn send_exit<T: ControlIo>(io: &UsbGuarded<T>) -> io::Result<()> {
    let mut payload = exit_frame().to_bytes();
    io.control(&Setup::set_report_feature(), &mut payload)
        .map(|_| ())
}

/// Read the 90-byte feature report that answers the last SET_REPORT.
pub fn read_status<T: ControlIo>(io: &UsbGuarded<T>) -> io::Result<Report> {
    let mut buf = [0u8; REPORT_LEN];
    let n = io.control(&Setup::get_report_feature(), &mut buf)?;
    Report::parse(&buf[..n]).map_err(io::Error::other)
}

// _IOC(dir, 'U', nr, size) from <linux/usbdevice_fs.h>
const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | ((b'U' as u64) << 8) | nr
}
const IOC_READ: u64 = 2;
const IOC_RW: u64 = 3;

/// `struct usbdevfs_ctrltransfer`.
#[repr(C)]
struct CtrlTransfer {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
    timeout_ms: u32,
    data: *mut libc::c_void,
}

pub const USBDEVFS_CONTROL: u64 = ioc(IOC_RW, 0, std::mem::size_of::<CtrlTransfer>());
pub const USBDEVFS_CLAIMINTERFACE: u64 = ioc(IOC_READ, 15, std::mem::size_of::<u32>());

/// An open `/dev/bus/usb/BBB/DDD`. Only [`Usbfs::open`] makes one, and it hands it out inside the guard.
pub struct Usbfs {
    file: File,
}

impl Usbfs {
    /// Open the node read/write as the current user. Nothing is sent. The caller is never given a bare `Usbfs`.
    pub fn open(path: &Path) -> io::Result<UsbGuarded<Usbfs>> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(UsbGuarded(Usbfs { file }))
    }

    /// The one place an ioctl is issued.
    fn ioctl(&self, request: u64, arg: *mut libc::c_void) -> io::Result<i32> {
        // SAFETY: `arg` points at a live value of the type `request` expects, for the whole call.
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), request as _, arg) };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(r)
        }
    }
}

impl ControlIo for Usbfs {
    fn claim_interface(&self, interface: u8) -> io::Result<()> {
        let mut n = u32::from(interface);
        self.ioctl(USBDEVFS_CLAIMINTERFACE, (&mut n as *mut u32).cast())
            .map(|_| ())
    }

    fn control(&self, setup: &Setup, data: &mut [u8]) -> io::Result<usize> {
        let mut t = CtrlTransfer {
            request_type: setup.request_type,
            request: setup.request,
            value: setup.value,
            index: setup.index,
            length: data.len() as u16,
            timeout_ms: CONTROL_TIMEOUT_MS,
            data: data.as_mut_ptr().cast(),
        };
        self.ioctl(USBDEVFS_CONTROL, (&mut t as *mut CtrlTransfer).cast())
            .map(|n| n as usize)
    }
}
