//! `razerctl firmware probe-bootloader` (DAN-1908, DAN-1929): golden frames and control requests, simulated runs of every
//! path, the refusals, and the fence that keeps the probe from ever erasing or writing. Every device here is simulated
//! or a fixture tree in a temp directory: nothing in this file opens `/dev/hidraw*`, `/dev/input` or `/dev/bus/usb`, or
//! reads the real `/sys`, and the one test that runs the binary does so without `--yes-enter-bootloader`.

use razerctl::firmware::{dfu, flash};
use razerctl::hidraw::FeatureIo;
use razerctl::probe::cli;
use razerctl::probe::hw::{self, Guarded, RealProbe, Roots};
use razerctl::probe::report::{
    utc, BootloaderWait, Exchange, Get, OpenFailure, Outcome, Pair, ProbeLog, ReturnWait, SendError,
};
use razerctl::probe::sysfs::{self, Interface0, UsbDevice};
use razerctl::probe::usbfs::{self, ControlIo, Setup, UsbGuarded};
use razerctl::probe::{
    self, enter_frame, exit_frame, gate, permitted, query_frame, read_version_frame, Answer, Build,
    Decoded, ProbeIo, Traffic, BOOTLOADER_ID, NORMAL_ID,
};
use razerctl::protocol::{Report, REPORT_LEN};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------------------------------------------------
// Golden frames

fn with_crc(mut b: [u8; REPORT_LEN]) -> [u8; REPORT_LEN] {
    b[88] = b[2..88].iter().fold(0, |a, x| a ^ x);
    b
}

fn golden(size: u8, class: u8, id: u8, args: &[u8]) -> [u8; REPORT_LEN] {
    let mut b = [0u8; REPORT_LEN];
    b[5] = size;
    b[6] = class;
    b[7] = id;
    b[8..8 + args.len()].copy_from_slice(args);
    with_crc(b)
}

#[test]
fn golden_read_version_frame() {
    let b = read_version_frame().to_bytes();
    assert_eq!(b, golden(8, 0x00, 0x87, &[]));
    // status 0, tid 0, packets 0, protocol 0, size 8, 00:87, 80 zero args, xor(8, 0, 0x87) = 0x8f, pad
    assert_eq!(&b[..8], &[0, 0, 0, 0, 0, 8, 0x00, 0x87]);
    assert_eq!(b[88], 0x8f);
    assert_eq!(b[89], 0);
}

#[test]
fn golden_enter_bootloader_frame() {
    let b = enter_frame().to_bytes();
    assert_eq!(b, golden(2, 0x00, 0x04, &[1, 0]));
    assert_eq!(&b[..10], &[0, 0, 0, 0, 0, 2, 0x00, 0x04, 1, 0]);
    // mode 1 is the whole point: size 2, args [1, 0]; xor(2, 0, 4, 1) = 7
    assert_eq!(b[88], 0x07);
}

#[test]
fn golden_exit_bootloader_frame() {
    let b = exit_frame().to_bytes();
    assert_eq!(b, golden(0, 0x10, 0x05, &[]));
    assert_eq!(&b[..8], &[0, 0, 0, 0, 0, 0, 0x10, 0x05]);
    assert_eq!(b[88], 0x15);
}

#[test]
fn golden_query_frame() {
    // DAN-1932 / the ticket, byte for byte: status 0, tid 0, packets 0, protocol 0, size 9, 10:80, 80 zero args, then
    // xor(9, 0x10, 0x80) = 0x99 and a pad byte
    let mut want = [0u8; REPORT_LEN];
    want[..8].copy_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0x10, 0x80]);
    want[88] = 0x99;
    assert_eq!(query_frame().to_bytes(), want);
    assert_eq!(query_frame().to_bytes(), golden(9, 0x10, 0x80, &[]));
    assert_eq!(0x09u8 ^ 0x10 ^ 0x80, 0x99);
    assert!(query_frame().to_bytes()[8..88].iter().all(|&b| b == 0));
    assert_eq!(query_frame().to_bytes()[89], 0);
}

#[test]
fn the_probe_frames_are_the_ones_phase_one_pinned() {
    assert_eq!(read_version_frame(), dfu::read_version_frame());
    assert_eq!(enter_frame(), dfu::enter_frame());
    assert_eq!(exit_frame(), dfu::exit_frame());
}

#[test]
fn the_reply_fields_are_read_from_the_offsets_the_recon_names() {
    // status [0], txn [1], class/id [6..8], PID = [16] | [15] << 8 (vendor buf[17] | buf[16] << 8, with the report id)
    let mut raw = [0u8; REPORT_LEN];
    raw[0] = 2;
    raw[6] = 0x10;
    raw[7] = 0x80;
    raw[15] = 0x00;
    raw[16] = 0x9F;
    let d = Decoded::of(&raw).unwrap();
    assert_eq!((d.status, d.txn, d.class, d.id), (2, 0, 0x10, 0x80));
    assert_eq!(d.pid, 0x009F);
    assert!(d.echoes((0x10, 0x80)) && !d.echoes((0x10, 0x05)));
    // the high byte is [15], not [17]: a byte at [14] or [17] is not the PID
    raw[15] = 0x11;
    raw[16] = 0x0E;
    raw[14] = 0xEE;
    raw[17] = 0xEE;
    assert_eq!(Decoded::of(&raw).unwrap().pid, 0x110E);
    assert!(Decoded::of(&raw[..89]).is_none() && Decoded::of(&[0u8; 91]).is_none());
}

