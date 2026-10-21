//! `razerctl firmware probe-bootloader` (DAN-1908): golden frames, simulated runs of every path, the refusals, and the
//! fence that keeps the probe from ever erasing or writing. Every device here is simulated: nothing in this file opens
//! `/dev/hidraw*` or `/dev/input`, and the one test that runs the binary does so without `--yes-enter-bootloader`.

use razerctl::firmware::{dfu, flash};
use razerctl::hidraw::FeatureIo;
use razerctl::probe::cli;
use razerctl::probe::hw::{self, Guarded};
use razerctl::probe::report::{
    utc, BootloaderWait, Enumeration, OpenFailure, Outcome, ProbeLog, ReturnWait,
};
use razerctl::probe::{
    self, enter_frame, exit_frame, permitted, read_version_frame, Answer, Build, ProbeIo, Traffic,
    BOOTLOADER_ID, NORMAL_ID,
};
use razerctl::protocol::{Report, REPORT_LEN};
use std::cell::RefCell;
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
fn the_probe_frames_are_the_ones_phase_one_pinned() {
    assert_eq!(read_version_frame(), dfu::read_version_frame());
    assert_eq!(enter_frame(), dfu::enter_frame());
    assert_eq!(exit_frame(), dfu::exit_frame());
}

#[test]
fn only_the_three_probe_frames_are_permitted() {
    for f in [read_version_frame(), enter_frame(), exit_frame()] {
        assert!(permitted(&f.to_bytes()), "{f:?}");
    }
    let refused: Vec<(&str, Report)> = vec![
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
        (
            "exit with args",
            Report::new((0x10, 0x05), 1, 0).with_args(&[1]),
        ),
        ("read version 00:81", Report::new((0x00, 0x81), 2, 0)),
    ];
    for (what, f) in refused {
        assert!(!permitted(&f.to_bytes()), "{what} must not be permitted");
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The guarded channel, against a simulated feature-report device

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

#[test]
fn the_real_code_path_emits_exactly_the_golden_frames() {
    let g = Guarded(SimFeature::new());
    let v = hw::read_version_via(&g).unwrap();
    assert_eq!(&v.args[..3], &[1, 4, 0]);
    assert!(hw::enter_via(&g).unwrap().is_ok());
    assert!(hw::exit_via(&g).unwrap().is_ok());
    let sent = g.0.sent.borrow();
    assert_eq!(
        *sent,
        vec![
            golden(8, 0x00, 0x87, &[]),
            golden(2, 0x00, 0x04, &[1, 0]),
            golden(0, 0x10, 0x05, &[]),
        ]
    );
}

#[test]
fn the_guard_stops_every_other_frame_before_the_device() {
    let g = Guarded(SimFeature::new());
    let forbidden = [
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
// Simulated runs

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_792_000_000 + secs)
}

fn enumeration(pid: u16, node: &str, interface: u8, desc: &[u8], secs: u64) -> Enumeration {
    Enumeration {
        vid: 0x1532,
        pid,
        node: node.into(),
        interface: Some(interface),
        bcd_device: Some(0x009f),
        descriptor: desc.to_vec(),
        seen_at: at(secs),
    }
}

const DESC: [u8; 12] = [
    0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x95, 0x5A, 0xB1, 0x02, 0xC0,
];

/// A scripted device. Each call is appended to `calls`; the clock advances one second per call.
struct Sim {
    calls: Vec<&'static str>,
    clock: u64,
    version: Result<Report, String>,
    enter: Result<Answer, String>,
    bootloader: Option<Vec<Enumeration>>,
    open: Result<String, OpenFailure>,
    exit: Result<Answer, String>,
    back: Option<Duration>,
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
            bootloader: Some(vec![
                enumeration(0x110E, "/dev/hidraw7", 0, &DESC, 4),
                enumeration(0x110E, "/dev/hidraw8", 1, &[0x05, 0x01], 4),
            ]),
            open: Ok("/dev/hidraw7 (interface 0)".into()),
            exit: Ok(Ok(reply((0x10, 0x05), 2))),
            back: Some(Duration::from_millis(2500)),
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
    fn enter_bootloader(&mut self) -> Result<Answer, String> {
        self.tick("enter_bootloader");
        self.enter.clone()
    }
    fn wait_for_bootloader(&mut self, timeout: Duration) -> BootloaderWait {
        assert_eq!(timeout, probe::BOOTLOADER_TIMEOUT);
        self.tick("wait_for_bootloader");
        match self.bootloader.clone() {
            Some(seen) => BootloaderWait {
                seen,
                appeared_after: Some(Duration::from_millis(1800)),
            },
            None => BootloaderWait::default(),
        }
    }
    fn open_bootloader(&mut self) -> Result<String, OpenFailure> {
        self.tick("open_bootloader");
        self.open.clone()
    }
    fn exit_bootloader(&mut self) -> Result<Answer, String> {
        self.tick("exit_bootloader");
        self.exit.clone()
    }
    fn wait_for_return(&mut self, timeout: Duration) -> ReturnWait {
        assert_eq!(timeout, probe::RETURN_TIMEOUT);
        self.tick("wait_for_return");
        ReturnWait {
            after: self.back,
            nodes: vec!["1532:009f /dev/hidraw3 interface 0 bcdDevice 0200".into()],
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

#[test]
fn a_full_run_goes_through_the_five_steps_in_order() {
    let mut sim = Sim::happy();
    let (log, checkpoints) = run(&mut sim);
    assert_eq!(
        sim.calls,
        [
            "read_version",
            "enter_bootloader",
            "wait_for_bootloader",
            "open_bootloader",
            "exit_bootloader",
            "wait_for_return"
        ]
    );
    assert_eq!(log.outcome, Some(Outcome::Completed));
    assert!(!log.outcome.as_ref().unwrap().needs_replug());
    assert_eq!(log.version_read.as_deref(), Some("1.04.00"));
    assert_eq!(log.enumerations.len(), 2);
    assert_eq!(log.returned_after, Some(Duration::from_millis(2500)));
    // one checkpoint at the start, one after each of the five steps' first halves, and the final one
    assert!(checkpoints >= 7, "{checkpoints}");
}

#[test]
fn the_report_is_self_explanatory() {
    let mut sim = Sim::happy();
    let (log, _) = run(&mut sim);
    let text = log.render();
    for needle in [
        "razerctl 0.3.1 git 0123456789abcdef0123456789abcdef01234567",
        "started 2026-10-1", // 1_792_000_000 s
        "RESULT  COMPLETED",
        "1. read version (00:87)",
        "ok, version 1.04.00",
        "2. enter bootloader (00:04 mode 1)",
        "3. wait for 1532:110E",
        "1532:110E appeared 1.80 s after the wait began",
        "#1 1532:110e  hidraw node /dev/hidraw7  interface 0  bcdDevice 009f",
        "report descriptor (12 bytes): 06 00 ff 09 01 a1 01 95 5a b1 02 c0",
        "#2 1532:110e  hidraw node /dev/hidraw8  interface 1",
        "4. open the 1532:110E node",
        "succeeded as the current user, root was not needed",
        "5. exit bootloader (10:05)",
        "1532:009F came back by itself 2.50 s after 10:05 was sent",
        "1532:009F after 10:05: came back by itself after 2.50 s",
    ] {
        assert!(text.contains(needle), "missing `{needle}` in:\n{text}");
    }
    assert!(
        !text.contains("NEXT"),
        "no replug instruction after a clean run"
    );
    // every step carries a start and an end time
    assert!(text.matches("Z ->").count() >= 6, "{text}");
}

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
    assert!(text.contains("TIMED OUT: no 1532:110E within 20 s. Nothing further was sent"));
    assert!(
        text.contains("NEXT    Unplug the dongle and plug it back in"),
        "{text}"
    );
    assert!(
        text.contains("enumerations observed in step 3 (0)"),
        "{text}"
    );
}

#[test]
fn step_five_timeout_is_written_and_says_replug() {
    let mut sim = Sim::happy();
    sim.back = None;
    let (log, _) = run(&mut sim);
    assert_eq!(log.outcome, Some(Outcome::DidNotReturn));
    assert!(log.outcome.as_ref().unwrap().needs_replug());
    assert!(!log.outcome.as_ref().unwrap().is_success());
    let text = log.render();
    assert!(text.contains("STEP 5 TIMED OUT"), "{text}");
    assert!(text.contains("did NOT come back by itself"), "{text}");
    assert!(text.contains("did not come back within 30 s"), "{text}");
    assert!(text.contains("NEXT    Unplug the dongle"), "{text}");
    // the bootloader enumerations of step 3 are still in the file
    assert!(text.contains("hidraw node /dev/hidraw7"), "{text}");
}

#[test]
fn a_bootloader_node_that_needs_root_is_recorded_and_not_escalated() {
    let mut sim = Sim::happy();
    sim.open = Err(OpenFailure {
        node: "/dev/hidraw7".into(),
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
            "open_bootloader"
        ],
        "10:05 must not be attempted without an open node"
    );
    assert!(matches!(log.outcome, Some(Outcome::OpenFailed(_))));
    assert!(log.outcome.as_ref().unwrap().needs_replug());
    let text = log.render();
    assert!(text.contains("errno 13"), "{text}");
    assert!(
        text.contains("root (or a udev rule for 1532:110E) would be needed"),
        "{text}"
    );
    assert!(text.contains("Not escalated"), "{text}");
    assert!(text.contains("NEXT    Unplug the dongle"), "{text}");
    // the enumerations observed before the failed open are still reported
    assert!(text.contains("report descriptor (12 bytes)"), "{text}");
}

#[test]
fn a_failed_open_that_is_not_a_permission_error_says_so() {
    let mut sim = Sim::happy();
    sim.open = Err(OpenFailure {
        node: "/dev/hidraw7".into(),
        errno: Some(2),
        message: "No such file or directory (os error 2)".into(),
    });
    let (log, _) = run(&mut sim);
    assert!(log.render().contains("errno 2"));
    assert!(log.render().contains("this is not a permission error"));
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

#[test]
fn a_device_that_resets_without_a_reply_is_still_waited_on() {
    let mut sim = Sim::happy();
    sim.enter = Ok(Err("Input/output error (os error 5)".into()));
    let (log, _) = run(&mut sim);
    assert_eq!(log.outcome, Some(Outcome::Completed));
    assert!(log
        .render()
        .contains("sent; no reply read (Input/output error"));
}

#[test]
fn an_enter_frame_that_could_not_be_written_stops_the_run() {
    let mut sim = Sim::happy();
    sim.enter = Err("No such device".into());
    let (log, _) = run(&mut sim);
    assert_eq!(sim.calls, ["read_version", "enter_bootloader"]);
    assert!(matches!(log.outcome, Some(Outcome::EnterFailed(_))));
}

#[test]
fn an_exit_that_could_not_be_written_is_reported_with_the_replug_instruction() {
    let mut sim = Sim::happy();
    sim.exit = Err("Broken pipe (os error 32)".into());
    let (log, _) = run(&mut sim);
    assert_eq!(
        sim.calls.last(),
        Some(&"exit_bootloader"),
        "no wait after a failed send"
    );
    assert!(matches!(log.outcome, Some(Outcome::ExitFailed(_))));
    assert!(log.render().contains("NEXT    Unplug the dongle"));
}

#[test]
fn a_wait_that_saw_only_other_nodes_is_a_timeout_not_a_bootloader() {
    let mut sim = Sim::happy();
    sim.bootloader = Some(vec![enumeration(0x00AB, "/dev/hidraw9", 0, &DESC, 4)]);
    let (log, _) = run(&mut sim);
    assert_eq!(log.outcome, Some(Outcome::BootloaderNeverAppeared));
    assert!(!sim.calls.contains(&"open_bootloader"));
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
fn ids_are_the_ones_the_plan_names() {
    assert_eq!(NORMAL_ID, (0x1532, 0x009F));
    assert_eq!(BOOTLOADER_ID, (0x1532, 0x110E));
    assert_eq!(probe::BOOTLOADER_TIMEOUT, Duration::from_secs(20));
    assert_eq!(probe::RETURN_TIMEOUT, Duration::from_secs(30));
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
        "3. wait up to 20 s",
        "5. send exit (10:05)",
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
        tokens(l).contains(&"Hidraw") || l.contains("hidraw::") || l.contains("/dev/hidraw")
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
        checked, 4,
        "src/probe/ gained or lost a file: review the fence"
    );
}

#[test]
fn fence_only_three_frames_are_constructed_and_only_in_the_probe_root() {
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
            "src/probe/mod.rs: Report::new(EXIT, 0, TID)",
        ],
        "a probe module builds a frame other than the three"
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
            "io.send_feature(&frame.to_bytes())",
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
