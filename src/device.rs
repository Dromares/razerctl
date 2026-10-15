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

use crate::hidraw::{self, transact_on, FeatureIo, Hidraw, HidrawNode};
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
    /// Why `dpi_stages` is `None` (asleep, never completed, ...). `None` when the read worked.
    pub dpi_stages_error: Option<String>,
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
        // The first read that gets no answer at all (TimedOut: asleep / out of range) ends the
        // round: every further read would wait out its own busy window, freezing the GUI for
        // seconds, and would fail the same way.
        let mut gone: Option<String> = None;
        let firmware = read_unless_gone(&mut gone, || self.firmware());
        let serial = read_unless_gone(&mut gone, || self.serial());
        let dpi = read_unless_gone(&mut gone, || self.dpi());
        let mut dpi_stages_error = gone.clone();
        let dpi_stages = read_unless_gone(&mut gone, || {
            self.dpi_stages()
                .inspect_err(|e| dpi_stages_error = Some(e.to_string()))
        });
        let poll_rate = read_unless_gone(&mut gone, || self.poll_rate());
        let battery_pct = read_unless_gone(&mut gone, || self.battery_raw()).map(raw_to_pct);
        let charging = read_unless_gone(&mut gone, || self.charging());
        Info {
            name: self.name(),
            firmware,
            serial,
            dpi,
            dpi_stages,
            dpi_stages_error,
            poll_rate,
            battery_pct,
            charging,
        }
    }
}

/// `f()`'s value, or `None` if it failed or the mouse was already found unreachable. A TimedOut
/// error is remembered in `gone` so [`Backend::info`] can stop asking.
fn read_unless_gone<T>(gone: &mut Option<String>, f: impl FnOnce() -> io::Result<T>) -> Option<T> {
    if gone.is_some() {
        return None;
    }
    match f() {
        Ok(v) => Some(v),
        Err(e) => {
            if e.kind() == io::ErrorKind::TimedOut {
                *gone = Some(e.to_string());
            }
            None
        }
    }
}

pub fn raw_to_pct(raw: u8) -> u8 {
    ((raw as u32 * 100 + 127) / 255) as u8
}

fn check_dpi(x: u16, y: u16) -> io::Result<()> {
    if !(100..=45000).contains(&x) || !(100..=45000).contains(&y) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "DPI must be 100..=45000",
        ));
    }
    Ok(())
}

/// Shared decoder for the 04:86 dpi-stages payload (hidraw) — layout:
/// [varstore, active, count, (idx, x_hi, x_lo, y_hi, y_lo, 0, 0) * count]
///
/// Strict: the firmware can answer Success with the table half built (see
/// [`read_dpi_stages_on`]), so every entry must be present, numbered 1..=count and carry a DPI
/// in the supported range, or the whole table is rejected as `InvalidData`.
pub fn decode_dpi_stages_proto(args: &[u8]) -> io::Result<DpiStages> {
    let bad = |what: String| io::Error::new(io::ErrorKind::InvalidData, what);
    if args.len() < 3 {
        return Err(bad(format!("payload is {} bytes", args.len())));
    }
    let active = args[1];
    let count = args[2] as usize;
    if !(1..=5).contains(&count) {
        return Err(bad(format!("stage count {count}")));
    }
    if args.len() < 3 + count * 7 {
        return Err(bad(format!("{} bytes for {count} stages", args.len())));
    }
    let mut stages = Vec::with_capacity(count);
    for i in 0..count {
        let o = 3 + i * 7;
        let (x, y) = (
            u16::from_be_bytes([args[o + 1], args[o + 2]]),
            u16::from_be_bytes([args[o + 3], args[o + 4]]),
        );
        if args[o] as usize != i + 1 || check_dpi(x, y).is_err() {
            return Err(bad(format!(
                "stage {} is {x}x{y} (index byte {})",
                i + 1,
                args[o]
            )));
        }
        stages.push((x, y));
    }
    Ok(DpiStages { active, stages })
}

/// Decoder for OpenRazer's binary dpi_stages sysfs file:
/// [active, (x_hi, x_lo, y_hi, y_lo) * n]
pub fn decode_dpi_stages_sysfs(b: &[u8]) -> io::Result<DpiStages> {
    if b.is_empty() || (b.len() - 1) % 4 != 0 {
        return Err(io::Error::other("malformed dpi_stages"));
    }
    let stages = b[1..]
        .chunks_exact(4)
        .map(|c| {
            (
                u16::from_be_bytes([c[0], c[1]]),
                u16::from_be_bytes([c[2], c[3]]),
            )
        })
        .collect();
    Ok(DpiStages {
        active: b[0],
        stages,
    })
}

