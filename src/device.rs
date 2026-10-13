//! High-level device operations, with two interchangeable backends:
//!
//! * `Hidraw`   – speaks Protocol25 directly (what Synapse's Electron main process
//!                does through node-rz-hid). Needs rw on /dev/hidrawN (see udev/).
//! * `OpenRazer` – when the openrazer kernel driver is bound, it owns the
//!                control channel and exposes the same commands as sysfs files.
//!                Using it avoids racing the kernel driver on the same interface.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::hidraw::{self, Hidraw, HidrawNode};
use crate::protocol::{byte_to_poll_rate, cmd, poll_rate_to_byte, Report};

pub const VARSTORE: u8 = 0x01;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DpiStages {
    pub active: u8, // 1-based
    pub stages: Vec<(u16, u16)>,
}

#[derive(Debug, Default)]
pub struct Info {
    pub name: String,
    pub firmware: Option<String>,
    pub serial: Option<String>,
    pub dpi: Option<(u16, u16)>,
    pub dpi_stages: Option<DpiStages>,
    pub poll_rate: Option<u32>,
    pub battery_pct: Option<u8>,
    pub charging: Option<bool>,
}

pub trait Backend {
    fn kind(&self) -> &'static str;
    fn path(&self) -> &Path;
    fn name(&self) -> String;
    fn firmware(&self) -> io::Result<String>;
    fn serial(&self) -> io::Result<String>;
    fn dpi(&self) -> io::Result<(u16, u16)>;
    fn set_dpi(&self, x: u16, y: u16) -> io::Result<()>;
    fn dpi_stages(&self) -> io::Result<DpiStages>;
    fn poll_rate(&self) -> io::Result<u32>;
    fn set_poll_rate(&self, hz: u32) -> io::Result<()>;
    /// Raw 0-255 level, as the firmware reports it.
    fn battery_raw(&self) -> io::Result<u8>;
    fn charging(&self) -> io::Result<bool>;

    fn info(&self) -> Info {
        Info {
            name: self.name(),
            firmware: self.firmware().ok(),
            serial: self.serial().ok(),
            dpi: self.dpi().ok(),
            dpi_stages: self.dpi_stages().ok(),
            poll_rate: self.poll_rate().ok(),
            battery_pct: self.battery_raw().ok().map(raw_to_pct),
            charging: self.charging().ok(),
        }
    }
}

pub fn raw_to_pct(raw: u8) -> u8 {
    ((raw as u32 * 100 + 127) / 255) as u8
}

fn check_dpi(x: u16, y: u16) -> io::Result<()> {
    if !(100..=45000).contains(&x) || !(100..=45000).contains(&y) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "DPI must be 100..=45000"));
    }
    Ok(())
}

/// Shared decoder for the 04:86 dpi-stages payload (hidraw) — layout:
/// [varstore, active, count, (idx, x_hi, x_lo, y_hi, y_lo, 0, 0) * count]
pub fn decode_dpi_stages_proto(args: &[u8]) -> DpiStages {
    let active = args[1];
    let count = (args[2] as usize).min(5);
    let stages = (0..count)
        .map(|i| {
            let o = 3 + i * 7;
            (u16::from_be_bytes([args[o + 1], args[o + 2]]), u16::from_be_bytes([args[o + 3], args[o + 4]]))
        })
        .collect();
    DpiStages { active, stages }
}

/// Decoder for OpenRazer's binary dpi_stages sysfs file:
/// [active, (x_hi, x_lo, y_hi, y_lo) * n]
pub fn decode_dpi_stages_sysfs(b: &[u8]) -> io::Result<DpiStages> {
    if b.is_empty() || (b.len() - 1) % 4 != 0 {
        return Err(io::Error::other("malformed dpi_stages"));
    }
    let stages = b[1..]
        .chunks_exact(4)
        .map(|c| (u16::from_be_bytes([c[0], c[1]]), u16::from_be_bytes([c[2], c[3]])))
        .collect();
    Ok(DpiStages { active: b[0], stages })
}

// ---------------------------------------------------------------- hidraw

pub struct HidrawBackend {
    node: HidrawNode,
    dev: Hidraw,
    tid: u8,
}

impl HidrawBackend {
    pub fn open(node: HidrawNode, tid: u8) -> io::Result<Self> {
        let dev = Hidraw::open(&node.dev)?;
        Ok(HidrawBackend { node, dev, tid })
    }
    fn q(&self, c: (u8, u8), size: u8, args: &[u8]) -> io::Result<Report> {
        self.dev.transact(&Report::new(c, size, self.tid).with_args(args))
    }
}

