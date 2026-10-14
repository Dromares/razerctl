//! Device discovery and raw HID feature-report I/O on Linux.
//!
//! Replaces the macOS-only pieces of the Synapse stack:
//!   * rz-usb-detect / node-rz-hid (IOKit)         -> /sys/class/hidraw + hidraw ioctls
//!   * com.razer.appengine.driver.dext (DriverKit) -> not needed for configuration;
//!     its job (button hook/remap via RazerGenericHIDDriverKit::SetHook /
//!     HandleMouseButton) is input interception, which on Linux belongs to evdev/uinput.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crate::protocol::{Report, Status, REPORT_LEN};

pub const RAZER_VID: u16 = 0x1532;

#[derive(Debug, Clone)]
pub struct HidrawNode {
    pub dev: PathBuf,   // /dev/hidrawN
    pub sysfs: PathBuf, // /sys/class/hidraw/hidrawN/device (the HID device dir)
    pub vid: u16,
    pub pid: u16,
    pub interface: Option<u8>,
    pub name: String,
    pub has_feature_reports: bool,
}

fn parse_hid_id(uevent: &str) -> Option<(u16, u16)> {
    // HID_ID=0003:00001532:0000009F
    let line = uevent.lines().find_map(|l| l.strip_prefix("HID_ID="))?;
    let mut parts = line.split(':');
    parts.next()?;
    let vid = u32::from_str_radix(parts.next()?, 16).ok()? as u16;
    let pid = u32::from_str_radix(parts.next()?, 16).ok()? as u16;
    Some((vid, pid))
}

fn interface_number(sysfs: &Path) -> Option<u8> {
    // .../usb8/8-3/8-3:1.2/0003:1532:009F.0003 -> parent dir has bInterfaceNumber
    let canon = fs::canonicalize(sysfs).ok()?;
    let s = fs::read_to_string(canon.parent()?.join("bInterfaceNumber")).ok()?;
    u8::from_str_radix(s.trim(), 16).ok()
}

/// Walk a HID report descriptor looking for a Main "Feature" item (tag 0xB).
pub fn descriptor_has_feature(desc: &[u8]) -> bool {
    let mut i = 0;
    while i < desc.len() {
        let prefix = desc[i];
        if prefix == 0xFE {
            // long item: 0xFE, size, tag, data...
            let size = *desc.get(i + 1).unwrap_or(&0) as usize;
            i += 3 + size;
            continue;
        }
        let size = match prefix & 0x03 {
            3 => 4,
            n => n as usize,
        };
        if prefix & 0xFC == 0xB0 {
            return true;
        }
        i += 1 + size;
    }
    false
}

/// usbhid builds `HID_NAME` as "<manufacturer> <product>", and Razer firmware
/// already starts the product string with the vendor ("Razer Razer Viper ...").
/// Collapse a leading word run that is immediately repeated ("A B A B C" ->
/// "A B C"). Names that don't repeat are returned unchanged; no vendor prefix
/// is ever added.
fn clean_hid_name(raw: &str) -> String {
    let words: Vec<&str> = raw.split_whitespace().collect();
    for k in 1..=words.len() / 2 {
        let (a, b) = (&words[..k], &words[k..2 * k]);
        if a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y)) {
            return words[k..].join(" ");
        }
    }
    words.join(" ")
}

pub fn enumerate() -> io::Result<Vec<HidrawNode>> {
    let mut out = Vec::new();
    let dir = match fs::read_dir("/sys/class/hidraw") {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let sysfs = entry.path().join("device");
        let Ok(uevent) = fs::read_to_string(sysfs.join("uevent")) else {
            continue;
        };
        let Some((vid, pid)) = parse_hid_id(&uevent) else {
            continue;
        };
        if vid != RAZER_VID {
            continue;
        }
        let hid_name = clean_hid_name(
            uevent
                .lines()
                .find_map(|l| l.strip_prefix("HID_NAME="))
                .unwrap_or(""),
        );
        let desc = fs::read(sysfs.join("report_descriptor")).unwrap_or_default();
        out.push(HidrawNode {
            dev: PathBuf::from("/dev").join(&name),
            interface: interface_number(&sysfs),
            sysfs,
            vid,
            pid,
            name: hid_name,
            has_feature_reports: descriptor_has_feature(&desc),
        });
    }
    out.sort_by(|a, b| a.dev.cmp(&b.dev));
    Ok(out)
}