/// Query the stage table, retrying while the answer is not a complete table.
///
/// The firmware flips the status to Success (0x02) as soon as it starts building this answer: on
/// the Viper Mini SE the payload can still hold only the first k of the 5 stage entries (rest
/// zero) or a garbage table, and re-reading that same answer never completes it (DAN-1526). A
/// valid table is the only thing accepted; anything else is re-requested after a short, growing
/// backoff, and if it never completes the caller gets an error, never a partial set.
pub fn read_dpi_stages_on(
    io: &impl FeatureIo,
    tid: u8,
    backoff: std::time::Duration,
) -> io::Result<DpiStages> {
    const ATTEMPTS: u32 = 8;
    let mut last = String::new();
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(backoff * attempt);
        }
        let r = transact_on(
            io,
            &Report::new(cmd::GET_DPI_STAGES, 0x26, tid).with_args(&[VARSTORE]),
        )?;
        let n = (r.data_size as usize).min(r.args.len());
        match decode_dpi_stages_proto(&r.args[..n]) {
            Ok(s) => return Ok(s),
            Err(e) => last = e.to_string(),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("incomplete DPI stage table after {ATTEMPTS} reads ({last})"),
    ))
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
        self.dev
            .transact(&Report::new(c, size, self.tid).with_args(args))
    }
}

impl Backend for HidrawBackend {
    fn kind(&self) -> &'static str {
        "hidraw"
    }
    fn path(&self) -> &Path {
        &self.node.dev
    }
    fn name(&self) -> String {
        self.node.name.clone()
    }
    fn firmware(&self) -> io::Result<String> {
        let r = self.q(cmd::GET_FIRMWARE_VERSION, 0x02, &[])?;
        Ok(format!("v{}.{}", r.args[0], r.args[1]))
    }
    fn serial(&self) -> io::Result<String> {
        let r = self.q(cmd::GET_SERIAL, 0x16, &[])?;
        let s: String = r.args[..22]
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| b as char)
            .collect();
        Ok(s.trim().to_string())
    }
    fn dpi(&self) -> io::Result<(u16, u16)> {
        let r = self.q(cmd::GET_DPI_XY, 0x07, &[VARSTORE])?;
        Ok((
            u16::from_be_bytes([r.args[1], r.args[2]]),
            u16::from_be_bytes([r.args[3], r.args[4]]),
        ))
    }
    fn set_dpi(&self, x: u16, y: u16) -> io::Result<()> {
        check_dpi(x, y)?;
        let [xh, xl] = x.to_be_bytes();
        let [yh, yl] = y.to_be_bytes();
        self.q(cmd::SET_DPI_XY, 0x07, &[VARSTORE, xh, xl, yh, yl, 0, 0])
            .map(|_| ())
    }
    fn dpi_stages(&self) -> io::Result<DpiStages> {
        read_dpi_stages_on(&self.dev, self.tid, std::time::Duration::from_millis(15))
    }
    fn poll_rate(&self) -> io::Result<u32> {
        let r = self.q(cmd::GET_POLL_RATE2, 0x01, &[])?;
        byte_to_poll_rate(r.args[1])
            .or(byte_to_poll_rate(r.args[0]))
            .ok_or_else(|| io::Error::other(format!("unknown poll code {:#x}", r.args[1])))
    }
    fn set_poll_rate(&self, hz: u32) -> io::Result<()> {
        let code = poll_rate_to_byte(hz).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "poll rate must be 125/250/500/1000/2000/4000/8000",
            )
        })?;
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
        (dir.join("device_type").exists() && readable).then(|| OpenRazerBackend {
            dir: dir.to_path_buf(),
        })
    }
    fn read(&self, f: &str) -> io::Result<String> {
        Ok(fs::read_to_string(self.dir.join(f))?.trim().to_string())
    }
    fn write(&self, f: &str, data: &[u8]) -> io::Result<()> {
        fs::write(self.dir.join(f), data)
    }
}