impl Backend for HidrawBackend {
    fn kind(&self) -> &'static str { "hidraw" }
    fn path(&self) -> &Path { &self.node.dev }
    fn name(&self) -> String { self.node.name.clone() }
    fn firmware(&self) -> io::Result<String> {
        let r = self.q(cmd::GET_FIRMWARE_VERSION, 0x02, &[])?;
        Ok(format!("v{}.{}", r.args[0], r.args[1]))
    }
    fn serial(&self) -> io::Result<String> {
        let r = self.q(cmd::GET_SERIAL, 0x16, &[])?;
        let s: String = r.args[..22].iter().take_while(|&&b| b != 0).map(|&b| b as char).collect();
        Ok(s.trim().to_string())
    }
    fn dpi(&self) -> io::Result<(u16, u16)> {
        let r = self.q(cmd::GET_DPI_XY, 0x07, &[VARSTORE])?;
        Ok((u16::from_be_bytes([r.args[1], r.args[2]]), u16::from_be_bytes([r.args[3], r.args[4]])))
    }
    fn set_dpi(&self, x: u16, y: u16) -> io::Result<()> {
        check_dpi(x, y)?;
        let [xh, xl] = x.to_be_bytes();
        let [yh, yl] = y.to_be_bytes();
        self.q(cmd::SET_DPI_XY, 0x07, &[VARSTORE, xh, xl, yh, yl, 0, 0]).map(|_| ())
    }
    fn dpi_stages(&self) -> io::Result<DpiStages> {
        let r = self.q(cmd::GET_DPI_STAGES, 0x26, &[VARSTORE])?;
        Ok(decode_dpi_stages_proto(&r.args))
    }
    fn poll_rate(&self) -> io::Result<u32> {
        let r = self.q(cmd::GET_POLL_RATE2, 0x01, &[])?;
        byte_to_poll_rate(r.args[1]).or(byte_to_poll_rate(r.args[0]))
            .ok_or_else(|| io::Error::other(format!("unknown poll code {:#x}", r.args[1])))
    }
    fn set_poll_rate(&self, hz: u32) -> io::Result<()> {
        let code = poll_rate_to_byte(hz)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "poll rate must be 125/250/500/1000/2000/4000/8000"))?;
        self.q(cmd::SET_POLL_RATE2, 0x02, &[0x00, code]).map(|_| ())
    }
    fn battery_raw(&self) -> io::Result<u8> {
        Ok(self.q(cmd::GET_BATTERY, 0x02, &[])?.args[1])
    }
    fn charging(&self) -> io::Result<bool> {
        Ok(self.q(cmd::GET_CHARGING, 0x02, &[])?.args[1] != 0)
    }
}

// ---------------------------------------------------------------- openrazer sysfs

pub struct OpenRazerBackend {
    dir: PathBuf,
}

impl OpenRazerBackend {
    /// `dir` is the HID device directory that has the razermouse attributes.
    /// Only used if the attributes are actually readable: with the driver loaded
    /// but openrazer-daemon's udev rules gone (e.g. right after uninstalling
    /// OpenRazer, before a reboot), they are root-only and hidraw must be used.
    pub fn probe(dir: &Path) -> Option<Self> {
        let readable = fs::read_to_string(dir.join("firmware_version")).is_ok();
        (dir.join("device_type").exists() && readable).then(|| OpenRazerBackend { dir: dir.to_path_buf() })
    }
    fn read(&self, f: &str) -> io::Result<String> {
        Ok(fs::read_to_string(self.dir.join(f))?.trim().to_string())
    }
    fn write(&self, f: &str, data: &[u8]) -> io::Result<()> {
        fs::write(self.dir.join(f), data)
    }
}