#[test]
fn only_the_two_normal_mode_frames_are_permitted_over_hidraw() {
    for f in [read_version_frame(), enter_frame()] {
        assert!(permitted(&f.to_bytes()), "{f:?}");
    }
    let refused: Vec<(&str, Report)> = vec![
        // 10:80 and 10:05 are the usbfs transport's frames: the bootloader has no hidraw node, so hidraw never carries them
        ("query 10:80", query_frame()),
        ("exit 10:05", exit_frame()),
        ("erase 10:01", dfu::erase_frame(0x1000, 0x2000)),
        ("program 10:02", dfu::program_frame(0x1000, &[1, 2, 3])),
        ("verify 10:83", dfu::verify_frame(0x1000)),
        ("flash enter 0a:00", Report::new(flash::ENTER, 0, 0)),
        ("flash region 0a:80", Report::new(flash::REGION_INFO, 80, 0)),
        (
            "flash record 0a:02",
            flash::record_frame(1, ":00000001FF").unwrap(),
        ),
        ("flash crc 0a:02", flash::crc_frame(1, 0x930c)),
        ("flash finish 0c:00", Report::new(flash::FINISH, 0, 0)),
        (
            "enter mode 2",
            Report::new((0x00, 0x04), 2, 0).with_args(&[2, 0]),
        ),
        (
            "enter with tid 0x1f",
            Report::new((0x00, 0x04), 2, 0x1f).with_args(&[1, 0]),
        ),
        ("read version 00:81", Report::new((0x00, 0x81), 2, 0)),
    ];
    for (what, f) in refused {
        assert!(!permitted(&f.to_bytes()), "{what} must not be permitted");
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The guarded hidraw channel, against a simulated feature-report device

/// Answers each frame with success for the same command; records what reached it.
struct SimFeature {
    sent: RefCell<Vec<[u8; REPORT_LEN]>>,
    pending: RefCell<Option<Report>>,
}

impl SimFeature {
    fn new() -> SimFeature {
        SimFeature {
            sent: RefCell::new(Vec::new()),
            pending: RefCell::new(None),
        }
    }
}

impl FeatureIo for SimFeature {
    fn send_feature(&self, payload: &[u8; REPORT_LEN]) -> io::Result<()> {
        self.sent.borrow_mut().push(*payload);
        let mut r = Report::parse(payload).unwrap();
        r.status = 2;
        if (r.command_class, r.command_id) == (0x00, 0x87) {
            r.args[..3].copy_from_slice(&[1, 4, 0]);
        }
        *self.pending.borrow_mut() = Some(r);
        Ok(())
    }
    fn get_feature(&self) -> io::Result<[u8; REPORT_LEN + 1]> {
        let r = self.pending.borrow().clone().expect("nothing was sent");
        let mut out = [0u8; REPORT_LEN + 1];
        out[1..].copy_from_slice(&r.to_bytes());
        Ok(out)
    }
}

/// A channel whose writes fail with a given errno after recording the frame (the bootloader cutting the transfer short).
struct FailingFeature(i32, RefCell<usize>);

impl FeatureIo for FailingFeature {
    fn send_feature(&self, _: &[u8; REPORT_LEN]) -> io::Result<()> {
        *self.1.borrow_mut() += 1;
        Err(io::Error::from_raw_os_error(self.0))
    }
    fn get_feature(&self) -> io::Result<[u8; REPORT_LEN + 1]> {
        Err(io::Error::from_raw_os_error(self.0))
    }
}

#[test]
fn the_hidraw_code_path_emits_exactly_the_golden_frames() {
    let g = Guarded(SimFeature::new());
    let v = hw::read_version_via(&g).unwrap();
    assert_eq!(&v.args[..3], &[1, 4, 0]);
    assert!(hw::enter_via(&g).unwrap().is_ok());
    let sent = g.0.sent.borrow();
    assert_eq!(
        *sent,
        vec![golden(8, 0x00, 0x87, &[]), golden(2, 0x00, 0x04, &[1, 0])]
    );
}

#[test]
fn an_errno_on_the_enter_send_reaches_the_caller_as_an_errno() {
    // the board's first run: the enter frame was cut off by the disconnect and the write returned EPROTO (71)
    let g = Guarded(FailingFeature(71, RefCell::new(0)));
    let e = hw::enter_via(&g).unwrap_err();
    assert_eq!(e.errno, Some(71));
    assert!(e.to_string().contains("errno 71"), "{e}");
    assert_eq!(*g.0 .1.borrow(), 1);
}

#[test]
fn the_guard_stops_every_other_frame_before_the_device() {
    let g = Guarded(SimFeature::new());
    let forbidden = [
        query_frame(),
        exit_frame(),
        dfu::erase_frame(0x1000, 0x2000),
        dfu::program_frame(0x1000, &[0xAA; 64]),
        flash::record_frame(1, ":00000001FF").unwrap(),
        flash::crc_frame(1, 0x930c),
        Report::new(flash::ENTER, 0, 0),
    ];
    for f in forbidden {
        let e = g.send_feature(&f.to_bytes()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{f:?}");
        assert!(
            hw::send_and_listen(&g, &f).is_err(),
            "send_and_listen let {f:?} through"
        );
    }
    assert!(g.0.sent.borrow().is_empty(), "something reached the device");
}

// ---------------------------------------------------------------------------------------------------------------------
// The usbfs transport: golden control requests, and the guard, against a recording device

#[test]
fn golden_setup_packets() {
    // GET_DESCRIPTOR: device-to-host, standard, interface; request 6; wValue 0x2200 (HID report descriptor, index 0);
    // wIndex 0 (interface 0); wLength 1024
    assert_eq!(
        Setup::get_report_descriptor().to_bytes(),
        [0x81, 0x06, 0x00, 0x22, 0x00, 0x00, 0x00, 0x04]
    );
    // SET_REPORT: host-to-device, class, interface; request 9; wValue 0x0300 (feature report, id 0); wIndex 0; wLength 90
    assert_eq!(
        Setup::set_report_feature().to_bytes(),
        [0x21, 0x09, 0x00, 0x03, 0x00, 0x00, 0x5a, 0x00]
    );
    // GET_REPORT: device-to-host, class, interface; request 1; same wValue; wLength 90
    assert_eq!(
        Setup::get_report_feature().to_bytes(),
        [0xA1, 0x01, 0x00, 0x03, 0x00, 0x00, 0x5a, 0x00]
    );
    assert!(Setup::get_report_descriptor().is_read());
    assert!(!Setup::set_report_feature().is_read());
    assert!(Setup::get_report_feature().is_read());
}

#[test]
fn golden_ioctl_numbers() {
    // <linux/usbdevice_fs.h>: _IOWR('U', 0, struct usbdevfs_ctrltransfer) and _IOR('U', 15, unsigned int)
    #[cfg(target_pointer_width = "64")]
    assert_eq!(usbfs::USBDEVFS_CONTROL, 0xC018_5500);
    assert_eq!(usbfs::USBDEVFS_CLAIMINTERFACE, 0x8004_550F);
}

/// A `10:80`-shaped or `10:05`-shaped reply as the device returns it: 90 bytes, status as given, the PID at `[15..17]`.
fn reply_bytes(cmd: (u8, u8), status: u8, pid: u16) -> Vec<u8> {
    let mut r = Report::new(cmd, 0, 0);
    r.status = status;
    r.args[7] = (pid >> 8) as u8;
    r.args[8] = pid as u8;
    r.to_bytes().to_vec()
}

/// A usbfs device that records every request that reaches it: the setup packet and, for a write, the payload, and when it
/// arrived. GET_REPORTs are answered from `gets` in order; with none scripted the device answers the last SET_REPORT with
/// status 2 (and PID 009F to a `10:80`).
struct SimUsb {
    requests: RefCell<Vec<(Setup, Vec<u8>)>>,
    arrived: RefCell<Vec<std::time::Instant>>,
    claimed: RefCell<Vec<u8>>,
    descriptor: Vec<u8>,
    set_errno: Option<i32>,
    /// `Err(errno)` = that GET_REPORT fails with it.
    gets: RefCell<VecDeque<Result<Vec<u8>, i32>>>,
    last_set: RefCell<Option<Vec<u8>>>,
}

impl SimUsb {
    fn new() -> SimUsb {
        SimUsb {
            requests: RefCell::new(Vec::new()),
            arrived: RefCell::new(Vec::new()),
            claimed: RefCell::new(Vec::new()),
            descriptor: DESC.to_vec(),
            set_errno: None,
            gets: RefCell::new(VecDeque::new()),
            last_set: RefCell::new(None),
        }
    }
    fn scripted(gets: Vec<Result<Vec<u8>, i32>>) -> SimUsb {
        let s = SimUsb::new();
        *s.gets.borrow_mut() = gets.into();
        s
    }
    /// The payloads of the SET_REPORTs that reached the device, in order.
    fn sets(&self) -> Vec<Vec<u8>> {
        self.requests
            .borrow()
            .iter()
            .filter(|(s, _)| *s == Setup::set_report_feature())
            .map(|(_, p)| p.clone())
            .collect()
    }
    fn gets_seen(&self) -> usize {
        self.requests
            .borrow()
            .iter()
            .filter(|(s, _)| *s == Setup::get_report_feature())
            .count()
    }
}

impl ControlIo for SimUsb {
    fn claim_interface(&self, interface: u8) -> io::Result<()> {
        self.claimed.borrow_mut().push(interface);
        Ok(())
    }
    fn control(&self, setup: &Setup, data: &mut [u8]) -> io::Result<usize> {
        let payload = if setup.is_read() {
            Vec::new()
        } else {
            data.to_vec()
        };
        self.requests.borrow_mut().push((*setup, payload));
        self.arrived.borrow_mut().push(std::time::Instant::now());
        if *setup == Setup::get_report_descriptor() {
            data[..self.descriptor.len()].copy_from_slice(&self.descriptor);
            Ok(self.descriptor.len())
        } else if *setup == Setup::set_report_feature() {
            *self.last_set.borrow_mut() = Some(data.to_vec());
            match self.set_errno {
                Some(e) => Err(io::Error::from_raw_os_error(e)),
                None => Ok(data.len()),
            }
        } else {
            let scripted = self.gets.borrow_mut().pop_front();
            let bytes = match scripted {
                Some(Err(e)) => return Err(io::Error::from_raw_os_error(e)),
                Some(Ok(b)) => b,
                None => {
                    let sent = self.last_set.borrow().clone().expect("GET before any SET");
                    let cmd = (sent[6], sent[7]);
                    reply_bytes(cmd, 2, if cmd == (0x10, 0x80) { 0x009F } else { 0 })
                }
            };
            data[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        }
    }
}

const ETIMEDOUT: i32 = 110;

#[test]
fn the_usbfs_path_sends_the_query_then_the_exit_on_one_handle_and_nothing_else() {
    let g = UsbGuarded(SimUsb::new());
    g.claim_interface(usbfs::INTERFACE).unwrap();
    assert_eq!(usbfs::read_report_descriptor(&g).unwrap(), DESC.to_vec());
    let q = hw::query_via(&g);
    assert_eq!(gate(&q), Ok(0x009F));
    let x = hw::exit_via(&g);
    assert_eq!(x.set_error(), None);
    let status = Decoded::of(x.last_get().unwrap().reply.as_ref().unwrap()).unwrap();
    assert_eq!((status.class, status.id, status.status), (0x10, 0x05, 2));
    assert_eq!(*g.0.claimed.borrow(), [0]);
    assert_eq!(
        *g.0.requests.borrow(),
        vec![
            (Setup::get_report_descriptor(), Vec::new()),
            (
                Setup::set_report_feature(),
                golden(9, 0x10, 0x80, &[]).to_vec()
            ),
            (Setup::get_report_feature(), Vec::new()),
            (
                Setup::set_report_feature(),
                golden(0, 0x10, 0x05, &[]).to_vec()
            ),
            (Setup::get_report_feature(), Vec::new()),
        ]
    );
}

#[test]
fn the_query_set_and_get_are_the_golden_setup_packets_and_payload() {
    let g = UsbGuarded(SimUsb::new());
    let _ = hw::query_via(&g);
    let reqs = g.0.requests.borrow();
    assert_eq!(reqs.len(), 2);
    assert_eq!(
        reqs[0].0.to_bytes(),
        [0x21, 0x09, 0x00, 0x03, 0x00, 0x00, 0x5a, 0x00]
    );
    assert_eq!(
        reqs[1].0.to_bytes(),
        [0xA1, 0x01, 0x00, 0x03, 0x00, 0x00, 0x5a, 0x00]
    );
    let mut want = vec![0u8; REPORT_LEN];
    want[5] = 0x09;
    want[6] = 0x10;
    want[7] = 0x80;
    want[88] = 0x99;
    assert_eq!(reqs[0].1, want);
}

#[test]
fn a_failed_set_report_is_reported_with_its_errno_and_no_status_read() {
    let mut sim = SimUsb::new();
    sim.set_errno = Some(71);
    let g = UsbGuarded(sim);
    let x = hw::exit_via(&g);
    assert_eq!(x.set_error().unwrap().errno, Some(71));
    assert!(
        x.last_get().is_none(),
        "no GET_REPORT after a failed SET_REPORT"
    );
    assert_eq!(g.0.requests.borrow().len(), 1);
    // and the same for the query: the device may have left the bus, so nothing is re-sent
    let mut sim = SimUsb::new();
    sim.set_errno = Some(71);
    let g = UsbGuarded(sim);
    let q = hw::query_via(&g);
    assert_eq!(q.set_error().unwrap().errno, Some(71));
    assert_eq!(g.0.requests.borrow().len(), 1);
    assert!(gate(&q).unwrap_err().contains("SET_REPORT failed"));
}

// --- the query's polling and re-sends (the vendor sender, DAN-1932) -----------------------------------------------------

fn ok(cmd: (u8, u8), status: u8, pid: u16) -> Result<Vec<u8>, i32> {
    Ok(reply_bytes(cmd, status, pid))
}
const Q: (u8, u8) = (0x10, 0x80);

#[test]
fn status_zero_and_one_are_polled_not_resent() {
    let g = UsbGuarded(SimUsb::scripted(vec![
        ok(Q, 0, 0),
        ok(Q, 1, 0),
        ok(Q, 2, 0x009F),
    ]));
    let q = hw::query_via(&g);
    assert_eq!(g.0.sets().len(), 1, "one SET");
    assert_eq!(g.0.gets_seen(), 3);
    assert_eq!((q.sets(), q.pairs[0].gets.len()), (1, 3));
    assert_eq!(gate(&q), Ok(0x009F));
}

#[test]
fn a_failed_get_and_a_reply_to_another_frame_are_polled_again() {
    let g = UsbGuarded(SimUsb::scripted(vec![
        Err(ETIMEDOUT),
        ok((0x10, 0x05), 2, 0x009F), // status 2, but for another frame
        ok(Q, 2, 0x009E),
    ]));
    let q = hw::query_via(&g);
    assert_eq!((g.0.sets().len(), g.0.gets_seen()), (1, 3));
    assert_eq!(
        gate(&q),
        Ok(0x009E),
        "009E is the second PID the vendor takes"
    );
}

#[test]
fn a_txn_mismatch_is_not_an_answer() {
    let mut bad = reply_bytes(Q, 2, 0x009F);
    bad[1] = 5;
    let g = UsbGuarded(SimUsb::scripted(vec![Ok(bad), ok(Q, 2, 0x009F)]));
    let q = hw::query_via(&g);
    assert_eq!((g.0.sets().len(), g.0.gets_seen()), (1, 2));
    assert_eq!(gate(&q), Ok(0x009F));
}

#[test]
fn a_short_get_is_a_failed_get() {
    let g = UsbGuarded(SimUsb::scripted(vec![
        Ok(reply_bytes(Q, 2, 0x009F)[..89].to_vec()),
        ok(Q, 2, 0x009F),
    ]));
    let q = hw::query_via(&g);
    assert_eq!(g.0.gets_seen(), 2);
    assert!(q.pairs[0].gets[0].reply.is_err());
    assert_eq!(gate(&q), Ok(0x009F));
}

#[test]
fn any_other_status_resends_the_query_up_to_three_pairs_in_all() {
    // 3, 4 and 5 re-send at once
    let g = UsbGuarded(SimUsb::scripted(vec![
        ok(Q, 3, 0),
        ok(Q, 4, 0),
        ok(Q, 2, 0x009F),
    ]));
    let q = hw::query_via(&g);
    assert_eq!((g.0.sets().len(), g.0.gets_seen()), (3, 3));
    assert!(g.0.sets().iter().all(|s| *s == golden(9, 0x10, 0x80, &[])));
    assert_eq!(gate(&q), Ok(0x009F));
    // and never a fourth pair
    let g = UsbGuarded(SimUsb::scripted(vec![
        ok(Q, 5, 0),
        ok(Q, 5, 0),
        ok(Q, 5, 0),
        ok(Q, 2, 0x009F),
    ]));
    let q = hw::query_via(&g);
    assert_eq!((g.0.sets().len(), g.0.gets_seen()), (3, 3));
    assert!(gate(&q).unwrap_err().contains("status 0x05"), "{q:?}");
}

#[test]
fn five_gets_without_an_answer_resend_the_query_and_then_stop() {
    // the GET times out every time (the board's second run): five reads per SET, three SETs
    let g = UsbGuarded(SimUsb::scripted(vec![Err(ETIMEDOUT); 20]));
    let q = hw::query_via(&g);
    assert_eq!(g.0.sets().len(), 3);
    assert_eq!(g.0.gets_seen(), 15);
    assert_eq!(
        q.pairs.iter().map(|p| p.gets.len()).collect::<Vec<_>>(),
        [5, 5, 5]
    );
    let why = gate(&q).unwrap_err();
    assert!(why.contains("errno 110"), "{why}");
}

// --- the exit's re-sends -----------------------------------------------------------------------------------------------

#[test]
fn the_exit_is_resent_after_a_failed_get_up_to_three_times() {
    let exit = golden(0, 0x10, 0x05, &[]).to_vec();
    // the GET times out every time: three SET/GET pairs, one GET each, the same SET each time
    let g = UsbGuarded(SimUsb::scripted(vec![Err(ETIMEDOUT); 5]));
    let x = hw::exit_via(&g);
    assert_eq!(g.0.sets(), vec![exit.clone(); 3]);
    assert_eq!(g.0.gets_seen(), 3);
    assert_eq!(x.sets(), 3);
    assert_eq!(x.set_error(), None);
    // accepted on the second try: two pairs
    let g = UsbGuarded(SimUsb::scripted(vec![
        Err(ETIMEDOUT),
        ok((0x10, 0x05), 2, 0),
    ]));
    let x = hw::exit_via(&g);
    assert_eq!((g.0.sets().len(), g.0.gets_seen(), x.sets()), (2, 2, 2));
    // accepted at once: one pair
    let g = UsbGuarded(SimUsb::new());
    let x = hw::exit_via(&g);
    assert_eq!((g.0.sets().len(), g.0.gets_seen(), x.sets()), (1, 1, 1));
}

#[test]
fn the_exit_is_resent_after_any_status_but_two() {
    for status in [0u8, 1, 3, 4, 5] {
        let g = UsbGuarded(SimUsb::scripted(vec![ok((0x10, 0x05), status, 0); 5]));
        let x = hw::exit_via(&g);
        assert_eq!(g.0.sets().len(), 3, "status {status}");
        assert_eq!(g.0.gets_seen(), 3, "status {status}: one GET per SET");
        assert_eq!(x.pairs.len(), 3);
    }
}

#[test]
fn a_set_that_fails_on_a_resend_ends_the_exit() {
    // the first GET fails; the second SET is cut off (the device left the bus): no third pair, the errno is kept
    struct CutOff(SimUsb);
    impl ControlIo for CutOff {
        fn claim_interface(&self, i: u8) -> io::Result<()> {
            self.0.claim_interface(i)
        }
        fn control(&self, s: &Setup, d: &mut [u8]) -> io::Result<usize> {
            if *s == Setup::set_report_feature() && self.0.sets().len() == 1 {
                self.0.requests.borrow_mut().push((*s, d.to_vec()));
                return Err(io::Error::from_raw_os_error(71));
            }
            self.0.control(s, d)
        }
    }
    let g = UsbGuarded(CutOff(SimUsb::scripted(vec![Err(ETIMEDOUT)])));
    let x = hw::exit_via(&g);
    assert_eq!(x.pairs.len(), 2);
    assert_eq!(x.set_error().unwrap().errno, Some(71));
    assert!(x.pairs[1].gets.is_empty());
}

#[test]
fn two_milliseconds_before_every_set_and_every_get_and_no_other_pause() {
    let g = UsbGuarded(SimUsb::scripted(vec![
        ok(Q, 1, 0),
        ok(Q, 2, 0x009F),
        Err(ETIMEDOUT),
        ok((0x10, 0x05), 2, 0),
    ]));
    let _ = hw::query_via(&g);
    let query_done = g.0.arrived.borrow().len();
    let _ = hw::exit_via(&g);
    let t = g.0.arrived.borrow().clone();
    assert_eq!(t.len(), 3 + 4);
    // [SET, GET, GET] [SET, GET, SET, GET]: each request comes at least 2 ms after the one before it
    for w in t.windows(2) {
        assert!(w[1] - w[0] >= Duration::from_millis(2), "{:?}", w[1] - w[0]);
    }
    // and the exit follows the query's last read without a sleep of its own: the vendor gap is the 2 ms pre-delay
    let gap = t[query_done] - t[query_done - 1];
    assert!(gap < Duration::from_millis(500), "gap {gap:?}");
}

fn set_report(value: u16, index: u16, length: u16) -> Setup {
    Setup {
        value,
        index,
        length,
        ..Setup::set_report_feature()
    }
}

#[test]
fn the_usbfs_guard_stops_every_other_request_before_the_device() {
    let g = UsbGuarded(SimUsb::new());
    let exit = golden(0, 0x10, 0x05, &[]).to_vec();
    let mut cases: Vec<(&str, Setup, Vec<u8>)> = vec![
        // a SET_REPORT with any payload but 10:05
        (
            "erase 10:01",
            Setup::set_report_feature(),
            dfu::erase_frame(0x1000, 0x2000).to_bytes().to_vec(),
        ),
        (
            "program 10:02",
            Setup::set_report_feature(),
            dfu::program_frame(0x1000, &[0xAA; 64]).to_bytes().to_vec(),
        ),
        (
            "flash record 0a:02",
            Setup::set_report_feature(),
            flash::record_frame(1, ":00000001FF")
                .unwrap()
                .to_bytes()
                .to_vec(),
        ),
        (
            "flash enter 0a:00",
            Setup::set_report_feature(),
            Report::new(flash::ENTER, 0, 0).to_bytes().to_vec(),
        ),
        (
            "enter 00:04",
            Setup::set_report_feature(),
            enter_frame().to_bytes().to_vec(),
        ),
        (
            "read version 00:87",
            Setup::set_report_feature(),
            read_version_frame().to_bytes().to_vec(),
        ),
        ("zeros", Setup::set_report_feature(), vec![0; REPORT_LEN]),
        ("exit with a flipped byte", Setup::set_report_feature(), {
            let mut b = exit.clone();
            b[8] = 1;
            b
        }),
        (
            "exit one byte short",
            set_report(0x0300, 0, 89),
            exit[..89].to_vec(),
        ),
        // the right payload under the wrong setup
        (
            "exit as report id 1",
            set_report(0x0301, 0, 90),
            exit.clone(),
        ),
        (
            "exit as an output report",
            set_report(0x0200, 0, 90),
            exit.clone(),
        ),
        (
            "exit on interface 1",
            set_report(0x0300, 1, 90),
            exit.clone(),
        ),
        (
            "exit with a vendor request type",
            Setup {
                request_type: 0x40,
                ..Setup::set_report_feature()
            },
            exit.clone(),
        ),
        (
            "exit as SET_IDLE",
            Setup {
                request: 0x0A,
                ..Setup::set_report_feature()
            },
            exit.clone(),
        ),
        (
            "exit as SET_PROTOCOL",
            Setup {
                request: 0x0B,
                ..Setup::set_report_feature()
            },
            exit.clone(),
        ),
        // standard and class requests that change the device
        (
            "SET_CONFIGURATION",
            Setup {
                request_type: 0x00,
                request: 0x09,
                value: 1,
                index: 0,
                length: 0,
            },
            vec![],
        ),
        (
            "SET_INTERFACE",
            Setup {
                request_type: 0x01,
                request: 0x0B,
                value: 0,
                index: 0,
                length: 0,
            },
            vec![],
        ),
        (
            "CLEAR_FEATURE(ENDPOINT_HALT)",
            Setup {
                request_type: 0x02,
                request: 0x01,
                value: 0,
                index: 0x81,
                length: 0,
            },
            vec![],
        ),
        (
            "SET_ADDRESS",
            Setup {
                request_type: 0x00,
                request: 0x05,
                value: 9,
                index: 0,
                length: 0,
            },
            vec![],
        ),
        // reads that are not the three
        (
            "device descriptor",
            Setup {
                request_type: 0x80,
                request: 0x06,
                value: 0x0100,
                index: 0,
                length: 18,
            },
            vec![0; 18],
        ),
        (
            "report descriptor of interface 1",
            Setup {
                index: 1,
                ..Setup::get_report_descriptor()
            },
            vec![0; 1024],
        ),
        (
            "report descriptor, wrong length",
            Setup {
                length: 64,
                ..Setup::get_report_descriptor()
            },
            vec![0; 64],
        ),
        (
            "report descriptor into a short buffer",
            Setup::get_report_descriptor(),
            vec![0; 64],
        ),
        (
            "GET_REPORT as an input report",
            Setup {
                value: 0x0100,
                ..Setup::get_report_feature()
            },
            vec![0; 90],
        ),
        (
            "GET_REPORT of interface 1",
            Setup {
                index: 1,
                ..Setup::get_report_feature()
            },
            vec![0; 90],
        ),
        (
            "GET_STATUS",
            Setup {
                request_type: 0x80,
                request: 0x00,
                value: 0,
                index: 0,
                length: 2,
            },
            vec![0; 2],
        ),
    ];
    cases.push((
        "the right setup with no payload",
        Setup::set_report_feature(),
        vec![],
    ));
    for (what, setup, mut data) in cases {
        let e = g.control(&setup, &mut data).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{what}");
    }
    for interface in [1u8, 2, 255] {
        assert!(g.claim_interface(interface).is_err(), "claimed {interface}");
    }
    assert!(
        g.0.requests.borrow().is_empty() && g.0.claimed.borrow().is_empty(),
        "something reached the device"
    );
    // and the ones that are allowed do get through: the two reads, and the SET_REPORT with the query or the exit
    assert!(usbfs::permitted(
        &Setup::get_report_descriptor(),
        &[0; 1024]
    ));
    assert!(usbfs::permitted(&Setup::set_report_feature(), &exit));
    assert!(usbfs::permitted(
        &Setup::set_report_feature(),
        &golden(9, 0x10, 0x80, &[])
    ));
    assert!(usbfs::permitted(&Setup::get_report_feature(), &[0; 90]));
}

#[test]
fn the_usbfs_guard_refuses_every_frame_but_the_query_and_the_exit() {
    // every (class, id) with the sizes the vendor frames use, and the two permitted ids with a changed size or argument
    let allowed = [(0x10u8, 0x80u8, 9u8), (0x10, 0x05, 0)];
    let mut tried = 0;
    for class in 0..=255u8 {
        for id in 0..=255u8 {
            for size in [0u8, 2, 8, 9, 80] {
                let f = Report::new((class, id), size, 0).to_bytes();
                let want = allowed.contains(&(class, id, size));
                assert_eq!(
                    usbfs::permitted(&Setup::set_report_feature(), &f),
                    want,
                    "{class:02x}:{id:02x} size {size}"
                );
                tried += 1;
            }
        }
    }
    assert_eq!(tried, 256 * 256 * 5);
    for (what, f) in [
        ("query with an argument", {
            let mut b = golden(9, 0x10, 0x80, &[]);
            b[8] = 1;
            b
        }),
        ("query with a bad checksum", {
            let mut b = golden(9, 0x10, 0x80, &[]);
            b[88] = 0;
            b
        }),
        ("query with a transaction id", {
            let mut b = golden(9, 0x10, 0x80, &[]);
            b[1] = 0x1f;
            b
        }),
        ("erase 10:01", dfu::erase_frame(0x1000, 0x2000).to_bytes()),
        ("program 10:02", dfu::program_frame(0x1000, &[1]).to_bytes()),
        ("verify 10:83", dfu::verify_frame(0x1000).to_bytes()),
        ("flash 0a:00", Report::new(flash::ENTER, 0, 0).to_bytes()),
    ] {
        assert!(
            !usbfs::permitted(&Setup::set_report_feature(), &f),
            "{what}"
        );
    }
    // the query is a SET_REPORT only: not under a read setup, not as another report
    let q = golden(9, 0x10, 0x80, &[]);
    for (what, s) in [
        ("as report id 1", set_report(0x0301, 0, 90)),
        ("on interface 1", set_report(0x0300, 1, 90)),
        ("short", set_report(0x0300, 0, 89)),
    ] {
        let data = if s.length as usize == q.len() {
            q.to_vec()
        } else {
            vec![0; s.length as usize]
        };
        assert!(!usbfs::permitted(&s, &data), "query {what}");
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Simulated runs

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_792_000_000 + secs)
}

const DESC: [u8; 12] = [
    0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x95, 0x5A, 0xB1, 0x02, 0xC0,
];

/// The kernel's cached descriptors of the bootloader, as far as the tests care: a device descriptor.
const KERNEL_DESC: [u8; 18] = [
    0x12, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x40, 0x32, 0x15, 0x0E, 0x11, 0x9F, 0x00, 0x01, 0x02,
    0x00, 0x01,
];

fn bootloader_device(interface0: Interface0) -> UsbDevice {
    UsbDevice {
        name: "8-3".into(),
        vid: 0x1532,
        pid: 0x110E,
        bus: Some(8),
        dev: Some(5),
        bcd_device: Some(0x009f),
        product: Some("RAZER BOOTLOADER".into()),
        interface0,
        descriptors: Ok(KERNEL_DESC.to_vec()),
        seen_at: at(3),
    }
}

fn errno(n: i32) -> SendError {
    SendError::from(io::Error::from_raw_os_error(n))
}

/// A scripted device. Each call is appended to `calls`; the clock advances one second per call.
struct Sim {
    calls: Vec<&'static str>,
    clock: u64,
    version: Result<Report, String>,
    enter: Result<Answer, SendError>,
    dongle_left: Option<Duration>,
    bootloader: Option<UsbDevice>,
    open: Result<String, OpenFailure>,
    descriptor: Result<Vec<u8>, SendError>,
    query: Exchange,
    exit: Exchange,
    bootloader_left: Option<Duration>,
    /// Seconds after the end of the step-4 call at which 1532:009F is listed again.
    back: Option<u64>,
}

/// One SET_REPORT and one GET_REPORT that returned a 90-byte reply.
fn exchange_ok(cmd: (u8, u8), status: u8, pid: u16) -> Exchange {
    Exchange {
        pairs: vec![Pair {
            set_at: at(0),
            set: Ok(()),
            gets: vec![Get {
                at: at(0),
                reply: Ok(reply_bytes(cmd, status, pid)),
            }],
        }],
    }
}

/// A SET_REPORT that went out and GET_REPORTs that all failed with `errno`.
fn exchange_get_failed(gets: usize, n: i32) -> Exchange {
    Exchange {
        pairs: vec![Pair {
            set_at: at(0),
            set: Ok(()),
            gets: (0..gets)
                .map(|_| Get {
                    at: at(0),
                    reply: Err(errno(n)),
                })
                .collect(),
        }],
    }
}

/// A SET_REPORT that failed with `errno`: nothing was read.
fn exchange_set_failed(n: i32) -> Exchange {
    Exchange {
        pairs: vec![Pair {
            set_at: at(0),
            set: Err(errno(n)),
            gets: Vec::new(),
        }],
    }
}

fn reply(cmd: (u8, u8), status: u8) -> Report {
    let mut r = Report::new(cmd, 0, 0);
    r.status = status;
    r
}

impl Sim {
    fn happy() -> Sim {
        let mut v = reply((0x00, 0x87), 2);
        v.args[..3].copy_from_slice(&[1, 4, 0]);
        Sim {
            calls: Vec::new(),
            clock: 0,
            version: Ok(v),
            enter: Ok(Ok(reply((0x00, 0x04), 2))),
            dongle_left: Some(Duration::from_millis(300)),
            bootloader: Some(bootloader_device(Interface0::Unbound)),
            open: Ok("/dev/bus/usb/008/005".into()),
            descriptor: Ok(DESC.to_vec()),
            query: exchange_ok(Q, 2, 0x009F),
            exit: exchange_ok((0x10, 0x05), 2, 0),
            bootloader_left: Some(Duration::from_millis(200)),
            back: Some(2),
        }
    }
    fn tick(&mut self, what: &'static str) {
        self.calls.push(what);
        self.clock += 1;
    }
}

impl ProbeIo for Sim {
    fn now(&self) -> SystemTime {
        at(self.clock)
    }
    fn describe_device(&self) -> String {
        "1532:009f /dev/hidraw3 interface 0 (Razer Viper Mini SE)".into()
    }
    fn read_version(&mut self) -> Result<Report, String> {
        self.tick("read_version");
        self.version.clone()
    }
    fn enter_bootloader(&mut self) -> Result<Answer, SendError> {
        self.tick("enter_bootloader");
        self.enter.clone()
    }
    fn wait_for_dongle_gone(&mut self, timeout: Duration) -> Option<Duration> {
        assert_eq!(timeout, probe::LEAVE_TIMEOUT);
        self.tick("wait_for_dongle_gone");
        self.dongle_left
    }
    fn wait_for_bootloader(&mut self, timeout: Duration) -> BootloaderWait {
        assert_eq!(timeout, probe::BOOTLOADER_TIMEOUT);
        self.tick("wait_for_bootloader");
        match self.bootloader.clone() {
            Some(device) => BootloaderWait {
                device: Some(device),
                appeared_at: Some(at(self.clock)),
                settled_at: Some(at(self.clock + 1)),
            },
            None => BootloaderWait::default(),
        }
    }
    fn open_usbfs(&mut self, device: &UsbDevice) -> Result<String, OpenFailure> {
        assert_eq!((device.vid, device.pid), BOOTLOADER_ID);
        self.tick("open_usbfs");
        self.open.clone()
    }
    fn read_report_descriptor(&mut self) -> Result<Vec<u8>, SendError> {
        self.tick("read_report_descriptor");
        self.descriptor.clone()
    }
    fn query_bootloader(&mut self) -> Exchange {
        self.tick("query_bootloader");
        self.query.clone()
    }
    fn exit_bootloader(&mut self) -> Exchange {
        self.tick("exit_bootloader");
        self.exit.clone()
    }
    fn wait_for_bootloader_gone(&mut self, timeout: Duration) -> Option<Duration> {
        assert_eq!(timeout, probe::LEAVE_TIMEOUT);
        self.tick("wait_for_bootloader_gone");
        self.bootloader_left
    }
    fn wait_for_return(&mut self, timeout: Duration) -> ReturnWait {
        assert_eq!(timeout, probe::RETURN_TIMEOUT);
        self.tick("wait_for_return");
        ReturnWait {
            back_at: self.back.map(|s| at(self.clock + s)),
            nodes: vec![
                "1532:009f 8-3 bus 8 device 6 bcdDevice 0101 product \"Razer Viper Mini SE Dongle\""
                    .into(),
            ],
        }
    }
}

fn build() -> Build {
    Build {
        version: "0.3.1".into(),
        git_sha: "0123456789abcdef0123456789abcdef01234567".into(),
    }
}

fn run(sim: &mut Sim) -> (ProbeLog, usize) {
    let mut checkpoints = 0;
    let log = probe::run(sim, &build(), &mut |_| checkpoints += 1);
    (log, checkpoints)
}

const HAPPY_CALLS: [&str; 8] = [
    "read_version",
    "enter_bootloader",
    "wait_for_bootloader",
    "open_usbfs",
    "read_report_descriptor",
    "query_bootloader",
    "exit_bootloader",
    "wait_for_return",
];

#[test]
fn a_full_run_goes_through_the_steps_in_order() {
    let mut sim = Sim::happy();
    let (log, checkpoints) = run(&mut sim);
    assert_eq!(sim.calls, HAPPY_CALLS);
    assert_eq!(log.outcome, Some(Outcome::Completed));
    assert!(!log.outcome.as_ref().unwrap().needs_replug());
    assert_eq!(log.version_read.as_deref(), Some("1.04.00"));
    assert_eq!(log.bootloader.as_ref().unwrap().name, "8-3");
    assert_eq!(log.report_descriptor, Some(Ok(DESC.to_vec())));
    assert_eq!(log.returned_at, Some(at(10)));
    // one checkpoint at the start, one after each step's first half, and the final one
    assert!(checkpoints >= 7, "{checkpoints}");
}

#[test]
fn the_report_is_self_explanatory() {
    let mut sim = Sim::happy();
    let (log, _) = run(&mut sim);
    let text = log.render();
    // `RAZERCTL_PROBE_SAMPLE=FILE cargo test` keeps the simulated run's report, to show what the board will paste
    if let Some(p) = std::env::var_os("RAZERCTL_PROBE_SAMPLE") {
        std::fs::write(p, &text).unwrap();
    }
    for needle in [
        "razerctl 0.3.1 git 0123456789abcdef0123456789abcdef01234567",
        "started 2026-10-1", // 1_792_000_000 s
        "RESULT  COMPLETED",
        "1. read version (00:87)",
        "ok, version 1.04.00",
        "2. enter bootloader (00:04 mode 1)",
        "sent without error",
        "3. wait for 1532:110E in /sys/bus/usb/devices",
        "1532:110E listed as 8-3 1.00 s after the wait began",
        "device 8-3  bus 8  device number 5",
        "bcdDevice 009f  product \"RAZER BOOTLOADER\"",
        "driver bound to interface 0: none",
        "kernel descriptors (18 bytes): 12 01 00 02 00 00 00 40 32 15 0e 11 9f 00 01 02 00 01",
        "3b. usbfs: open the node, read the report descriptor",
        "usbfs (step 3b): opened /dev/bus/usb/008/005 as the current user, root was not needed, interface 0 claimed",
        "GET_DESCRIPTOR 81 06 00 22 00 00 00 04 (12 bytes): 06 00 ff 09 01 a1 01 95 5a b1 02 c0",
        "3c. query the bootloader (10:80)",
        "gate: OPEN. The reply is status 2, txn 0, class/id 10:80 and PID 009f, so 10:05 was sent next",
        "query 10:80, SET_REPORT 21 09 00 03 00 00 5a 00: accepted (attempt 1 of up to 3",
        "decoded: status 0x02 txn 0x00 class/id 10:80 PID 009f",
        "settled 1.00 s after it was first seen",
        "3d. exit bootloader (10:05)",
        "exit 10:05, SET_REPORT 21 09 00 03 00 00 5a 00: accepted",
        "GET_REPORT a1 01 00 03 00 00 5a 00: command 10:05 status byte 0x02",
        "4. wait for 1532:009F",
        "1532:009F listed again 4.00 s after the 10:05 send",
        "1532:009F after 10:05: came back 4.00 s after the 10:05 send, 9.00 s after the enter-bootloader send (step 2), 7.00 s after 1532:110E appeared (step 3); back at 2026-10-1",
        "Razer USB devices at the end of step 4 (1)",
        "1532:009f 8-3 bus 8 device 6 bcdDevice 0101",
    ] {
        assert!(text.contains(needle), "missing `{needle}` in:\n{text}");
    }
    assert!(
        !text.contains("NEXT"),
        "no replug instruction after a clean run"
    );
    // the board unplugged the dongle on the first run: nothing shows it ever returns on its own
    assert!(!text.contains("by itself"), "{text}");
    // every step carries a start and an end time
    assert!(text.matches("Z ->").count() >= 6, "{text}");
}

// --- step 2: an error on the send is "entered" only if the dongle leaves the bus --------------------------------------

#[test]
fn an_eproto_on_the_enter_send_with_the_dongle_leaving_counts_as_entered_and_goes_on() {
    // the board's first run (2026-10-21): the write returned EPROTO (71) because the dongle dropped off the bus
    // mid-transfer, which is the command working
    let mut sim = Sim::happy();
    sim.enter = Err(errno(71));
    let (log, _) = run(&mut sim);
    assert_eq!(
        sim.calls,
        [
            "read_version",
            "enter_bootloader",
            "wait_for_dongle_gone",
            "wait_for_bootloader",
            "open_usbfs",
            "read_report_descriptor",
            "query_bootloader",
            "exit_bootloader",
            "wait_for_return"
        ]
    );
    assert_eq!(log.outcome, Some(Outcome::Completed));
    let text = log.render();
    assert!(
        text.contains("the send returned errno 71 (Protocol error (os error 71))"),
        "{text}"
    );
    assert!(
        text.contains(
            "1532:009F left the bus 0.30 s later, so the dongle DID enter the bootloader"
        ),
        "{text}"
    );
}

#[test]
fn an_error_on_the_enter_send_with_the_dongle_staying_is_not_entered() {
    let mut sim = Sim::happy();
    sim.enter = Err(errno(71));
    sim.dongle_left = None;
    let (log, _) = run(&mut sim);
    assert_eq!(
        sim.calls,
        ["read_version", "enter_bootloader", "wait_for_dongle_gone"],
        "nothing may follow an enter send that did not take"
    );
    assert!(matches!(log.outcome, Some(Outcome::EnterFailed(_))));
    assert!(!log.outcome.as_ref().unwrap().needs_replug());
    let text = log.render();
    assert!(text.contains("STOPPED at step 2"), "{text}");
    assert!(text.contains("errno 71"), "{text}");
    assert!(
        text.contains("1532:009F did not leave the bus within 3 s, so it did not enter"),
        "{text}"
    );
}

#[test]
fn an_enter_send_without_an_error_does_not_wait_for_the_dongle_to_leave() {
    let mut sim = Sim::happy();
    let _ = run(&mut sim);
    assert!(!sim.calls.contains(&"wait_for_dongle_gone"));
}

#[test]
fn a_device_that_resets_without_a_reply_is_still_waited_on() {
    let mut sim = Sim::happy();
    sim.enter = Ok(Err("Input/output error (os error 5)".into()));
    let (log, _) = run(&mut sim);
    assert_eq!(log.outcome, Some(Outcome::Completed));
    assert!(log
        .render()
        .contains("sent without error; no reply read (Input/output error"));
}

#[test]
fn a_device_that_refuses_the_bootloader_is_not_waited_on() {
    for status in [3u8, 4, 5] {
        let mut sim = Sim::happy();
        sim.enter = Ok(Ok(reply((0x00, 0x04), status)));
        let (log, _) = run(&mut sim);
        assert_eq!(
            sim.calls,
            ["read_version", "enter_bootloader"],
            "status {status}"
        );
        assert!(matches!(log.outcome, Some(Outcome::EnterFailed(_))));
        assert!(!log.outcome.as_ref().unwrap().needs_replug());
    }
}

// --- step 3: 1532:110E in sysfs ---------------------------------------------------------------------------------------

#[test]
fn step_three_timeout_sends_nothing_further_and_says_replug() {
    let mut sim = Sim::happy();
    sim.bootloader = None;
    let (log, _) = run(&mut sim);
    assert_eq!(
        sim.calls,
        ["read_version", "enter_bootloader", "wait_for_bootloader"],
        "nothing may be sent after the step 3 timeout"
    );
    assert_eq!(log.outcome, Some(Outcome::BootloaderNeverAppeared));
    assert!(log.outcome.as_ref().unwrap().needs_replug());
    let text = log.render();
    assert!(text.contains("STEP 3 TIMED OUT"), "{text}");
    assert!(text.contains("TIMED OUT: no 1532:110E within 10 s. Nothing further was sent"));
    assert!(
        text.contains("NEXT    Unplug the dongle and plug it back in"),
        "{text}"
    );
    assert!(
        !text.contains("udev"),
        "the udev line is for a node that cannot be opened: {text}"
    );
    assert!(text.contains("never listed"), "{text}");
}

#[test]
fn a_bound_driver_is_recorded_and_nothing_is_sent_or_detached() {
    let mut sim = Sim::happy();
    sim.bootloader = Some(bootloader_device(Interface0::Bound("usbhid".into())));
    let (log, _) = run(&mut sim);
    assert_eq!(
        sim.calls,
        ["read_version", "enter_bootloader", "wait_for_bootloader"],
        "a bound driver means no open, no descriptor read, no exit"
    );
    assert!(matches!(log.outcome, Some(Outcome::DriverBound(_))));
    assert!(log.outcome.as_ref().unwrap().needs_replug());
    let text = log.render();
    assert!(text.contains("STOPPED at step 3b"), "{text}");
    assert!(
        text.contains("the kernel driver `usbhid` is bound to interface 0"),
        "{text}"
    );
    assert!(
        text.contains("driver bound to interface 0: `usbhid`"),
        "{text}"
    );
    assert!(
        text.contains("Not detached; nothing was sent to the bootloader"),
        "{text}"
    );
    assert!(text.contains("NEXT    Unplug the dongle"), "{text}");
}

#[test]
fn an_interface_zero_that_is_not_listed_yet_is_not_a_bound_driver() {
    let mut sim = Sim::happy();
    sim.bootloader = Some(bootloader_device(Interface0::Missing));
    let (log, _) = run(&mut sim);
    assert_eq!(log.outcome, Some(Outcome::Completed));
    assert!(log
        .render()
        .contains("driver bound to interface 0: interface 0 is not listed"));
}

// --- step 3b: the usbfs node ------------------------------------------------------------------------------------------

#[test]
fn a_usbfs_node_that_needs_root_is_recorded_and_not_escalated() {
    let mut sim = Sim::happy();
    sim.open = Err(OpenFailure {
        node: "/dev/bus/usb/008/005".into(),
        errno: Some(13),
        message: "Permission denied (os error 13)".into(),
    });
    let (log, _) = run(&mut sim);
    assert_eq!(
        sim.calls,
        [
            "read_version",
            "enter_bootloader",
            "wait_for_bootloader",
            "open_usbfs"
        ],
        "neither the descriptor read nor 10:05 may be attempted without an open node"
    );
    assert!(matches!(
        log.outcome,
        Some(Outcome::UsbfsUnavailable { .. })
    ));
    assert!(log.outcome.as_ref().unwrap().needs_replug());
    let text = log.render();
    assert!(text.contains("STOPPED at step 3b (usbfs)"), "{text}");
    assert!(text.contains("errno 13"), "{text}");
    assert!(
        text.contains("the udev rule does not let this user open it"),
        "{text}"
    );
    assert!(
        text.contains("Nothing was sent to the bootloader"),
        "{text}"
    );
    // both instructions: the udev line, then the replug
    assert!(text.contains("NEXT    Reinstall the udev rule"), "{text}");
    assert!(text.contains("sudo cp udev/70-razerctl.rules"), "{text}");
    assert!(text.contains("NEXT    Unplug the dongle"), "{text}");
    // what was learnt before the failed open is still in the file
    assert!(text.contains("kernel descriptors (18 bytes)"), "{text}");
}

#[test]
fn a_failed_open_that_is_not_a_permission_error_says_so() {
    let mut sim = Sim::happy();
    sim.open = Err(OpenFailure {
        node: "/dev/bus/usb/008/005".into(),
        errno: Some(16),
        message: "Device or resource busy (os error 16)".into(),
    });
    let (log, _) = run(&mut sim);
    assert!(log.render().contains("errno 16"));
    assert!(log.render().contains("this is not a permission error"));
    assert!(
        !log.render().contains("NEXT    Reinstall the udev rule"),
        "the udev line is for a permission error: {}",
        log.render()
    );
    assert!(log.render().contains("NEXT    Unplug the dongle"));
    assert_eq!(sim.calls.last(), Some(&"open_usbfs"));
}

#[test]
fn a_report_descriptor_that_cannot_be_read_is_recorded_and_the_exit_still_goes() {
    let mut sim = Sim::happy();
    sim.descriptor = Err(errno(32));
    let (log, _) = run(&mut sim);
    assert_eq!(sim.calls, HAPPY_CALLS);
    assert_eq!(log.outcome, Some(Outcome::Completed));
    let text = log.render();
    assert!(
        text.contains("GET_DESCRIPTOR 81 06 00 22 00 00 00 04: FAILED, errno 32"),
        "{text}"
    );
}

// --- step 3d: exit ----------------------------------------------------------------------------------------------------

#[test]
fn a_disconnect_during_the_exit_is_the_expected_shape_and_the_run_goes_on() {
    let mut sim = Sim::happy();
    sim.exit = exchange_set_failed(71);
    let (log, _) = run(&mut sim);
    assert_eq!(
        sim.calls,
        [
            "read_version",
            "enter_bootloader",
            "wait_for_bootloader",
            "open_usbfs",
            "read_report_descriptor",
            "query_bootloader",
            "exit_bootloader",
            "wait_for_bootloader_gone",
            "wait_for_return"
        ]
    );
    assert_eq!(log.outcome, Some(Outcome::Completed));
    let text = log.render();
    assert!(
        text.contains("exit 10:05, SET_REPORT 21 09 00 03 00 00 5a 00: errno 71"),
        "{text}"
    );
    assert!(
        text.contains("1532:110E left the bus 0.20 s later, so the exit took effect"),
        "{text}"
    );
}

#[test]
fn a_status_read_that_fails_after_an_accepted_exit_is_recorded_not_fatal() {
    let mut sim = Sim::happy();
    sim.exit = exchange_get_failed(1, 19);
    let (log, _) = run(&mut sim);
    assert_eq!(log.outcome, Some(Outcome::Completed));
    assert!(!sim.calls.contains(&"wait_for_bootloader_gone"));
    let text = log.render();
    assert!(
        text.contains("GET_REPORT read nothing (errno 19 (No such device"),
        "{text}"
    );
    assert!(
        text.contains("GET_REPORT a1 01 00 03 00 00 5a 00: errno 19 (No such device"),
        "{text}"
    );
}

#[test]
fn an_exit_that_failed_with_the_bootloader_still_there_stops_and_says_replug() {
    let mut sim = Sim::happy();
    sim.exit = exchange_set_failed(32);
    sim.bootloader_left = None;
    let (log, _) = run(&mut sim);
    assert_eq!(
        sim.calls.last(),
        Some(&"wait_for_bootloader_gone"),
        "no wait for the return after an exit that did not take"
    );
    assert!(matches!(log.outcome, Some(Outcome::ExitFailed(_))));
    assert!(log.outcome.as_ref().unwrap().needs_replug());
    let text = log.render();
    assert!(text.contains("STOPPED at step 3d"), "{text}");
    assert!(text.contains("errno 32"), "{text}");
    assert!(text.contains("1532:110E is still on the bus"), "{text}");
    assert!(text.contains("NEXT    Unplug the dongle"), "{text}");
}

// --- step 4: 1532:009F is back ----------------------------------------------------------------------------------------

#[test]
fn step_four_timeout_is_written_and_says_replug() {
    let mut sim = Sim::happy();
    sim.back = None;
    let (log, _) = run(&mut sim);
    assert_eq!(log.outcome, Some(Outcome::DidNotReturn));
    assert!(log.outcome.as_ref().unwrap().needs_replug());
    assert!(!log.outcome.as_ref().unwrap().is_success());
    let text = log.render();
    assert!(text.contains("STEP 4 TIMED OUT"), "{text}");
    assert!(text.contains("so it must be replugged"), "{text}");
    assert!(
        !text.contains("by itself"),
        "never imply it comes back by itself: {text}"
    );
    assert!(text.contains("did not come back within 30 s"), "{text}");
    assert!(text.contains("NEXT    Unplug the dongle"), "{text}");
    // everything learnt before the timeout is still in the file
    assert!(text.contains("device 8-3  bus 8"), "{text}");
    assert!(text.contains("exit 10:05, SET_REPORT"), "{text}");
}

#[test]
fn the_return_is_timed_from_the_exit_and_from_the_entry() {
    // 009F back 20 s after the end of the step-4 call: the numbers from the 10:05 send, from the enter send and from the
    // bootloader appearing tell whether 10:05 is what brought it back
    let mut sim = Sim::happy();
    sim.back = Some(20);
    let (log, _) = run(&mut sim);
    let text = log.render();
    assert!(
        text.contains("came back 22.00 s after the 10:05 send, 27.00 s after the enter-bootloader send (step 2), 25.00 s after 1532:110E appeared"),
        "{text}"
    );
}

#[test]
fn a_failed_version_read_stops_before_entering_the_bootloader() {
    let mut sim = Sim::happy();
    sim.version = Err("the receiver can't reach the mouse".into());
    let (log, _) = run(&mut sim);
    assert_eq!(sim.calls, ["read_version"]);
    assert!(matches!(log.outcome, Some(Outcome::ReadVersionFailed(_))));
    assert!(!log.outcome.as_ref().unwrap().needs_replug());
    assert!(log.render().contains("STOPPED at step 1"));
}

#[test]
fn a_version_answer_that_is_not_success_stops_before_entering_the_bootloader() {
    let mut sim = Sim::happy();
    sim.version = Ok(reply((0x00, 0x87), 5));
    let (log, _) = run(&mut sim);
    assert_eq!(sim.calls, ["read_version"]);
    assert!(log.render().contains("NotSupported"), "{}", log.render());
}

#[test]
fn an_unfinished_log_says_so() {
    let log = ProbeLog::new(build(), at(0), "dev".into());
    let text = log.render();
    assert!(text.contains("RESULT  INCOMPLETE"), "{text}");
}

#[test]
fn timestamps_are_utc_iso() {
    assert_eq!(utc(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    assert_eq!(
        utc(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
        "2023-11-14T22:13:20.000Z"
    );
    assert_eq!(
        utc(UNIX_EPOCH + Duration::from_millis(1_709_208_000_123)),
        "2024-02-29T12:00:00.123Z"
    );
    assert_eq!(
        utc(UNIX_EPOCH + Duration::from_secs(951_782_400)),
        "2000-02-29T00:00:00.000Z"
    );
}

#[test]
fn ids_and_timeouts_are_the_ones_the_plan_names() {
    assert_eq!(NORMAL_ID, (0x1532, 0x009F));
    assert_eq!(BOOTLOADER_ID, (0x1532, 0x110E));
    // steps 2 and 3 are short: the bootloader enumerated about a second after the enter frame
    assert_eq!(probe::BOOTLOADER_TIMEOUT, Duration::from_secs(10));
    assert_eq!(probe::LEAVE_TIMEOUT, Duration::from_secs(3));
    // the wait for the return starts after the exit send; nothing shows the bootloader leaves by itself, so it is short
    assert_eq!(probe::RETURN_TIMEOUT, Duration::from_secs(30));
}

// ---------------------------------------------------------------------------------------------------------------------
// The kernel's USB device list, against a fixture tree (never the real /sys)

/// `iface0`: `None` = no `:1.0` directory; `Some(None)` = there, no driver; `Some(Some(d))` = driver `d` bound.
#[allow(clippy::too_many_arguments)]
fn fake_usb(
    root: &Path,
    name: &str,
    id: (u16, u16),
    bcd: u16,
    product: &str,
    (bus, dev): (u32, u32),
    iface0: Option<Option<&str>>,
) {
    let d = root.join(name);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("busnum"), format!("{bus}\n")).unwrap();
    std::fs::write(d.join("devnum"), format!("{dev}\n")).unwrap();
    std::fs::write(d.join("bcdDevice"), format!("{bcd:04x}\n")).unwrap();
    std::fs::write(d.join("product"), format!("{product}\n")).unwrap();
    std::fs::write(d.join("descriptors"), KERNEL_DESC).unwrap();
    std::fs::write(d.join("idProduct"), format!("{:04x}\n", id.1)).unwrap();
    // the ids come last: a scan that sees them sees the rest
    std::fs::write(d.join("idVendor"), format!("{:04x}\n", id.0)).unwrap();
    if let Some(driver) = iface0 {
        let i = root.join(format!("{name}:1.0"));
        std::fs::create_dir_all(&i).unwrap();
        std::fs::write(i.join("bInterfaceNumber"), "00\n").unwrap();
        if let Some(n) = driver {
            std::os::unix::fs::symlink(
                format!("../../../../bus/usb/drivers/{n}"),
                i.join("driver"),
            )
            .unwrap();
        }
    }
}

fn fixture_roots(name: &str) -> (PathBuf, Roots) {
    let base = scratch(name);
    let roots = Roots {
        sysfs: base.join("sys/bus/usb/devices"),
        dev: base.join("dev/bus/usb"),
    };
    std::fs::create_dir_all(&roots.sysfs).unwrap();
    std::fs::create_dir_all(&roots.dev).unwrap();
    (base, roots)
}

fn dongle_in(root: &Path) {
    fake_usb(
        root,
        "8-3",
        NORMAL_ID,
        0x0101,
        "Razer Viper Mini SE Dongle",
        (8, 4),
        Some(Some("usbhid")),
    );
}

/// A probe over fixture directories. Its control node is `/dev/null`, which nothing here writes to.
fn fixture_probe(roots: Roots) -> RealProbe {
    let node = razerctl::hidraw::HidrawNode {
        dev: PathBuf::from("/dev/null"),
        sysfs: PathBuf::from("/nonexistent"),
        vid: 0x1532,
        pid: 0x009F,
        interface: Some(0),
        name: "Razer Viper Mini SE".into(),
        has_feature_reports: true,
    };
    RealProbe::open(&node, roots).unwrap()
}

#[test]
fn sysfs_scan_reads_every_field_the_report_needs() {
    let (base, roots) = fixture_roots("scan");
    fake_usb(
        &roots.sysfs,
        "8-3",
        BOOTLOADER_ID,
        0x009f,
        "RAZER BOOTLOADER",
        (8, 5),
        Some(None),
    );
    let found = sysfs::find(&roots.sysfs, BOOTLOADER_ID, at(7)).unwrap();
    assert_eq!(found, {
        let mut d = bootloader_device(Interface0::Unbound);
        d.seen_at = at(7);
        d
    });
    assert_eq!(
        found.summary(),
        "1532:110e 8-3 bus 8 device 5 bcdDevice 009f product \"RAZER BOOTLOADER\""
    );
    assert_eq!(
        sysfs::usbfs_path(Path::new("/dev/bus/usb"), 8, 5),
        Path::new("/dev/bus/usb/008/005")
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn sysfs_find_ignores_interfaces_hubs_and_other_ids() {
    let (base, roots) = fixture_roots("find");
    dongle_in(&roots.sysfs);
    // a root hub and an unrelated device, and an interface directory that carries the bootloader's ids
    fake_usb(
        &roots.sysfs,
        "usb8",
        (0x1d6b, 0x0002),
        0x0700,
        "xHCI Host Controller",
        (8, 1),
        None,
    );
    let ifc = roots.sysfs.join("8-3:1.0");
    std::fs::write(ifc.join("idVendor"), "1532\n").unwrap();
    std::fs::write(ifc.join("idProduct"), "110e\n").unwrap();
    assert!(sysfs::find(&roots.sysfs, BOOTLOADER_ID, at(0)).is_none());
    assert!(sysfs::present(&roots.sysfs, NORMAL_ID));
    assert!(!sysfs::present(&roots.sysfs, BOOTLOADER_ID));
    let all = sysfs::scan(&roots.sysfs, at(0));
    assert_eq!(
        all.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
        ["8-3", "usb8"]
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn a_missing_sysfs_root_is_an_empty_list() {
    let (base, _) = fixture_roots("missing");
    assert!(sysfs::scan(&base.join("nope"), at(0)).is_empty());
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn a_bound_an_unbound_and_a_missing_interface_zero_are_told_apart() {
    let (base, roots) = fixture_roots("iface0");
    fake_usb(
        &roots.sysfs,
        "1-1",
        BOOTLOADER_ID,
        0x9f,
        "A",
        (1, 2),
        Some(Some("usbhid")),
    );
    fake_usb(
        &roots.sysfs,
        "1-2",
        BOOTLOADER_ID,
        0x9f,
        "B",
        (1, 3),
        Some(None),
    );
    fake_usb(&roots.sysfs, "1-3", BOOTLOADER_ID, 0x9f, "C", (1, 4), None);
    let all = sysfs::scan(&roots.sysfs, at(0));
    assert_eq!(all[0].interface0, Interface0::Bound("usbhid".into()));
    assert_eq!(all[1].interface0, Interface0::Unbound);
    assert_eq!(all[2].interface0, Interface0::Missing);
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn unreadable_descriptors_are_reported_not_hidden() {
    let (base, roots) = fixture_roots("nodesc");
    fake_usb(
        &roots.sysfs,
        "8-3",
        BOOTLOADER_ID,
        0x9f,
        "B",
        (8, 5),
        Some(None),
    );
    std::fs::remove_file(roots.sysfs.join("8-3/descriptors")).unwrap();
    let d = sysfs::find(&roots.sysfs, BOOTLOADER_ID, at(0)).unwrap();
    assert!(d.descriptors.is_err());
    let mut log = ProbeLog::new(build(), at(0), "dev".into());
    log.push(probe::report::Step::new("3", "x", at(0), at(1), "y".into()));
    log.bootloader = Some(d);
    assert!(log
        .render()
        .contains("kernel descriptors: could not be read"));
    let _ = std::fs::remove_dir_all(base);
}

// --- the real transport's waits, against a fixture tree ---------------------------------------------------------------

fn after<F: FnOnce() + Send + 'static>(ms: u64, f: F) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(ms));
        f()
    })
}

#[test]
fn a_bootloader_listed_in_sysfs_with_no_hidraw_node_is_found() {
    // the board's first run: 1532:110E enumerated, usbhid refused it, and no hidraw node was ever made. The listing in
    // sysfs is the only place it shows up
    let (base, roots) = fixture_roots("find110e");
    let mut p = fixture_probe(roots.clone());
    let sys = roots.sysfs.clone();
    let t = after(200, move || {
        fake_usb(
            &sys,
            "8-3",
            BOOTLOADER_ID,
            0x009f,
            "RAZER BOOTLOADER",
            (8, 5),
            Some(None),
        )
    });
    let w = p.wait_for_bootloader(Duration::from_secs(5));
    t.join().unwrap();
    let d = w.device.expect("the bootloader was not found");
    assert_eq!((d.vid, d.pid), BOOTLOADER_ID);
    assert_eq!((d.bus, d.dev), (Some(8), Some(5)));
    assert_eq!(d.bcd_device, Some(0x009f));
    assert_eq!(d.product.as_deref(), Some("RAZER BOOTLOADER"));
    assert_eq!(d.interface0, Interface0::Unbound);
    assert_eq!(d.descriptors, Ok(KERNEL_DESC.to_vec()));
    assert!(w.appeared_at.is_some());
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn a_wait_that_saw_only_the_dongle_is_a_timeout_not_a_bootloader() {
    let (base, roots) = fixture_roots("only009f");
    dongle_in(&roots.sysfs);
    let mut p = fixture_probe(roots);
    let w = p.wait_for_bootloader(Duration::from_millis(400));
    assert!(w.device.is_none() && w.appeared_at.is_none());
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn the_dongle_leaving_sysfs_is_seen_and_a_dongle_that_stays_is_not() {
    let (base, roots) = fixture_roots("gone");
    dongle_in(&roots.sysfs);
    let mut p = fixture_probe(roots.clone());
    assert_eq!(p.wait_for_dongle_gone(Duration::from_millis(300)), None);
    let dir = roots.sysfs.join("8-3");
    let t = after(250, move || std::fs::remove_dir_all(dir).unwrap());
    let d = p
        .wait_for_dongle_gone(Duration::from_secs(5))
        .expect("the dongle left");
    t.join().unwrap();
    assert!(d >= Duration::from_millis(100), "{d:?}");
    // already gone: immediate
    std::fs::remove_dir_all(roots.sysfs.join("8-3:1.0")).unwrap();
    assert!(
        p.wait_for_dongle_gone(Duration::from_millis(300)).unwrap() < Duration::from_millis(50)
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn the_bootloader_leaving_sysfs_is_seen_and_one_that_stays_is_not() {
    let (base, roots) = fixture_roots("blgone");
    fake_usb(
        &roots.sysfs,
        "8-3",
        BOOTLOADER_ID,
        0x9f,
        "B",
        (8, 5),
        Some(None),
    );
    let mut p = fixture_probe(roots.clone());
    assert_eq!(p.wait_for_bootloader_gone(Duration::from_millis(300)), None);
    let dir = roots.sysfs.join("8-3");
    let t = after(250, move || std::fs::remove_dir_all(dir).unwrap());
    assert!(p.wait_for_bootloader_gone(Duration::from_secs(5)).is_some());
    t.join().unwrap();
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn the_dongle_is_back_only_when_009f_is_listed_and_110e_is_not() {
    let (base, roots) = fixture_roots("back");
    // both listed (the bootloader has not gone yet): not back
    dongle_in(&roots.sysfs);
    fake_usb(
        &roots.sysfs,
        "8-4",
        BOOTLOADER_ID,
        0x9f,
        "B",
        (8, 5),
        Some(None),
    );
    let mut p = fixture_probe(roots.clone());
    let r = p.wait_for_return(Duration::from_millis(300));
    assert!(r.back_at.is_none());
    // the bootloader goes: back
    std::fs::remove_dir_all(roots.sysfs.join("8-4")).unwrap();
    let r = p.wait_for_return(Duration::from_secs(2));
    assert!(r.back_at.is_some());
    assert_eq!(r.nodes.len(), 1, "{:?}", r.nodes);
    assert!(r.nodes[0].starts_with("1532:009f 8-3 bus 8 device 4 bcdDevice 0101"));
    // nothing listed at all: not back
    std::fs::remove_dir_all(roots.sysfs.join("8-3")).unwrap();
    assert!(p
        .wait_for_return(Duration::from_millis(300))
        .back_at
        .is_none());
    let _ = std::fs::remove_dir_all(base);
}

// --- opening the usbfs node, against a fixture directory --------------------------------------------------------------

fn fixture_bootloader() -> UsbDevice {
    bootloader_device(Interface0::Unbound)
}

#[test]
fn a_missing_usbfs_node_is_an_open_failure_with_its_errno() {
    let (base, roots) = fixture_roots("nonode");
    let mut p = fixture_probe(roots);
    let f = p.open_usbfs(&fixture_bootloader()).unwrap_err();
    assert_eq!(f.errno, Some(2));
    assert!(f.node.ends_with("008/005"), "{f}");
    // nothing was opened, so nothing can be sent
    for x in [p.query_bootloader(), p.exit_bootloader()] {
        assert!(x.set_error().is_some() && x.last_get().is_none());
    }
    assert!(p.read_report_descriptor().is_err());
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn a_usbfs_node_the_user_may_not_open_is_an_eacces_not_an_escalation() {
    let (base, roots) = fixture_roots("eacces");
    let node = roots.dev.join("008/005");
    std::fs::create_dir_all(node.parent().unwrap()).unwrap();
    std::fs::write(&node, "").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    // root opens anything; the fixture proves nothing then
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: running as root");
        let _ = std::fs::remove_dir_all(base);
        return;
    }
    let mut p = fixture_probe(roots);
    let f = p.open_usbfs(&fixture_bootloader()).unwrap_err();
    assert_eq!(f.errno, Some(13), "{f}");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn a_node_that_opens_but_cannot_be_claimed_is_an_open_failure_and_is_not_kept() {
    // a regular file takes the open and refuses the ioctl (ENOTTY, 25), the same shape as a claim refused by the kernel
    let (base, roots) = fixture_roots("noclaim");
    let node = roots.dev.join("008/005");
    std::fs::create_dir_all(node.parent().unwrap()).unwrap();
    std::fs::write(&node, "").unwrap();
    let mut p = fixture_probe(roots);
    let f = p.open_usbfs(&fixture_bootloader()).unwrap_err();
    assert_eq!(f.errno, Some(25), "{f}");
    for x in [p.query_bootloader(), p.exit_bootloader()] {
        assert!(
            x.set_error().unwrap().message.contains("never opened"),
            "an interface that could not be claimed must not be used"
        );
    }
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn a_bootloader_without_a_bus_and_device_number_is_an_open_failure() {
    let (base, roots) = fixture_roots("nobus");
    let mut p = fixture_probe(roots);
    let mut d = fixture_bootloader();
    d.bus = None;
    let f = p.open_usbfs(&d).unwrap_err();
    assert!(f.message.contains("no busnum/devnum"), "{f}");
    let _ = std::fs::remove_dir_all(base);
}

// ---------------------------------------------------------------------------------------------------------------------
// Refusals: other razerctl or openrazer traffic on the device

#[test]
fn nothing_in_the_way_is_no_refusal() {
    assert!(probe::refusals(&Traffic::default()).is_empty());
}

#[test]
fn each_kind_of_other_traffic_is_a_refusal() {
    let cases: Vec<(&str, Traffic, &str)> = vec![
        (
            "razerctl daemon or GUI",
            Traffic {
                razerctl: vec![(41, "razerctl remap".into())],
                ..Default::default()
            },
            "another razerctl is running: pid 41",
        ),
        (
            "openrazer daemon",
            Traffic {
                openrazer_daemons: vec![(7, "python3 /usr/bin/openrazer-daemon".into())],
                ..Default::default()
            },
            "openrazer-daemon is running: pid 7",
        ),
        (
            "openrazer kernel driver",
            Traffic {
                openrazer_drivers: vec![("/dev/hidraw3".into(), "razermouse".into())],
                ..Default::default()
            },
            "driver `razermouse` is bound to /dev/hidraw3",
        ),
        (
            "any other holder of the node",
            Traffic {
                holders: vec![(99, "polychromatic".into(), "/dev/hidraw3".into())],
                ..Default::default()
            },
            "pid 99 `polychromatic` has /dev/hidraw3 open",
        ),
    ];
    for (what, t, needle) in cases {
        let r = probe::refusals(&t);
        assert_eq!(r.len(), 1, "{what}: {r:?}");
        assert!(r[0].contains(needle), "{what}: {r:?}");
    }
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("razerctl-probe-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn fake_process(proc_root: &Path, pid: u32, comm: &str, cmdline: &[&str], fds: &[&str]) {
    let p = proc_root.join(pid.to_string());
    std::fs::create_dir_all(p.join("fd")).unwrap();
    std::fs::write(p.join("comm"), format!("{comm}\n")).unwrap();
    std::fs::write(p.join("cmdline"), cmdline.join("\0") + "\0").unwrap();
    for (i, target) in fds.iter().enumerate() {
        std::os::unix::fs::symlink(target, p.join("fd").join(i.to_string())).unwrap();
    }
}

fn fake_node(sys: &Path, name: &str, driver: Option<&str>) -> razerctl::hidraw::HidrawNode {
    let dir = sys.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    if let Some(d) = driver {
        std::os::unix::fs::symlink(format!("/sys/bus/hid/drivers/{d}"), dir.join("driver"))
            .unwrap();
    }
    razerctl::hidraw::HidrawNode {
        dev: PathBuf::from(format!("/dev/{name}")),
        sysfs: dir,
        vid: 0x1532,
        pid: 0x009F,
        interface: Some(0),
        name: "Razer Viper Mini SE".into(),
        has_feature_reports: true,
    }
}

#[test]
fn traffic_detection_finds_each_kind_in_a_fake_proc_and_sys() {
    let root = scratch("traffic");
    let (proc_root, sys) = (root.join("proc"), root.join("sys"));
    std::fs::create_dir_all(&proc_root).unwrap();
    let bound = fake_node(&sys, "hidraw3", Some("razermouse"));
    let plain = fake_node(&sys, "hidraw4", Some("hid-generic"));
    let nodes = [bound, plain];

    fake_process(&proc_root, 100, "bash", &["bash"], &[]);
    fake_process(
        &proc_root,
        101,
        "razerctl",
        &["razerctl", "remap"],
        &["/dev/input/event5"],
    );
    fake_process(
        &proc_root,
        102,
        "razerctl-gui",
        &["/usr/bin/razerctl-gui"],
        &[],
    );
    fake_process(
        &proc_root,
        103,
        "python3",
        &["python3", "/usr/bin/openrazer-daemon", "-F"],
        &[],
    );
    fake_process(
        &proc_root,
        104,
        "openrazer-daemo",
        &["openrazer-daemon"],
        &[],
    );
    fake_process(
        &proc_root,
        105,
        "polychromatic",
        &["polychromatic"],
        &["/dev/hidraw4", "/dev/null"],
    );
    fake_process(
        &proc_root,
        106,
        "razerctl",
        &["razerctl", "firmware", "probe-bootloader"],
        &[],
    ); // me

    let (t, unchecked) = hw::collect_traffic(&proc_root, &nodes, 106);
    assert_eq!(unchecked, 0);
    assert_eq!(
        t.razerctl.iter().map(|p| p.0).collect::<Vec<_>>(),
        [101, 102],
        "me (106) is not other traffic"
    );
    assert_eq!(t.razerctl[0].1, "razerctl remap");
    assert_eq!(
        t.openrazer_daemons.iter().map(|p| p.0).collect::<Vec<_>>(),
        [103, 104]
    );
    assert_eq!(
        t.openrazer_drivers,
        [("/dev/hidraw3".to_string(), "razermouse".to_string())]
    );
    assert_eq!(
        t.holders,
        [(105, "polychromatic".to_string(), "/dev/hidraw4".to_string())]
    );
    assert_eq!(probe::refusals(&t).len(), 2 + 2 + 1 + 1);

    // only bash and me: clean
    let clean = scratch("traffic-clean");
    fake_process(&clean, 100, "bash", &["bash"], &["/dev/pts/0"]);
    fake_process(&clean, 106, "razerctl", &["razerctl"], &["/dev/hidraw3"]);
    let (t, _) = hw::collect_traffic(
        &clean,
        &[fake_node(&sys, "hidraw5", Some("hid-generic"))],
        106,
    );
    assert_eq!(t, Traffic::default());
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(clean);
}

#[test]
fn processes_whose_files_cannot_be_read_are_counted_not_ignored() {
    let proc_root = scratch("unreadable");
    let p = proc_root.join("200");
    std::fs::create_dir_all(&p).unwrap();
    std::fs::write(p.join("comm"), "other\n").unwrap();
    // no fd directory at all, like another user's process
    let (_, unchecked) = hw::collect_traffic(&proc_root, &[], 1);
    assert_eq!(unchecked, 1);
    let _ = std::fs::remove_dir_all(proc_root);
}

// ---------------------------------------------------------------------------------------------------------------------
// The command line

fn bin() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_razerctl"))
}

#[test]
fn argument_parsing() {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert_eq!(
        cli::parse(&s(&[])).unwrap(),
        cli::Args {
            yes: false,
            out: None
        }
    );
    assert_eq!(
        cli::parse(&s(&["--yes-enter-bootloader", "--out", "p.txt"])).unwrap(),
        cli::Args {
            yes: true,
            out: Some("p.txt".into())
        }
    );
    assert!(cli::parse(&s(&["--yes"])).is_err());
    assert!(cli::parse(&s(&["--out"])).is_err());
    assert!(cli::parse(&s(&["--yes-enter-bootloader=1"])).is_err());
}

#[test]
fn without_the_flag_it_prints_the_plan_and_exits_non_zero() {
    // NOTE: never add `--yes-enter-bootloader` to a test that runs the binary: on a machine with the dongle it would act.
    let out = bin()
        .args(["firmware", "probe-bootloader"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    for needle in [
        "would, on the Razer dongle 1532:009F",
        "1. read its version (00:87)",
        "2. send enter-bootloader (00:04, mode 1)",
        "3. wait up to 10 s",
        "3b. after letting it settle for at least 1000 ms",
        "3c. send the query Razer's updater sends first (10:80)",
        "3d. only if the reply has status 2 and PID 009F or 009E: send exit (10:05) straight away",
        "00:87, 00:04, 10:80 and 10:05",
        "4. wait up to 30 s",
        "never erases or writes firmware",
        "Nothing was sent",
        "--yes-enter-bootloader",
    ] {
        assert!(stdout.contains(needle), "missing `{needle}` in:\n{stdout}");
    }
    assert!(
        stderr.contains("refusing to run without --yes-enter-bootloader"),
        "{stderr}"
    );
}

#[test]
fn an_unknown_argument_is_refused_without_the_plan_and_without_a_device() {
    let out = bin()
        .args(["firmware", "probe-bootloader", "--yes"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unexpected argument '--yes'"));
}

#[test]
fn the_output_file_is_never_overwritten() {
    let d = scratch("out");
    let p = d.join("probe.txt");
    cli::create_out(&p).unwrap();
    std::fs::write(&p, "earlier probe").unwrap();
    let e = cli::create_out(&p).unwrap_err();
    assert!(e.contains("nothing was sent to the device"), "{e}");
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "earlier probe");
    assert!(cli::create_out(&d.join("no/such/dir/p.txt")).is_err());
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn the_build_stamp_names_a_commit() {
    let b = cli::build();
    assert_eq!(b.version, env!("CARGO_PKG_VERSION"));
    assert!(
        b.git_sha == "unknown"
            || (b.git_sha.len() == 40 && b.git_sha.bytes().all(|c| c.is_ascii_hexdigit())),
        "{}",
        b.git_sha
    );
}

// ---------------------------------------------------------------------------------------------------------------------
// The fence: no path from a real device to erase, program or a class 0x0A flash frame.

fn src_files() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push((
                    p.strip_prefix(env!("CARGO_MANIFEST_DIR"))
                        .unwrap()
                        .display()
                        .to_string(),
                    std::fs::read_to_string(&p).unwrap(),
                ));
            }
        }
    }
    out.sort();
    out
}

/// The code of a file: no `//` comment lines, and no `//` tails (the string literals here contain no `//`).
fn code(src: &str) -> Vec<String> {
    src.lines()
        .map(|l| l.split("//").next().unwrap_or("").to_string())
        .filter(|l| !l.trim().is_empty())
        .collect()
}

fn tokens(line: &str) -> Vec<&str> {
    line.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
        .collect()
}

fn squeeze(line: &str) -> String {
    line.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_lowercase()
}

/// Identifiers that only the phase-1 DFU / class 0x0A code has.
const DFU_NAMES: [&str; 14] = [
    "dfu",
    "flash",
    "sim",
    "Transport",
    "BootloaderSeen",
    "erase_frame",
    "program_frame",
    "verify_frame",
    "ERASE",
    "PROGRAM",
    "VERIFY",
    "RECORD",
    "REGION_INFO",
    "stream_frames",
];

/// Frame constants no probe code may spell: erase `10:01`, program `10:02`, any class `0x0A` command.
const FORBIDDEN_COMMANDS: [&str; 3] = ["(0x10,0x01)", "(0x10,0x02)", "(0x0a,"];

fn touches_a_device(src: &str) -> bool {
    code(src).iter().any(|l| {
        tokens(l).contains(&"Hidraw")
            || tokens(l).contains(&"Usbfs")
            || l.contains("hidraw::")
            || l.contains("/dev/hidraw")
            || l.contains("/dev/bus/usb")
            || l.contains("libc::ioctl")
    })
}

#[test]
fn fence_probe_cannot_name_the_dfu_code_or_a_destructive_command() {
    let mut checked = 0;
    for (name, src) in src_files()
        .into_iter()
        .filter(|(n, _)| n.starts_with("src/probe/"))
    {
        checked += 1;
        for l in code(&src) {
            for t in tokens(&l) {
                assert!(!DFU_NAMES.contains(&t), "{name} names `{t}`: {l}");
            }
            assert!(
                !l.contains("firmware::"),
                "{name} reaches into the firmware module: {l}"
            );
            let sq = squeeze(&l);
            for c in FORBIDDEN_COMMANDS {
                assert!(
                    !sq.contains(c),
                    "{name} spells the destructive command {c}: {l}"
                );
            }
        }
    }
    assert_eq!(
        checked, 6,
        "src/probe/ gained or lost a file: review the fence"
    );
}

#[test]
fn fence_only_four_frames_are_constructed_and_only_in_the_probe_root() {
    let files = src_files();
    let mut news = Vec::new();
    for (name, src) in files.iter().filter(|(n, _)| n.starts_with("src/probe/")) {
        for l in code(src) {
            if l.contains("Report::new(") || l.contains(".with_args(") {
                news.push(format!("{name}: {}", l.trim()));
            }
        }
    }
    assert_eq!(
        news,
        [
            "src/probe/mod.rs: Report::new(READ_VERSION, 8, TID)",
            "src/probe/mod.rs: Report::new(ENTER_MODE, 2, TID).with_args(&[1, 0])",
            "src/probe/mod.rs: Report::new(QUERY, 9, TID)",
            "src/probe/mod.rs: Report::new(EXIT, 0, TID)",
        ],
        "a probe module builds a frame other than the four"
    );
}

#[test]
fn fence_the_real_transport_implements_only_the_probe_trait() {
    let files = src_files();
    let impls = |name: &str| -> Vec<String> {
        let (_, src) = files.iter().find(|(n, _)| n == name).unwrap();
        code(src)
            .into_iter()
            .map(|l| l.trim().to_string())
            .filter(|l| l.starts_with("impl"))
            .collect()
    };
    assert_eq!(
        impls("src/probe/hw.rs"),
        [
            "impl<T: FeatureIo> FeatureIo for Guarded<T> {",
            "impl Roots {",
            "impl RealProbe {",
            "impl ProbeIo for RealProbe {",
        ]
    );
    // ProbeIo is implemented by the real transport and by nothing else in the library
    for (name, src) in &files {
        for l in code(src) {
            if l.contains("ProbeIo for") {
                assert_eq!(name, "src/probe/hw.rs", "{l}");
            }
        }
    }
    // no real-transport type implements the phase-1 DFU trait: the one implementation is the simulated device
    let mut dfu_impls = Vec::new();
    for (name, src) in &files {
        for l in code(src) {
            if tokens(&l).windows(2).any(|w| w == ["Transport", "for"]) {
                dfu_impls.push(format!("{name}: {}", l.trim()));
            }
        }
    }
    assert_eq!(dfu_impls.len(), 1, "{dfu_impls:#?}");
    assert!(
        dfu_impls[0].starts_with("src/firmware/sim.rs"),
        "{dfu_impls:#?}"
    );
}

#[test]
fn fence_no_device_touching_module_can_reach_the_dfu_code() {
    let mut device_files = Vec::new();
    for (name, src) in src_files() {
        if name.starts_with("src/firmware/") || !touches_a_device(&src) {
            continue;
        }
        device_files.push(name.clone());
        // the binaries print usage text that names `firmware flash`, and dispatch to `firmware::cli`; they may do no more
        let binary = name == "src/main.rs" || name.starts_with("src/bin/");
        let banned: &[&str] = if binary {
            &["dfu", "Transport", "BootloaderSeen"]
        } else {
            &["dfu", "flash", "Transport", "BootloaderSeen"]
        };
        for l in code(&src) {
            let t = tokens(&l);
            for b in banned {
                assert!(
                    !t.contains(b),
                    "{name} touches a device and names `{b}`: {l}"
                );
            }
            if l.contains("firmware::") {
                assert!(
                    binary && l.contains("razerctl::firmware::cli("),
                    "{name} touches a device and uses the firmware module: {l}"
                );
            }
        }
    }
    assert!(
        device_files.contains(&"src/probe/hw.rs".to_string()),
        "{device_files:?}"
    );
    assert!(
        device_files.contains(&"src/hidraw.rs".to_string()),
        "{device_files:?}"
    );
    for f in ["src/probe/usbfs.rs", "src/probe/hw.rs"] {
        assert!(
            device_files.contains(&f.to_string()),
            "{f} is not seen as touching a device: {device_files:?}"
        );
    }
}

#[test]
fn fence_the_destructive_frames_exist_only_in_the_phase_one_modules() {
    for (name, src) in src_files() {
        if matches!(
            name.as_str(),
            "src/firmware/dfu.rs" | "src/firmware/flash.rs"
        ) {
            continue;
        }
        for l in code(&src) {
            let sq = squeeze(&l);
            for c in FORBIDDEN_COMMANDS {
                assert!(!sq.contains(c), "{name} spells {c}: {l}");
            }
        }
    }
}

#[test]
fn fence_every_write_in_the_real_transport_goes_through_the_guard() {
    let files = src_files();
    let (_, hw) = files.iter().find(|(n, _)| n == "src/probe/hw.rs").unwrap();
    let lines = code(hw);
    // the raw ioctl wrappers are called in two places: the guard's own delegation, and the guarded `send_and_listen`
    let sends: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("send_feature("))
        .collect();
    assert_eq!(
        sends.iter().map(|l| l.trim()).collect::<Vec<_>>(),
        [
            "fn send_feature(&self, payload: &[u8; REPORT_LEN]) -> io::Result<()> {",
            "self.0.send_feature(payload)",
            "io.send_feature(&frame.to_bytes())?;",
        ]
    );
    // the guard checks the allow-list before it delegates
    let guard_at = lines
        .iter()
        .position(|l| l.contains("if !permitted(payload)"))
        .expect("the guard checks permitted()");
    let delegate_at = lines
        .iter()
        .position(|l| l.contains("self.0.send_feature(payload)"))
        .unwrap();
    assert!(guard_at < delegate_at);
    // no field or binding holds a bare Hidraw: every one is wrapped in Guarded
    for l in &lines {
        let bare = tokens(l).contains(&"Hidraw")
            && !l.contains("Guarded")
            && !l.contains("Hidraw::open(")
            && !l.trim_start().starts_with("use ");
        assert!(!bare, "an unguarded Hidraw: {l}");
    }
    // raw hidraw write entry points are not used around the guard
    for banned in [
        "HIDIOCSFEATURE",
        ".ioctl(",
        "libc::",
        "write_all",
        ".write(",
        "OpenOptions",
    ] {
        assert!(!hw.contains(banned), "hw.rs uses `{banned}`");
    }
}

// --- the usbfs transport (DAN-1929) -----------------------------------------------------------------------------------

fn src_file(name: &str) -> String {
    src_files()
        .into_iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("{name} is gone"))
        .1
}

/// The ioctl request constants of <linux/usbdevice_fs.h> that usbfs.rs may name.
const USBFS_ALLOWED_IOCTLS: [&str; 2] = ["USBDEVFS_CONTROL", "USBDEVFS_CLAIMINTERFACE"];

/// Words that only a usbfs reset, reconfiguration, driver detach or non-control transfer would need.
const USBFS_BANNED_WORDS: [&str; 12] = [
    "reset",
    "detach",
    "disconnect",
    "setconfig",
    "set_config",
    "setinterface",
    "set_interface",
    "bulk",
    "submiturb",
    "clear_halt",
    "clearhalt",
    "drop_priv",
];

#[test]
fn fence_the_usbfs_transport_issues_only_the_control_and_claim_ioctls() {
    let lines = code(&src_file("src/probe/usbfs.rs"));
    // one raw ioctl call in the file, in the one private method
    let raw: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("libc::ioctl("))
        .collect();
    assert_eq!(raw.len(), 1, "{raw:?}");
    // that method is called twice, once per allowed request
    let calls: Vec<String> = lines
        .iter()
        .filter(|l| l.contains("self.ioctl("))
        .map(|l| l.trim().to_string())
        .collect();
    assert_eq!(
        calls,
        [
            "self.ioctl(USBDEVFS_CLAIMINTERFACE, (&mut n as *mut u32).cast())",
            "self.ioctl(USBDEVFS_CONTROL, (&mut t as *mut CtrlTransfer).cast())",
        ]
    );
    // every request constant named is one of the two
    for l in &lines {
        for t in tokens(l) {
            if t.starts_with("USBDEVFS_") {
                assert!(
                    USBFS_ALLOWED_IOCTLS.contains(&t),
                    "usbfs.rs names `{t}`: {l}"
                );
            }
        }
        let low = l.to_lowercase();
        for w in USBFS_BANNED_WORDS {
            assert!(!low.contains(w), "usbfs.rs contains `{w}`: {l}");
        }
    }
    // and the ioctl is nowhere else in the probe, nor anywhere in the library but hidraw.rs
    for (name, src) in src_files() {
        if name == "src/hidraw.rs" || name == "src/probe/usbfs.rs" {
            continue;
        }
        for l in code(&src) {
            assert!(!l.contains("libc::ioctl"), "{name} issues an ioctl: {l}");
            assert!(
                name == "src/probe/hw.rs" || !l.contains("/dev/bus/usb"),
                "{name} names /dev/bus/usb: {l}"
            );
        }
    }
}

#[test]
fn fence_the_usbfs_transport_implements_only_the_control_traits_and_has_three_requests() {
    let lines: Vec<String> = code(&src_file("src/probe/usbfs.rs"))
        .into_iter()
        .map(|l| l.trim().to_string())
        .collect();
    let impls: Vec<&String> = lines.iter().filter(|l| l.starts_with("impl")).collect();
    assert_eq!(
        impls,
        [
            "impl Setup {",
            "impl<T: ControlIo> ControlIo for UsbGuarded<T> {",
            "impl Usbfs {",
            "impl ControlIo for Usbfs {",
        ],
        "a trait other than the control channel is implemented for a usbfs type"
    );
    // the Setup constructors: the three requests, and nothing else builds a request
    let ctors: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("pub const fn "))
        .collect();
    assert_eq!(
        ctors,
        [
            "pub const fn get_report_descriptor() -> Setup {",
            "pub const fn set_report_feature() -> Setup {",
            "pub const fn get_report_feature() -> Setup {",
        ]
    );
    let literals: Vec<&String> = lines.iter().filter(|l| l.as_str() == "Setup {").collect();
    assert_eq!(literals.len(), 3, "{literals:?}");
    // the phase-1 DFU trait has exactly one implementation in the library, and it is the simulated device
    for (name, src) in src_files() {
        for l in code(&src) {
            if name.starts_with("src/probe/") {
                assert!(
                    !tokens(&l).windows(2).any(|w| w == ["Transport", "for"]),
                    "{name}: {l}"
                );
            }
        }
    }
}

#[test]
fn fence_every_usbfs_request_goes_through_the_guard() {
    let lines: Vec<String> = code(&src_file("src/probe/usbfs.rs"))
        .into_iter()
        .map(|l| l.trim().to_string())
        .collect();
    let at = |needle: &str| {
        lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("usbfs.rs lost `{needle}`"))
    };
    // the guard checks the allow-list before it delegates, and delegates in exactly one place per method
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.contains("self.0.control("))
            .count(),
        1
    );
    assert!(at("if !permitted(setup, data)") < at("self.0.control(setup, data)"));
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.contains("self.0.claim_interface("))
            .count(),
        1
    );
    assert!(at("if interface != INTERFACE") < at("self.0.claim_interface(interface)"));
    // a bare `Usbfs` is built in one place, and handed out only inside the guard
    let built: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("Usbfs { file"))
        .collect();
    assert_eq!(built, ["Ok(UsbGuarded(Usbfs { file }))"]);
    assert!(lines
        .iter()
        .any(|l| l == "pub fn open(path: &Path) -> io::Result<UsbGuarded<Usbfs>> {"));
    // every call of the control channel in the high-level helpers takes a guard
    for l in lines.iter().filter(|l| l.starts_with("pub fn ")) {
        assert!(
            !l.contains("&Usbfs") && !l.contains("&T)"),
            "a helper takes an unguarded channel: {l}"
        );
    }
    // no other module holds a bare Usbfs
    for (name, src) in src_files() {
        if name == "src/probe/usbfs.rs" {
            continue;
        }
        for l in code(&src) {
            if tokens(&l).contains(&"Usbfs") {
                assert!(
                    l.contains("UsbGuarded<Usbfs>")
                        || l.contains("Usbfs::open(")
                        || l.trim_start().starts_with("use "),
                    "{name} holds an unguarded Usbfs: {l}"
                );
            }
        }
    }
}

#[test]
fn fence_sysfs_only_reads() {
    let src = src_file("src/probe/sysfs.rs");
    for banned in [
        "fs::write",
        "File::create",
        "OpenOptions",
        ".write(",
        "write_all",
        "libc::",
        "set_permissions",
        "remove_",
        "rename",
        "create_dir",
        "symlink(",
        "ioctl",
        "Hidraw",
        "Usbfs",
    ] {
        for l in code(&src) {
            assert!(!l.contains(banned), "sysfs.rs uses `{banned}`: {l}");
        }
    }
}