impl Backend for OpenRazerBackend {
    fn kind(&self) -> &'static str {
        "openrazer-sysfs"
    }
    fn path(&self) -> &Path {
        &self.dir
    }
    fn name(&self) -> String {
        self.read("device_type").unwrap_or_default()
    }
    fn firmware(&self) -> io::Result<String> {
        self.read("firmware_version")
    }
    fn serial(&self) -> io::Result<String> {
        self.read("device_serial")
    }
    fn dpi(&self) -> io::Result<(u16, u16)> {
        let s = self.read("dpi")?;
        let (x, y) = s
            .split_once(':')
            .ok_or_else(|| io::Error::other(format!("bad dpi '{s}'")))?;
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
        poll_rate_to_byte(hz).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "poll rate must be 125/250/500/1000/2000/4000/8000",
            )
        })?;
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
pub enum Prefer {
    Auto,
    Hidraw,
    OpenRazer,
}

/// One logical device per USB device (Synapse keys by deviceContainerId);
/// returns the best backend for each.
pub fn open_all(
    prefer: Prefer,
    tid: u8,
) -> io::Result<Vec<(HidrawNode, Result<Box<dyn Backend>, io::Error>)>> {
    let nodes = hidraw::enumerate()?;
    let mut out: Vec<(HidrawNode, Result<Box<dyn Backend>, io::Error>)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for n in &nodes {
        let usb_dev = fs::canonicalize(&n.sysfs)
            .ok()
            .and_then(|p| p.parent().and_then(|p| p.parent()).map(Path::to_path_buf));
        if !seen.insert((usb_dev, n.pid)) {
            continue;
        }
        // all interfaces of this USB device, in order
        let siblings: Vec<&HidrawNode> = nodes
            .iter()
            .filter(|m| {
                m.pid == n.pid
                    && fs::canonicalize(&m.sysfs)
                        .ok()
                        .and_then(|p| p.parent().and_then(|p| p.parent()).map(Path::to_path_buf))
                        == fs::canonicalize(&n.sysfs).ok().and_then(|p| {
                            p.parent().and_then(|p| p.parent()).map(Path::to_path_buf)
                        })
            })
            .collect();
        let sysfs_backend = siblings
            .iter()
            .find_map(|m| OpenRazerBackend::probe(&m.sysfs));
        let control = siblings
            .iter()
            .find(|m| m.has_feature_reports && m.interface == Some(0))
            .or_else(|| siblings.iter().find(|m| m.has_feature_reports))
            .copied()
            .unwrap_or(n);

        let backend: Result<Box<dyn Backend>, io::Error> = match (prefer, sysfs_backend) {
            (Prefer::OpenRazer, Some(b)) | (Prefer::Auto, Some(b)) => Ok(Box::new(b)),
            (Prefer::OpenRazer, None) => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "openrazer driver not bound",
            )),
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
        let b = [
            0x04, 0x01, 0x90, 0x01, 0x90, 0x03, 0x20, 0x03, 0x20, 0x06, 0x40, 0x06, 0x40, 0x0c,
            0x80, 0x0c, 0x80, 0x19, 0x00, 0x19, 0x00,
        ];
        let s = decode_dpi_stages_sysfs(&b).unwrap();
        assert_eq!(s.active, 4);
        assert_eq!(
            s.stages,
            vec![
                (400, 400),
                (800, 800),
                (1600, 1600),
                (3200, 3200),
                (6400, 6400)
            ]
        );
        assert!(decode_dpi_stages_sysfs(&b[..3]).is_err());
    }

    #[test]
    fn proto_stages_decoder() {
        let mut a = [0u8; 80];
        a[..10].copy_from_slice(&[1, 2, 1, 1, 0x03, 0x20, 0x03, 0x20, 0, 0]);
        let s = decode_dpi_stages_proto(&a).unwrap();
        assert_eq!(
            s,
            DpiStages {
                active: 2,
                stages: vec![(800, 800)]
            }
        );
    }

    // ---- DAN-1526: replayed from the raw HID log of the real Viper Mini SE (fw 1.4) ----

    use crate::hidraw::FeatureIo;
    use crate::protocol::{Report, REPORT_LEN};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::time::Duration;

    /// Payload of a complete 04:86 answer (status 0x02), captured live.
    const FULL: [u8; 38] = [
        0x01, 0x04, 0x05, 0x01, 0x01, 0x90, 0x01, 0x90, 0x00, 0x00, 0x02, 0x03, 0x20, 0x03, 0x20,
        0x00, 0x00, 0x03, 0x06, 0x40, 0x06, 0x40, 0x00, 0x00, 0x04, 0x0c, 0x80, 0x0c, 0x80, 0x00,
        0x00, 0x05, 0x19, 0x00, 0x19, 0x00, 0x00, 0x00,
    ];
    const WANT: [(u16, u16); 5] = [
        (400, 400),
        (800, 800),
        (1600, 1600),
        (3200, 3200),
        (6400, 6400),
    ];

    fn answer(status: u8, payload: &[u8]) -> Report {
        let mut r = Report::new(cmd::GET_DPI_STAGES, 0x26, 0x1f).with_args(payload);
        r.status = status;
        r
    }
    fn full() -> Report {
        answer(2, &FULL)
    }
    fn busy() -> Report {
        answer(1, &FULL[..1])
    }
    /// What the firmware really sends sometimes: status Success but only the first `n` stage
    /// entries filled in (the rest still zero, count byte already 5). Re-reading never completes it.
    fn truncated(n: usize) -> Report {
        let mut p = FULL;
        p[3 + n * 7..].fill(0);
        answer(2, &p)
    }

    /// A device that answers each request with the next scripted sequence of reads; the last read
    /// of a sequence repeats if polled further.
    struct Mock {
        rounds: RefCell<VecDeque<Vec<Report>>>,
        cur: RefCell<VecDeque<Report>>,
        last: RefCell<Option<Report>>,
        sends: RefCell<usize>,
    }
    impl Mock {
        fn new(rounds: Vec<Vec<Report>>) -> Mock {
            Mock {
                rounds: RefCell::new(rounds.into()),
                cur: RefCell::default(),
                last: RefCell::default(),
                sends: RefCell::new(0),
            }
        }
        fn sends(&self) -> usize {
            *self.sends.borrow()
        }
    }
    impl FeatureIo for Mock {
        fn send_feature(&self, _p: &[u8; REPORT_LEN]) -> io::Result<()> {
            *self.sends.borrow_mut() += 1;
            *self.cur.borrow_mut() = self
                .rounds
                .borrow_mut()
                .pop_front()
                .unwrap_or_default()
                .into();
            Ok(())
        }
        fn get_feature(&self) -> io::Result<[u8; REPORT_LEN + 1]> {
            let next = self.cur.borrow_mut().pop_front();
            let r = match next {
                Some(r) => {
                    *self.last.borrow_mut() = Some(r.clone());
                    r
                }
                None => self.last.borrow().clone().expect("mock ran out of answers"),
            };
            let mut raw = [0u8; REPORT_LEN + 1];
            raw[1..].copy_from_slice(&r.to_bytes());
            Ok(raw)
        }
    }

    const NO_WAIT: Duration = Duration::ZERO;

    #[test]
    fn complete_answer_loads_in_one_query() {
        let m = Mock::new(vec![vec![busy(), busy(), full()]]);
        let s = read_dpi_stages_on(&m, 0x1f, NO_WAIT).unwrap();
        assert_eq!((s.active, s.stages), (4, WANT.to_vec()));
        assert_eq!(m.sends(), 1);
    }

    #[test]
    fn truncated_success_is_requeried_until_complete() {
        // log: before-A-2 round 0 (first stage only), before-A-4, before-A-5, then a good one
        let m = Mock::new(vec![
            vec![busy(), truncated(1)],
            vec![busy(), busy(), truncated(2)],
            vec![busy(), truncated(4)],
            vec![busy(), full()],
        ]);
        let s = read_dpi_stages_on(&m, 0x1f, NO_WAIT).unwrap();
        assert_eq!(
            s.stages,
            WANT.to_vec(),
            "must never surface a partial stage set"
        );
        assert_eq!(m.sends(), 4);
    }

    #[test]
    fn garbage_answer_is_requeried() {
        // log: before-A-2 round 0: count=5 but the table is [32768, 0, 0, 0, 0]
        let mut g = FULL;
        g[3..].fill(0);
        g[3] = 0x01;
        g[4] = 0x80;
        let m = Mock::new(vec![vec![answer(2, &g)], vec![full()]]);
        let s = read_dpi_stages_on(&m, 0x1f, NO_WAIT).unwrap();
        assert_eq!(s.stages, WANT.to_vec());
    }

    #[test]
    fn stays_truncated_is_an_error_not_a_partial_set() {
        let m = Mock::new(vec![vec![truncated(2)]; 20]);
        let e = read_dpi_stages_on(&m, 0x1f, NO_WAIT).unwrap_err();
        assert!(e.to_string().contains("incomplete"), "{e}");
        assert!(m.sends() > 1, "should have re-queried before giving up");
    }

    #[test]
    fn busy_longer_than_one_window_is_requeried_not_given_up_on() {
        // log: before-A-10 round 0: 04:86 Busy for >20 polls -> "device stayed busy" -> no stages.
        // dpi_latency: an idle mouse can leave a request Busy for 6 s; a re-send gets answered.
        let mut lost = vec![busy(); 35];
        lost.push(full()); // never reached: the request is re-sent after one window
        let m = Mock::new(vec![lost, vec![busy(), busy(), full()]]);
        let s = read_dpi_stages_on(&m, 0x1f, NO_WAIT).unwrap();
        assert_eq!(s.stages, WANT.to_vec());
        assert_eq!(m.sends(), 2);
    }

    #[test]
    fn always_busy_ends_as_asleep_after_bounded_resends() {
        let m = Mock::new(vec![vec![busy()]; 40]);
        let e = read_dpi_stages_on(&m, 0x1f, NO_WAIT).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(e.to_string().contains("asleep"), "{e}");
        assert!((2..=20).contains(&m.sends()), "sends: {}", m.sends());
    }

    #[test]
    fn a_write_is_never_resent_for_busy() {
        let req = Report::new(cmd::SET_DPI_XY, 7, 0x1f);
        let mut busy_write = req.clone();
        busy_write.status = 1;
        let m = Mock::new(vec![vec![busy_write]; 5]);
        assert!(transact_on(&m, &req).is_err());
        assert_eq!(m.sends(), 1);
    }

    /// A backend whose first read gets no answer: the rest of `info()` must not be attempted.
    struct Gone(RefCell<Vec<&'static str>>);
    impl Gone {
        fn asked(&self, what: &'static str) -> io::Error {
            self.0.borrow_mut().push(what);
            io::Error::new(io::ErrorKind::TimedOut, "probably asleep")
        }
    }
    impl Backend for Gone {
        fn kind(&self) -> &'static str {
            "gone"
        }
        fn path(&self) -> &Path {
            Path::new("/dev/null")
        }
        fn name(&self) -> String {
            "x".into()
        }
        fn firmware(&self) -> io::Result<String> {
            Err(self.asked("firmware"))
        }
        fn serial(&self) -> io::Result<String> {
            Err(self.asked("serial"))
        }
        fn dpi(&self) -> io::Result<(u16, u16)> {
            Err(self.asked("dpi"))
        }
        fn set_dpi(&self, _: u16, _: u16) -> io::Result<()> {
            unreachable!()
        }
        fn dpi_stages(&self) -> io::Result<DpiStages> {
            Err(self.asked("dpi_stages"))
        }
        fn poll_rate(&self) -> io::Result<u32> {
            Err(self.asked("poll_rate"))
        }
        fn set_poll_rate(&self, _: u32) -> io::Result<()> {
            unreachable!()
        }
        fn battery_raw(&self) -> io::Result<u8> {
            Err(self.asked("battery"))
        }
        fn charging(&self) -> io::Result<bool> {
            Err(self.asked("charging"))
        }
    }

    #[test]
    fn info_stops_at_the_first_unanswered_read_and_says_why() {
        let b = Gone(RefCell::default());
        let i = b.info();
        assert_eq!(*b.0.borrow(), vec!["firmware"]);
        assert!(i.dpi_stages.is_none());
        assert!(i.dpi_stages_error.unwrap().contains("asleep"));
    }

    #[test]
    fn asleep_is_reported_as_asleep() {
        let m = Mock::new(vec![vec![answer(4, &FULL[..1])]]);
        let e = read_dpi_stages_on(&m, 0x1f, NO_WAIT).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(e.to_string().contains("asleep"), "{e}");
    }
    #[test]
    fn battery_percent() {
        assert_eq!(raw_to_pct(255), 100);
        assert_eq!(raw_to_pct(0), 0);
        assert_eq!(raw_to_pct(202), 79);
    }
}