impl Backend for OpenRazerBackend {
    fn kind(&self) -> &'static str { "openrazer-sysfs" }
    fn path(&self) -> &Path { &self.dir }
    fn name(&self) -> String { self.read("device_type").unwrap_or_default() }
    fn firmware(&self) -> io::Result<String> { self.read("firmware_version") }
    fn serial(&self) -> io::Result<String> { self.read("device_serial") }
    fn dpi(&self) -> io::Result<(u16, u16)> {
        let s = self.read("dpi")?;
        let (x, y) = s.split_once(':').ok_or_else(|| io::Error::other(format!("bad dpi '{s}'")))?;
        let p = |v: &str| v.parse::<u16>().map_err(io::Error::other);
        Ok((p(x)?, p(y)?))
    }
    fn set_dpi(&self, x: u16, y: u16) -> io::Result<()> {
        check_dpi(x, y)?;
        let [xh, xl] = x.to_be_bytes();
        let [yh, yl] = y.to_be_bytes();
        self.write("dpi", &[xh, xl, yh, yl])
    }
    fn dpi_stages(&self) -> io::Result<DpiStages> {
        decode_dpi_stages_sysfs(&fs::read(self.dir.join("dpi_stages"))?)
    }
    fn poll_rate(&self) -> io::Result<u32> {
        self.read("poll_rate")?.parse().map_err(io::Error::other)
    }
    fn set_poll_rate(&self, hz: u32) -> io::Result<()> {
        poll_rate_to_byte(hz)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "poll rate must be 125/250/500/1000/2000/4000/8000"))?;
        self.write("poll_rate", hz.to_string().as_bytes())
    }
    fn battery_raw(&self) -> io::Result<u8> {
        self.read("charge_level")?.parse().map_err(io::Error::other)
    }
    fn charging(&self) -> io::Result<bool> {
        Ok(self.read("charge_status")? != "0")
    }
}

// ---------------------------------------------------------------- selection

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prefer { Auto, Hidraw, OpenRazer }

/// One logical device per USB device (Synapse keys by deviceContainerId);
/// returns the best backend for each.
pub fn open_all(prefer: Prefer, tid: u8) -> io::Result<Vec<(HidrawNode, Result<Box<dyn Backend>, io::Error>)>> {
    let nodes = hidraw::enumerate()?;
    let mut out: Vec<(HidrawNode, Result<Box<dyn Backend>, io::Error>)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for n in &nodes {
        let usb_dev = fs::canonicalize(&n.sysfs).ok()
            .and_then(|p| p.parent().and_then(|p| p.parent()).map(Path::to_path_buf));
        if !seen.insert((usb_dev, n.pid)) {
            continue;
        }
        // all interfaces of this USB device, in order
        let siblings: Vec<&HidrawNode> = nodes.iter().filter(|m| m.pid == n.pid
            && fs::canonicalize(&m.sysfs).ok().and_then(|p| p.parent().and_then(|p| p.parent()).map(Path::to_path_buf))
               == fs::canonicalize(&n.sysfs).ok().and_then(|p| p.parent().and_then(|p| p.parent()).map(Path::to_path_buf)))
            .collect();
        let sysfs_backend = siblings.iter().find_map(|m| OpenRazerBackend::probe(&m.sysfs));
        let control = siblings.iter().find(|m| m.has_feature_reports && m.interface == Some(0))
            .or_else(|| siblings.iter().find(|m| m.has_feature_reports))
            .copied().unwrap_or(n);

        let backend: Result<Box<dyn Backend>, io::Error> = match (prefer, sysfs_backend) {
            (Prefer::OpenRazer, Some(b)) | (Prefer::Auto, Some(b)) => Ok(Box::new(b)),
            (Prefer::OpenRazer, None) => Err(io::Error::new(io::ErrorKind::NotFound, "openrazer driver not bound")),
            _ => HidrawBackend::open(control.clone(), tid).map(|b| Box::new(b) as Box<dyn Backend>),
        };
        out.push((control.clone(), backend));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sysfs_stages_from_live_viper_mini_se_dump() {
        // bytes read from /sys/.../0003:1532:009F.0001/dpi_stages on this machine
        let b = [0x04, 0x01, 0x90, 0x01, 0x90, 0x03, 0x20, 0x03, 0x20, 0x06, 0x40, 0x06, 0x40,
                 0x0c, 0x80, 0x0c, 0x80, 0x19, 0x00, 0x19, 0x00];
        let s = decode_dpi_stages_sysfs(&b).unwrap();
        assert_eq!(s.active, 4);
        assert_eq!(s.stages, vec![(400, 400), (800, 800), (1600, 1600), (3200, 3200), (6400, 6400)]);
        assert!(decode_dpi_stages_sysfs(&b[..3]).is_err());
    }

    #[test]
    fn proto_stages_decoder() {
        let mut a = [0u8; 80];
        a[..10].copy_from_slice(&[1, 2, 1, 1, 0x03, 0x20, 0x03, 0x20, 0, 0]);
        let s = decode_dpi_stages_proto(&a);
        assert_eq!(s, DpiStages { active: 2, stages: vec![(800, 800)] });
    }

    #[test]
    fn battery_percent() {
        assert_eq!(raw_to_pct(255), 100);
        assert_eq!(raw_to_pct(0), 0);
        assert_eq!(raw_to_pct(202), 79);
    }
}
