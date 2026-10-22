//! Read-only look at the kernel's USB device list (`/sys/bus/usb/devices`). Reading these files asks the kernel for what it
//! cached when the device enumerated; nothing is sent to the device. The root is a parameter, so the tests point it at a
//! fixture tree and never at the real `/sys`.
//!
//! One USB device is a directory such as `8-3` (bus 8, port 3) with `idVendor`, `idProduct`, `bcdDevice`, `busnum`,
//! `devnum`, `product` and the binary `descriptors` file (the device, configuration, interface and endpoint descriptors
//! as the kernel read them). Its interfaces are the sibling directories `8-3:1.0`, `8-3:1.1`, ...; one with a driver
//! bound has a `driver` link.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const DEFAULT_ROOT: &str = "/sys/bus/usb/devices";

/// What is bound to interface 0 of a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Interface0 {
    /// The device has no `:1.0` interface directory (yet).
    Missing,
    /// The interface is there and no driver claimed it (what the board's bootloader looked like: usbhid refused it).
    Unbound,
    Bound(String),
}

/// One USB device as the kernel lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDevice {
    /// Directory name, `8-3`.
    pub name: String,
    pub vid: u16,
    pub pid: u16,
    pub bus: Option<u32>,
    pub dev: Option<u32>,
    pub bcd_device: Option<u16>,
    pub product: Option<String>,
    pub interface0: Interface0,
    /// The hidraw nodes the kernel made for interface 0 (`hidraw0`, ...), found under `<name>:1.0/*/hidraw/` in sysfs: the
    /// link between a USB device and its `/dev/hidrawN`, whatever the HID name says. Empty if there is none.
    pub hidraw: Vec<String>,
    /// The kernel's cached descriptors, or why they could not be read.
    pub descriptors: Result<Vec<u8>, String>,
    pub seen_at: SystemTime,
}

impl UsbDevice {
    /// One line: ids, location, bcdDevice and product.
    pub fn summary(&self) -> String {
        format!(
            "{:04x}:{:04x} {} bus {} device {} bcdDevice {} product {}",
            self.vid,
            self.pid,
            self.name,
            self.bus.map_or("?".into(), |n| n.to_string()),
            self.dev.map_or("?".into(), |n| n.to_string()),
            self.bcd_device.map_or("?".into(), |b| format!("{b:04x}")),
            self.product
                .as_deref()
                .map_or("?".into(), |p| format!("\"{p}\""))
        )
    }
}

fn text(dir: &Path, file: &str) -> Option<String> {
    Some(fs::read_to_string(dir.join(file)).ok()?.trim().to_string())
}

fn hex16(dir: &Path, file: &str) -> Option<u16> {
    u16::from_str_radix(&text(dir, file)?, 16).ok()
}

fn interface0(root: &Path, name: &str) -> Interface0 {
    let dir = root.join(format!("{name}:1.0"));
    if !dir.is_dir() {
        return Interface0::Missing;
    }
    match fs::read_link(dir.join("driver")) {
        Ok(target) => Interface0::Bound(
            target
                .file_name()
                .map_or("?".into(), |n| n.to_string_lossy().into_owned()),
        ),
        Err(_) => Interface0::Unbound,
    }
}

/// The names under `<name>:1.0/<hid device>/hidraw/` (`hidraw0`, ...), sorted: the nodes of interface 0.
fn hidraw_nodes(root: &Path, name: &str) -> Vec<String> {
    let Ok(dir) = fs::read_dir(root.join(format!("{name}:1.0"))) else {
        return Vec::new();
    };
    let mut out: Vec<String> = dir
        .flatten()
        .filter_map(|hid| fs::read_dir(hid.path().join("hidraw")).ok())
        .flat_map(|nodes| nodes.flatten())
        .map(|n| n.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("hidraw"))
        .collect();
    out.sort();
    out
}

/// Every USB device under `root` (interfaces and root hubs left out), sorted by name. A missing root is an empty list.
pub fn scan(root: &Path, seen_at: SystemTime) -> Vec<UsbDevice> {
    let Ok(dir) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<UsbDevice> = dir
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.contains(':') {
                return None;
            }
            let d = e.path();
            Some(UsbDevice {
                vid: hex16(&d, "idVendor")?,
                pid: hex16(&d, "idProduct")?,
                bus: text(&d, "busnum").and_then(|s| s.parse().ok()),
                dev: text(&d, "devnum").and_then(|s| s.parse().ok()),
                bcd_device: hex16(&d, "bcdDevice"),
                product: text(&d, "product"),
                interface0: interface0(root, &name),
                hidraw: hidraw_nodes(root, &name),
                descriptors: fs::read(d.join("descriptors")).map_err(|e| e.to_string()),
                seen_at,
                name,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The first device with this id.
pub fn find(root: &Path, id: (u16, u16), seen_at: SystemTime) -> Option<UsbDevice> {
    scan(root, seen_at)
        .into_iter()
        .find(|d| (d.vid, d.pid) == id)
}

/// Whether a device with this id is listed.
pub fn present(root: &Path, id: (u16, u16)) -> bool {
    find(root, id, SystemTime::UNIX_EPOCH).is_some()
}

/// `/dev/bus/usb/BBB/DDD` under `dev_root`, the usbfs node of a device.
pub fn usbfs_path(dev_root: &Path, bus: u32, dev: u32) -> PathBuf {
    dev_root.join(format!("{bus:03}")).join(format!("{dev:03}"))
}