// _IOC(_IOC_READ|_IOC_WRITE, 'H', nr, len)
const fn hidioc(nr: u64, len: usize) -> u64 {
    (3u64 << 30) | ((len as u64) << 16) | ((b'H' as u64) << 8) | nr
}
const HIDIOCSFEATURE_NR: u64 = 0x06;
const HIDIOCGFEATURE_NR: u64 = 0x07;

pub struct Hidraw {
    file: File,
}

impl Hidraw {
    pub fn open(path: &Path) -> io::Result<Hidraw> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Hidraw { file })
    }

    fn ioctl(&self, nr: u64, buf: &mut [u8]) -> io::Result<usize> {
        let req = hidioc(nr, buf.len());
        // SAFETY: buf is a valid, writable buffer of the length encoded in `req`.
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), req as _, buf.as_mut_ptr()) };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(r as usize)
        }
    }

    /// 91-byte feature report: report ID 0 + 90-byte payload (UsbRzDeviceAction.js).
    pub fn send_feature(&self, payload: &[u8; REPORT_LEN]) -> io::Result<()> {
        let mut buf = [0u8; REPORT_LEN + 1];
        buf[1..].copy_from_slice(payload);
        self.ioctl(HIDIOCSFEATURE_NR, &mut buf).map(|_| ())
    }

    pub fn get_feature(&self) -> io::Result<[u8; REPORT_LEN + 1]> {
        let mut buf = [0u8; REPORT_LEN + 1];
        self.ioctl(HIDIOCGFEATURE_NR, &mut buf)?;
        Ok(buf)
    }

    /// Send a request and poll for its response, retrying while the device
    /// reports Busy (Synapse's mutex+retry loop around sendFeatureReport).
    ///
    /// If another program (e.g. openrazer-daemon polling battery) talks to the
    /// mouse between our write and read, the response belongs to *its* command
    /// (Mismatch). That is a collision, not a device error, so we resend.
    pub fn transact(&self, req: &Report) -> io::Result<Report> {
        let sent = (req.command_class, req.command_id);
        let mut last = None;
        for _attempt in 0..5 {
            self.send_feature(&req.to_bytes())?;
            let mut collided = false;
            for _ in 0..20 {
                std::thread::sleep(std::time::Duration::from_millis(2));
                let raw = self.get_feature()?;
                let resp = Report::parse(&raw).map_err(io::Error::other)?;
                match resp.status_for(sent) {
                    Status::Success => return Ok(resp),
                    Status::Busy | Status::NewCommand => continue,
                    Status::Mismatch => { collided = true; last = Some(resp); break; }
                    Status::Timeout => return Err(io::Error::new(io::ErrorKind::TimedOut,
                        "the receiver can't reach the mouse (status 0x04) — it is probably asleep; move it or click, then retry")),
                    s => return Err(io::Error::other(format!("device returned {s:?}: {resp:?}"))),
                }
            }
            if !collided {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "device stayed busy",
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Err(io::Error::other(format!(
            "response kept colliding with another program's command: {last:?}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_linux_headers() {
        // HIDIOCSFEATURE(91) / HIDIOCGFEATURE(91) from <linux/hidraw.h>
        assert_eq!(hidioc(HIDIOCSFEATURE_NR, 91), 0xC05B4806);
        assert_eq!(hidioc(HIDIOCGFEATURE_NR, 91), 0xC05B4807);
    }

    #[test]
    fn hid_id_parsing() {
        let u = "DRIVER=razermouse\nHID_ID=0003:00001532:0000009F\nHID_NAME=Razer x\n";
        assert_eq!(parse_hid_id(u), Some((0x1532, 0x009f)));
    }

    #[test]
    fn hid_name_collapses_duplicated_vendor_prefix() {
        assert_eq!(
            clean_hid_name("Razer Razer Viper Mini Signature Edition"),
            "Razer Viper Mini Signature Edition"
        );
    }

    #[test]
    fn hid_name_with_single_vendor_prefix_is_unchanged() {
        assert_eq!(clean_hid_name("Razer Viper Mini"), "Razer Viper Mini");
    }

    #[test]
    fn hid_name_without_vendor_prefix_is_left_alone() {
        assert_eq!(clean_hid_name("Viper Mini"), "Viper Mini");
        assert_eq!(clean_hid_name(""), "");
    }

    #[test]
    fn feature_item_detection() {
        // Usage Page (vendor 0xFF00), Usage 1, Collection, Report Count 90, Feature(2), End
        let with = [
            0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x95, 0x5A, 0xB1, 0x02, 0xC0,
        ];
        let without = [0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x81, 0x02, 0xC0];
        assert!(descriptor_has_feature(&with));
        assert!(!descriptor_has_feature(&without));
    }
}
