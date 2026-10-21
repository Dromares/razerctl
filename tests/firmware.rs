//! `razerctl firmware` (DAN-1885): the updater reader, the Intel-HEX assembler, the class 0x0A stream and the DFU state
//! machine. Every fixture is generated in this file from arithmetic patterns: Razer's firmware is never committed, and no
//! test is built from it. Nothing here opens a device.

use razerctl::firmware::dfu::{self, BootloaderSeen, DfuError, Options, Transport, Version};
use razerctl::firmware::flash;
use razerctl::firmware::ihex;
use razerctl::firmware::resources;
use razerctl::firmware::zip::{self, Zip};
use razerctl::firmware::{inspect, Bootloader, FirmwareImage};
use razerctl::protocol::Report;

// ---------------------------------------------------------------------------------------------------------------------
// Fixture builders

fn v7(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn net_string(out: &mut Vec<u8>, s: &str) {
    v7(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

enum Val {
    Str(String),
    Int(i32),
    Bytes(Vec<u8>),
}

/// A .NET ResourceReader set, version 2, no type table.
fn resource_set(entries: &[(String, Val)]) -> Vec<u8> {
    let mut o = Vec::new();
    o.extend_from_slice(&0xBEEF_CACEu32.to_le_bytes());
    o.extend_from_slice(&1i32.to_le_bytes());
    let mut hdr = Vec::new();
    net_string(
        &mut hdr,
        "System.Resources.ResourceReader, mscorlib, Version=4.0.0.0, Culture=neutral",
    );
    net_string(&mut hdr, "System.Resources.RuntimeResourceSet");
    o.extend_from_slice(&(hdr.len() as i32).to_le_bytes());
    o.extend_from_slice(&hdr);
    o.extend_from_slice(&2i32.to_le_bytes());
    o.extend_from_slice(&(entries.len() as i32).to_le_bytes());
    o.extend_from_slice(&0i32.to_le_bytes());
    while o.len() % 8 != 0 {
        o.push(b'P');
    }
    for i in 0..entries.len() {
        o.extend_from_slice(&(0x1000 + i as i32).to_le_bytes()); // hashes: the reader does not look at them
    }
    let (mut names, mut data, mut pos) = (Vec::new(), Vec::new(), Vec::new());
    for (name, val) in entries {
        pos.push(names.len() as i32);
        let units: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        v7(&mut names, units.len() as u32);
        names.extend_from_slice(&units);
        names.extend_from_slice(&(data.len() as i32).to_le_bytes());
        match val {
            Val::Str(s) => {
                v7(&mut data, 1);
                net_string(&mut data, s);
            }
            Val::Int(i) => {
                v7(&mut data, 8);
                data.extend_from_slice(&i.to_le_bytes());
            }
            Val::Bytes(b) => {
                v7(&mut data, 32);
                v7(&mut data, b.len() as u32);
                data.extend_from_slice(b);
            }
        }
    }
    for p in &pos {
        o.extend_from_slice(&p.to_le_bytes());
    }
    let data_start = o.len() + 4 + names.len();
    o.extend_from_slice(&(data_start as i32).to_le_bytes());
    o.extend_from_slice(&names);
    o.extend_from_slice(&data);
    o
}

/// A ZIP archive; `deflate` members go through the same inflate crate the reader uses.
fn make_zip(members: &[(&str, &[u8], bool)]) -> Vec<u8> {
    let (mut body, mut cen) = (Vec::new(), Vec::new());
    for (name, data, deflate) in members {
        let packed = if *deflate {
            miniz_oxide::deflate::compress_to_vec(data, 6)
        } else {
            data.to_vec()
        };
        let method: u16 = if *deflate { 8 } else { 0 };
        let crc = zip::crc32(data);
        let local = body.len() as u32;
        body.extend_from_slice(b"PK\x03\x04");
        for v in [20u16, 0, method, 0, 0] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        for v in [crc, packed.len() as u32, data.len() as u32] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        body.extend_from_slice(&(name.len() as u16).to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(name.as_bytes());
        body.extend_from_slice(&packed);

        cen.extend_from_slice(b"PK\x01\x02");
        for v in [20u16, 20, 0, method, 0, 0] {
            cen.extend_from_slice(&v.to_le_bytes());
        }
        for v in [crc, packed.len() as u32, data.len() as u32] {
            cen.extend_from_slice(&v.to_le_bytes());
        }
        cen.extend_from_slice(&(name.len() as u16).to_le_bytes());
        for v in [0u16, 0, 0, 0] {
            cen.extend_from_slice(&v.to_le_bytes());
        }
        cen.extend_from_slice(&0u32.to_le_bytes());
        cen.extend_from_slice(&local.to_le_bytes());
        cen.extend_from_slice(name.as_bytes());
    }
    let mut z = body;
    let cen_off = z.len() as u32;
    z.extend_from_slice(&cen);
    z.extend_from_slice(b"PK\x05\x06");
    for v in [0u16, 0, members.len() as u16, members.len() as u16] {
        z.extend_from_slice(&v.to_le_bytes());
    }
    z.extend_from_slice(&(cen.len() as u32).to_le_bytes());
    z.extend_from_slice(&cen_off.to_le_bytes());
    z.extend_from_slice(&0u16.to_le_bytes());
    z
}

fn ihex_record(addr: u16, kind: u8, data: &[u8]) -> String {
    let mut b = vec![data.len() as u8, (addr >> 8) as u8, addr as u8, kind];
    b.extend_from_slice(data);
    let sum = b.iter().fold(0u8, |a, &x| a.wrapping_add(x));
    b.push(sum.wrapping_neg());
    format!(
        ":{}",
        b.iter().map(|x| format!("{x:02X}")).collect::<String>()
    )
}

/// Synthetic image bytes: a counting pattern that is neither constant nor anyone's firmware.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| {
            (i as u8)
                .wrapping_mul(7)
                .wrapping_add(seed)
                .wrapping_add((i >> 8) as u8)
        })
        .collect()
}

/// Intel-HEX text for `data` at `base`, 16 bytes a record, an extended-address record up front and an EOF record.
fn hex_lines(base: u32, data: &[u8]) -> Vec<String> {
    let mut v = vec![ihex_record(0, 4, &[(base >> 24) as u8, (base >> 16) as u8])];
    for (i, c) in data.chunks(16).enumerate() {
        v.push(ihex_record(
            (base as u16).wrapping_add((i * 16) as u16),
            0,
            c,
        ));
    }
    v.push(ihex_record(0, 1, &[]));
    v
}

fn s(x: &str) -> Val {
    Val::Str(x.to_string())
}

const DEV_BASE: u32 = 0x0800_0000;
const FLASH_BASE: u32 = 0x0900_0000;

/// A two-image updater: `Dev1` for 1532:00F1 (bootloader 1532:0BEE, bcd 00F1) and `Flash1` for the same device.
struct Synth {
    dev: Vec<u8>,
    flash: Vec<u8>,
    dev_lines: Vec<String>,
    flash_lines: Vec<String>,
}

impl Synth {
    fn new() -> Synth {
        let dev = pattern(300, 1);
        let flash = pattern(100, 9);
        Synth {
            dev_lines: hex_lines(DEV_BASE, &dev),
            flash_lines: hex_lines(FLASH_BASE, &flash),
            dev,
            flash,
        }
    }

    fn entries(&self) -> Vec<(String, Val)> {
        let mut e: Vec<(String, Val)> = vec![
            ("VID1".into(), s("1532")),
            ("PID1".into(), s("00F1")),
            ("BLVID1".into(), s("1532")),
            ("BLPID1".into(), s("0BEE")),
            ("BLVER1".into(), s("2.0")),
            ("BCDPID_BL1".into(), s("00F1")),
            ("DevFWVer1".into(), s("2.07.00")),
            ("FlashFWVer1".into(), s("3.01.02")),
            ("DevFWVerNum".into(), Val::Int(3)),
            (
                "Dev1FWLineNum".into(),
                Val::Int(self.dev_lines.len() as i32),
            ),
            (
                "Flash1FWLineNum".into(),
                Val::Int(self.flash_lines.len() as i32),
            ),
        ];
        for (i, l) in self.dev_lines.iter().enumerate() {
            e.push((format!("Dev1FWLine{i}"), Val::Str(l.clone())));
        }
        for (i, l) in self.flash_lines.iter().enumerate() {
            e.push((format!("Flash1FWLine{i}"), Val::Str(l.clone())));
        }
        e
    }

    /// A fake updater: junk, then the archive, so the reader has to find it by signature.
    fn exe_with(&self, entries: &[(String, Val)], prefix: usize) -> Vec<u8> {
        let res = resource_set(entries);
        let other = pattern(500, 3);
        let zip = make_zip(&[
            ("Tool.exe", &other, true),
            ("DeviceUpdater.resources", &res, true),
            ("de-DE\\Tool.resources.dll", &other[..40], false),
        ]);
        let mut exe: Vec<u8> = b"MZ"
            .iter()
            .copied()
            .chain(pattern(prefix - 2, 5))
            .collect();
        exe.extend_from_slice(&zip);
        exe
    }

    fn exe(&self) -> Vec<u8> {
        self.exe_with(&self.entries(), 4001)
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// ZIP

#[test]
fn crc32_matches_the_standard_check_value() {
    assert_eq!(zip::crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(zip::crc32(b""), 0);
}

#[test]
fn zip_is_found_by_signature_at_any_offset() {
    for prefix in [2usize, 129_597, 4001, 70_000] {
        let body = pattern(300, 2);
        let mut exe = vec![0x4D; prefix];
        let z = make_zip(&[("a.bin", &body, true), ("sub\\b.txt", b"hello", false)]);
        exe.extend_from_slice(&z);
        let found = Zip::find(&exe).unwrap();
        assert_eq!(found.offset, prefix, "archive start inside the file");
        assert_eq!(found.entries.len(), 2);
        let a = found.entry("a.bin").unwrap();
        assert_eq!(found.read(a).unwrap(), body, "deflated member");
        let b = found.entry("b.txt").unwrap();
        assert_eq!(
            found.read(b).unwrap(),
            b"hello",
            "stored member, found by file name under a path"
        );
    }
}

#[test]
fn truncated_zip_is_refused_naming_the_directory() {
    let exe = Synth::new().exe();
    for cut in [1usize, 10, 22, 200] {
        let err = Zip::find(&exe[..exe.len() - cut]).unwrap_err();
        assert!(
            err.contains("end-of-central-directory") || err.contains("central directory"),
            "cut {cut}: {err}"
        );
    }
    assert!(Zip::find(b"MZ").unwrap_err().contains("too short"));
}

#[test]
fn zip_member_crc_is_checked() {
    let payload = b"stored member payload, long enough to find";
    let mut z = make_zip(&[("s.bin", payload, false)]);
    let at = z.windows(payload.len()).position(|w| w == payload).unwrap();
    z[at + 3] ^= 0xFF;
    let zz = Zip::find(&z).unwrap();
    assert!(zz
        .read(&zz.entries[0])
        .unwrap_err()
        .contains("CRC-32 mismatch"));

    // a member whose data runs past the end of the file
    let mut z = make_zip(&[("s.bin", payload, false)]);
    let p = z.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
    z[p + 20..p + 24].copy_from_slice(&u32::MAX.to_le_bytes()); // compressed size
    let zz = Zip::find(&z).unwrap();
    assert!(zz
        .read(&zz.entries[0])
        .unwrap_err()
        .contains("past the end"));

    // directory size disagrees with the data
    let mut z = make_zip(&[("d.bin", &pattern(300, 1), true)]);
    let p = z.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
    z[p + 24..p + 28].copy_from_slice(&299u32.to_le_bytes()); // uncompressed size
    let zz = Zip::find(&z).unwrap();
    assert!(zz.read(&zz.entries[0]).is_err());
}

#[test]
fn zip_without_the_resource_member_is_refused_by_inspect() {
    let z = make_zip(&[("Other.dll", b"x", false)]);
    let mut exe = vec![b'M'; 100];
    exe.extend_from_slice(&z);
    let err = inspect(&exe).unwrap_err();
    assert!(err.contains("DeviceUpdater.resources"), "{err}");
}

#[test]
fn unsupported_zip_features_are_named() {
    let mut z = make_zip(&[("a", b"abc", false)]);
    // method 99 in the central directory entry
    let p = z.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
    z[p + 10] = 99;
    let zz = Zip::find(&z).unwrap();
    assert!(zz
        .read(&zz.entries[0])
        .unwrap_err()
        .contains("compression method 99"));
    // encrypted flag
    let mut z = make_zip(&[("a", b"abc", false)]);
    let p = z.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
    z[p + 8] = 1;
    assert!(Zip::find(&z).unwrap_err().contains("encrypted"));
}

// ---------------------------------------------------------------------------------------------------------------------
// .NET resource set

#[test]
fn resource_set_round_trips_strings_and_integers() {
    let set = resource_set(&[
        ("alpha".into(), s("one")),
        ("Ünïcode".into(), s("tw\u{f6}")),
        ("n".into(), Val::Int(-5)),
    ]);
    let m = resources::parse(&set).unwrap();
    assert_eq!(m["alpha"], "one");
    assert_eq!(m["Ünïcode"], "tw\u{f6}");
    assert_eq!(m["n"], "-5");
}

#[test]
fn resource_set_refusals_name_what_failed() {
    let good = resource_set(&[("a".into(), s("b"))]);
    let mut bad_magic = good.clone();
    bad_magic[0] ^= 1;
    assert!(resources::parse(&bad_magic)
        .unwrap_err()
        .contains("bad magic"));

    let mut bad_header = good.clone();
    bad_header[4] = 7;
    assert!(resources::parse(&bad_header)
        .unwrap_err()
        .contains("unknown resource layout: header version 7"));

    let mut other_reader = good.clone();
    other_reader[13] = b'X'; // inside "System.Resources.ResourceReader"
    assert!(resources::parse(&other_reader)
        .unwrap_err()
        .contains("unknown resource layout: reader"));

    for cut in [3usize, 20, good.len() - 3] {
        let err = resources::parse(&good[..cut]).unwrap_err();
        assert!(
            err.contains("truncated") || err.contains("claims"),
            "cut {cut}: {err}"
        );
    }

    let typed = resource_set(&[("blob".into(), Val::Bytes(vec![1, 2, 3]))]);
    let err = resources::parse(&typed).unwrap_err();
    assert!(
        err.contains("'blob'") && err.contains("type code 32"),
        "{err}"
    );

    let dup = resource_set(&[("a".into(), s("1")), ("a".into(), s("2"))]);
    assert!(resources::parse(&dup)
        .unwrap_err()
        .contains("two entries named 'a'"));
}

// ---------------------------------------------------------------------------------------------------------------------
// Intel-HEX

fn assemble(lines: &[String]) -> Result<ihex::Image, String> {
    ihex::assemble(lines.iter().enumerate().map(|(i, l)| {
        (
            Box::leak(format!("L{i}").into_boxed_str()) as &str,
            l.as_str(),
        )
    }))
}

#[test]
fn ihex_flattens_records_with_ff_in_the_gaps() {
    let data = pattern(40, 4);
    let mut lines = vec![ihex_record(0, 4, &[0x12, 0x34])];
    lines.push(ihex_record(0x0100, 0, &data[..16]));
    lines.push(ihex_record(0x0130, 0, &data[16..32])); // gap of 32 bytes
    lines.push(ihex_record(0, 5, &[0, 0, 0, 1])); // start address: ignored
    lines.push(ihex_record(0, 1, &[]));
    let img = assemble(&lines).unwrap();
    assert_eq!(img.start, 0x1234_0100);
    assert_eq!(img.end(), 0x1234_0140);
    assert_eq!(&img.data[..16], &data[..16]);
    assert!(img.data[16..48].iter().all(|&b| b == 0xFF));
    assert_eq!(&img.data[48..], &data[16..32]);
}

#[test]
fn ihex_refusals_name_the_line() {
    let ok = hex_lines(0x2000_0000, &pattern(48, 1));
    assert!(assemble(&ok).is_ok());

    let mut bad_sum = ok.clone();
    let last = bad_sum[2].len() - 1;
    let flip = if bad_sum[2].ends_with('0') { "1" } else { "0" };
    bad_sum[2].replace_range(last.., flip);
    let err = assemble(&bad_sum).unwrap_err();
    assert!(
        err.starts_with("L2:") && err.contains("bad checksum"),
        "{err}"
    );

    let mut no_eof = ok.clone();
    no_eof.pop();
    assert!(assemble(&no_eof)
        .unwrap_err()
        .contains("no end-of-file record"));

    let mut after_eof = ok.clone();
    after_eof.push(ihex_record(0, 0, &[1]));
    assert!(assemble(&after_eof)
        .unwrap_err()
        .contains("after the end-of-file"));

    let mut short = ok.clone();
    short[1] = short[1][..short[1].len() - 4].to_string();
    assert!(assemble(&short).unwrap_err().contains("L1:"));

    let mut nonhex = ok.clone();
    nonhex[1].replace_range(3..4, "G");
    assert!(assemble(&nonhex).unwrap_err().contains("non-hex"));

    let mut no_colon = ok.clone();
    no_colon[1].remove(0);
    assert!(assemble(&no_colon)
        .unwrap_err()
        .contains("does not start with ':'"));

    let overlap = vec![
        ihex_record(0x10, 0, &[1, 2, 3, 4]),
        ihex_record(0x12, 0, &[9]),
        ihex_record(0, 1, &[]),
    ];
    assert!(assemble(&overlap).unwrap_err().contains("overlaps"));

    let unknown = vec![ihex_record(0, 3, &[0, 0, 0, 0]), ihex_record(0, 1, &[])];
    assert!(assemble(&unknown).unwrap_err().contains("not supported"));

    assert!(assemble(&[ihex_record(0, 1, &[])])
        .unwrap_err()
        .contains("no data records"));

    let huge = vec![
        ihex_record(0, 0, &[1]),
        ihex_record(0, 4, &[0x40, 0]),
        ihex_record(0, 0, &[1]),
        ihex_record(0, 1, &[]),
    ];
    assert!(assemble(&huge).unwrap_err().contains("limit"));
}

// ---------------------------------------------------------------------------------------------------------------------
// inspect

#[test]
fn inspect_reports_every_image() {
    let sy = Synth::new();
    let ins = inspect(&sy.exe()).unwrap();
    assert_eq!(ins.zip_offset, 4001);
    assert_eq!(ins.zip_members, 3);
    assert_eq!(ins.images.len(), 2);

    let dev = ins.images.iter().find(|i| i.name == "Dev1").unwrap();
    assert_eq!((dev.vid, dev.pid), (0x1532, 0x00F1));
    assert_eq!(dev.version, "2.07.00");
    assert_eq!(dev.image.start, DEV_BASE);
    assert_eq!(dev.image.end(), DEV_BASE as u64 + 300);
    assert_eq!(dev.image.data, sy.dev);
    assert_eq!(dev.records, sy.dev_lines.len());
    assert_eq!(
        dev.sha256,
        razerctl::sha256::hex_digest(&sy.dev[..]).unwrap(),
        "sha256 is over the flattened image bytes"
    );
    assert_eq!(
        dev.bootloader,
        Some(Bootloader {
            vid: 0x1532,
            pid: 0x0BEE,
            version: "2.0".into(),
            bcd: 0x00F1
        })
    );

    let fl = ins.images.iter().find(|i| i.name == "Flash1").unwrap();
    assert_eq!(fl.version, "3.01.02");
    assert_eq!(fl.image.data, sy.flash);
    assert!(
        fl.bootloader.is_none(),
        "the Flash image does not go through the bootloader"
    );
    let lines: Vec<&str> = sy.flash_lines.iter().map(String::as_str).collect();
    assert_eq!(fl.flash_crc, Some(flash::stream_crc(lines).unwrap()));
}

fn replace(entries: &mut [(String, Val)], key: &str, v: Val) {
    entries.iter_mut().find(|(k, _)| k == key).unwrap().1 = v;
}

#[test]
fn inspect_refuses_a_bad_record_checksum_naming_the_line() {
    let sy = Synth::new();
    let mut e = sy.entries();
    let mut line = sy.dev_lines[5].clone();
    let n = line.len();
    line.replace_range(n - 2.., if line.ends_with("00") { "01" } else { "00" });
    replace(&mut e, "Dev1FWLine5", Val::Str(line));
    let err = inspect(&sy.exe_with(&e, 100)).unwrap_err();
    assert!(
        err.contains("Dev1") && err.contains("Dev1FWLine5") && err.contains("bad checksum"),
        "{err}"
    );
}

#[test]
fn inspect_refuses_unknown_layouts() {
    let sy = Synth::new();
    // no <image>FWLineNum at all
    let e: Vec<_> = sy
        .entries()
        .into_iter()
        .filter(|(k, _)| !k.ends_with("FWLineNum"))
        .collect();
    let err = inspect(&sy.exe_with(&e, 100)).unwrap_err();
    assert!(
        err.contains("unknown resource layout") && err.contains("FWLineNum"),
        "{err}"
    );
    // a line the count promises is missing
    let mut e = sy.entries();
    replace(
        &mut e,
        "Dev1FWLineNum",
        Val::Int(sy.dev_lines.len() as i32 + 1),
    );
    let err = inspect(&sy.exe_with(&e, 100)).unwrap_err();
    assert!(
        err.contains("Dev1FWLine") && err.contains("missing"),
        "{err}"
    );
    // a config entry the target needs is missing
    let e: Vec<_> = sy
        .entries()
        .into_iter()
        .filter(|(k, _)| k != "PID1")
        .collect();
    let err = inspect(&sy.exe_with(&e, 100)).unwrap_err();
    assert!(err.contains("'PID1'"), "{err}");
    let mut e = sy.entries();
    replace(&mut e, "PID1", s("zzzz"));
    assert!(inspect(&sy.exe_with(&e, 100))
        .unwrap_err()
        .contains("'PID1' = 'zzzz'"));
    // not a resource set
    let z = make_zip(&[(
        "DeviceUpdater.resources",
        b"not a resource set at all",
        false,
    )]);
    let mut exe = vec![0u8; 50];
    exe.extend_from_slice(&z);
    let err = inspect(&exe).unwrap_err();
    assert!(
        err.contains("DeviceUpdater.resources") && err.contains("bad magic"),
        "{err}"
    );
}

#[test]
fn inspect_refuses_a_flash_record_too_long_for_a_frame() {
    let sy = Synth::new();
    let mut e = sy.entries();
    replace(
        &mut e,
        "Flash1FWLine1",
        Val::Str(format!(":{}", "0".repeat(100))),
    );
    let err = inspect(&sy.exe_with(&e, 100)).unwrap_err();
    assert!(
        err.contains("Flash1") || err.contains("flash record"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------------------------------------------------
// class 0x0A stream

/// A literal port of the vendor's `Common.CRC16` (byte-swap form), kept apart from the implementation under test.
fn vendor_crc16(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc = (crc >> 8) | (crc << 8);
        crc ^= b as u16;
        crc ^= (crc & 0xFF) >> 4;
        crc ^= crc << 12;
        crc ^= (crc & 0xFF) << 5;
    }
    crc
}

#[test]
fn flash_crc_is_crc16_ccitt_false() {
    assert_eq!(flash::crc16_update(flash::CRC_INIT, b"123456789"), 0x29B1);
    assert_eq!(flash::crc16_update(flash::CRC_INIT, b""), 0xFFFF);
    let data = pattern(1000, 6);
    assert_eq!(
        flash::crc16_update(0xFFFF, &data),
        vendor_crc16(0xFFFF, &data)
    );
    assert_eq!(vendor_crc16(0xFFFF, b"123456789"), 0x29B1);
}

#[test]
fn flash_stream_crc_runs_across_zero_padded_records() {
    let lines = [":0300000001020309", ":00000001FF"];
    let mut expect = 0xFFFF;
    for l in lines {
        let mut buf = [0u8; 92];
        buf[..l.len()].copy_from_slice(l.as_bytes());
        expect = vendor_crc16(expect, &buf);
    }
    assert_eq!(flash::stream_crc(lines).unwrap(), expect);
    // one running value, not one per record: the second record alone gives something else
    assert_ne!(
        flash::stream_crc(lines).unwrap(),
        flash::stream_crc([lines[1]]).unwrap()
    );
    // and the padding is part of it
    assert_ne!(
        flash::stream_crc([lines[0]]).unwrap(),
        vendor_crc16(0xFFFF, lines[0].as_bytes())
    );
    // trailing CR/LF is not part of a record
    assert_eq!(
        flash::stream_crc([":00000001FF\r\n"]).unwrap(),
        flash::stream_crc([":00000001FF"]).unwrap()
    );
}

fn frame91(r: &Report) -> [u8; 91] {
    let mut b = [0u8; 91];
    b[1..].copy_from_slice(&r.to_bytes());
    b
}

#[test]
fn flash_record_frame_layout_is_pinned() {
    let f = flash::record_frame(0x09, ":00000001FF").unwrap();
    let b = frame91(&f);
    assert_eq!(b[0], 0, "report id");
    assert_eq!(b[1], 0, "status");
    assert_eq!(b[2], 0, "transaction id is 0");
    assert_eq!(&b[3..5], &[0xFF, 0xFF], "packet counter of a data record");
    assert_eq!(b[5], 0);
    assert_eq!(
        b[6],
        11 + 7,
        "size byte is the text length plus the 7-byte header"
    );
    assert_eq!((b[7], b[8]), (0x0A, 0x02));
    assert_eq!(
        &b[9..16],
        &[0x09, 0, 0, 0, 0, 0, 11],
        "[region, 0 x5, text length]"
    );
    assert_eq!(&b[16..27], b":00000001FF");
    assert!(b[27..89].iter().all(|&x| x == 0));
    assert_eq!(
        b[89],
        b[3..89].iter().fold(0, |a, x| a ^ x),
        "XOR of bytes 3..=88"
    );
}

#[test]
fn flash_crc_frame_layout_is_pinned() {
    let b = frame91(&flash::crc_frame(0x09, 0xBEEF));
    assert_eq!(&b[3..5], &[0, 0], "packet counter of the closing record");
    assert_eq!(b[6], 10);
    assert_eq!((b[7], b[8]), (0x0A, 0x02));
    assert_eq!(
        &b[9..19],
        &[0x09, 0, 0, 0, 0, 0, 3, 0x3C, 0xBE, 0xEF],
        "'<' then the CRC, big-endian"
    );
    assert_eq!(b[2], 0);
}

#[test]
fn flash_stream_ends_with_the_crc_of_everything_before_it() {
    let lines = [":0300000001020309", ":00000001FF"];
    let frames = flash::stream_frames(7, &lines).unwrap();
    assert_eq!(frames.len(), 3);
    let crc = flash::stream_crc(lines).unwrap().to_be_bytes();
    assert_eq!(&frames[2].args[7..10], &[0x3C, crc[0], crc[1]]);
    assert!(flash::record_frame(1, &"A".repeat(flash::MAX_TEXT + 1)).is_err());
    assert!(flash::record_frame(1, &"A".repeat(flash::MAX_TEXT)).is_ok());
    assert!(flash::record_frame(1, "é").is_err());
}

// ---------------------------------------------------------------------------------------------------------------------
// DFU state machine, against a recording mock

#[derive(Clone, Copy, PartialEq, Debug)]
enum Quirk {
    None,
    /// Read back this address with one flipped bit, every time.
    CorruptAt(u32),
    /// Read back this address wrong on the first try only.
    CorruptOnce(u32),
    /// Answer the first N Program frames with "failed".
    RejectPrograms(usize),
    /// Reply with a different transaction id.
    WrongTid,
}

struct Mock {
    id: (u16, u16),
    version: (u8, u8, u8),
    seen: BootloaderSeen,
    quirk: Quirk,
    log: Vec<Report>,
    mem: std::collections::BTreeMap<u32, u8>,
    rejected: usize,
    corrupted: bool,
    waits: usize,
}

impl Mock {
    fn new() -> Mock {
        Mock {
            id: (0x1532, 0x00F1),
            version: (2, 6, 0),
            seen: BootloaderSeen {
                vid: 0x1532,
                pid: 0x0BEE,
                bcd: 0x00F1,
            },
            quirk: Quirk::None,
            log: vec![],
            mem: Default::default(),
            rejected: 0,
            corrupted: false,
            waits: 0,
        }
    }
    fn ids(&self) -> Vec<(u8, u8)> {
        self.log
            .iter()
            .map(|f| (f.command_class, f.command_id))
            .collect()
    }
    fn count(&self, c: (u8, u8)) -> usize {
        self.ids().iter().filter(|&&x| x == c).count()
    }
}

impl Transport for Mock {
    fn device_id(&self) -> (u16, u16) {
        self.id
    }
    fn exchange(&mut self, f: &Report) -> Result<Report, String> {
        self.log.push(f.clone());
        let mut r = f.clone();
        r.status = 2;
        match (f.command_class, f.command_id) {
            dfu::READ_VERSION => {
                r.args[..3].copy_from_slice(&[self.version.0, self.version.1, self.version.2])
            }
            dfu::PROGRAM => {
                if let Quirk::RejectPrograms(n) = self.quirk {
                    if self.rejected < n {
                        self.rejected += 1;
                        r.status = 3;
                        return Ok(r);
                    }
                }
                let a = u32::from_be_bytes(f.args[1..5].try_into().unwrap());
                for i in 0..dfu::PACKET {
                    self.mem.insert(a + i as u32, f.args[5 + i]);
                }
            }
            dfu::VERIFY => {
                let a = u32::from_be_bytes(f.args[1..5].try_into().unwrap());
                for i in 0..dfu::PACKET {
                    r.args[5 + i] = self.mem[&(a + i as u32)];
                }
                match self.quirk {
                    Quirk::CorruptAt(x) if x == a => r.args[5] ^= 1,
                    Quirk::CorruptOnce(x) if x == a && !self.corrupted => {
                        self.corrupted = true;
                        r.args[5] ^= 1;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        if self.quirk == Quirk::WrongTid {
            r.transaction_id = 0x1f;
        }
        Ok(r)
    }
    fn wait_for_bootloader(&mut self, _: &Bootloader) -> Result<BootloaderSeen, String> {
        self.waits += 1;
        self.log.push(Report::new((0xEE, 0xEE), 0, 0)); // marker: where the wait happened
        Ok(self.seen.clone())
    }
}

fn fw_image(len: usize) -> FirmwareImage {
    let data = pattern(len, 11);
    FirmwareImage {
        name: "Dev1".into(),
        vid: 0x1532,
        pid: 0x00F1,
        version: "2.07.00".into(),
        sha256: String::new(),
        records: 0,
        lines: vec![],
        flash_crc: None,
        image: ihex::Image {
            start: DEV_BASE,
            data,
        },
        bootloader: Some(Bootloader {
            vid: 0x1532,
            pid: 0x0BEE,
            version: "2.0".into(),
            bcd: 0x00F1,
        }),
    }
}

fn run(m: &mut Mock, img: &FirmwareImage) -> Result<dfu::Outcome, DfuError> {
    dfu::flash(m, img, &Options::default())
}

#[test]
fn dfu_sequence_golden_frames() {
    let img = fw_image(130); // two full packets and a padded third
    let mut m = Mock::new();
    let out = run(&mut m, &img).unwrap();
    assert_eq!(
        out,
        dfu::Outcome {
            device_version: Version(2, 6, 0),
            programmed: 3,
            verified: 3
        }
    );
    let mut want = vec![(0x00, 0x87), (0x00, 0x04), (0xEE, 0xEE), (0x10, 0x01)];
    want.extend([(0x10, 0x02); 3]);
    want.extend([(0x10, 0x83); 3]);
    want.push((0x10, 0x05));
    assert_eq!(
        m.ids(),
        want,
        "version, enter, wait, erase, program x3, verify x3, exit"
    );

    // every frame uses transaction id 0
    assert!(m.log.iter().all(|f| f.transaction_id == 0));

    let hex = |r: &Report, n: usize| -> String {
        frame91(r)[..n].iter().map(|b| format!("{b:02x}")).collect()
    };
    // report id, status, tid, two packet bytes, protocol type: all 0; then size, class, id
    let head = |size: u8, c: u8, i: u8| format!("000000000000{size:02x}{c:02x}{i:02x}");
    // 00:87, size 8
    assert_eq!(hex(&m.log[0], 9), head(8, 0x00, 0x87));
    // 00:04, size 2, args [1, 0]
    assert_eq!(hex(&m.log[1], 11), head(2, 0x00, 0x04) + "0100");
    // 10:01 erase, size 8, [start BE][end BE]: 0x08000000 .. 0x08000082
    assert_eq!(hex(&m.log[3], 17), head(8, 0x10, 0x01) + "0800000008000082");
    // 10:02 program, size byte 8 (not 69), [len 64][addr BE][data]
    let p0 = &m.log[4];
    assert_eq!(p0.data_size, 8);
    assert_eq!(hex(p0, 14), head(8, 0x10, 0x02) + "4008000000");
    assert_eq!(&frame91(p0)[14..14 + 64], &img.image.data[..64]);
    let p2 = &m.log[6];
    assert_eq!(p2.data_size, 8);
    assert_eq!(
        &p2.args[..5],
        &[0x40, 0x08, 0x00, 0x00, 0x80],
        "big-endian address of the third packet"
    );
    assert_eq!(&p2.args[5..7], &img.image.data[128..130]);
    assert!(
        p2.args[7..5 + 64].iter().all(|&b| b == 0xFF),
        "short last packet is padded with 0xFF"
    );
    // 10:83 read back, size 5, [len][addr BE]
    assert_eq!(hex(&m.log[7], 14), head(5, 0x10, 0x83) + "4008000000");
    // 10:05 exit, size 0, last
    assert_eq!(hex(m.log.last().unwrap(), 9), head(0, 0x10, 0x05));
    // XOR checksum of a frame is in byte 89
    let b = frame91(p0);
    assert_eq!(b[89], b[3..89].iter().fold(0, |a, x| a ^ x));
}

#[test]
fn dfu_never_exits_after_a_verify_mismatch() {
    let img = fw_image(200);
    let mut m = Mock::new();
    m.quirk = Quirk::CorruptAt(DEV_BASE + 64);
    let err = run(&mut m, &img).unwrap_err();
    assert_eq!(
        err,
        DfuError::VerifyMismatch {
            address: DEV_BASE + 64
        }
    );
    assert_eq!(
        m.count(dfu::EXIT),
        0,
        "10:05 must never be sent after a mismatch"
    );
    assert_eq!(
        m.count(dfu::VERIFY),
        1 + 10,
        "block 0 once, then the bad block ten times"
    );
    assert!(err.to_string().contains("NOT told to exit"));
}

#[test]
fn dfu_verify_retries_a_transient_misread() {
    let img = fw_image(200);
    let mut m = Mock::new();
    m.quirk = Quirk::CorruptOnce(DEV_BASE);
    let out = run(&mut m, &img).unwrap();
    assert_eq!(out.verified, 4);
    assert_eq!(m.count(dfu::VERIFY), 5, "one extra read of block 0");
    assert_eq!(m.count(dfu::EXIT), 1);
}

#[test]
fn dfu_verifies_every_block_before_exit() {
    let img = fw_image(64 * 5);
    let mut m = Mock::new();
    run(&mut m, &img).unwrap();
    let ids = m.ids();
    let last_verify = ids.iter().rposition(|&x| x == dfu::VERIFY).unwrap();
    let exit = ids.iter().position(|&x| x == dfu::EXIT).unwrap();
    let last_program = ids.iter().rposition(|&x| x == dfu::PROGRAM).unwrap();
    assert!(last_program < ids.iter().position(|&x| x == dfu::VERIFY).unwrap());
    assert_eq!(exit, last_verify + 1);
    assert_eq!(m.count(dfu::VERIFY), 5);
}

#[test]
fn dfu_checks_the_bootloader_identity_before_erasing() {
    for seen in [
        BootloaderSeen {
            bcd: 0x00F2,
            ..Mock::new().seen
        },
        BootloaderSeen {
            pid: 0x0BEF,
            ..Mock::new().seen
        },
        BootloaderSeen {
            vid: 0x1533,
            ..Mock::new().seen
        },
    ] {
        let mut m = Mock::new();
        m.seen = seen;
        let err = run(&mut m, &fw_image(100)).unwrap_err();
        assert!(
            matches!(err, DfuError::Refused(ref s) if s.contains("nothing was erased")),
            "{err}"
        );
        assert_eq!(m.count(dfu::ERASE), 0);
        assert_eq!(m.count(dfu::PROGRAM), 0);
        assert_eq!(m.count(dfu::EXIT), 0);
    }
}

#[test]
fn dfu_refuses_the_wrong_device_without_sending_anything() {
    let mut m = Mock::new();
    m.id = (0x1532, 0x00F2);
    let err = run(&mut m, &fw_image(100)).unwrap_err();
    assert!(
        matches!(err, DfuError::Refused(ref s) if s.contains("00f1") && s.contains("00f2")),
        "{err}"
    );
    assert!(m.log.is_empty(), "not one frame");
}

#[test]
fn dfu_refuses_an_image_that_is_not_a_bootloader_image() {
    let mut img = fw_image(100);
    img.bootloader = None;
    let mut m = Mock::new();
    assert!(matches!(run(&mut m, &img), Err(DfuError::Refused(_))));
    assert!(m.log.is_empty());
    let mut img = fw_image(100);
    img.image.data.clear();
    assert!(matches!(run(&mut m, &img), Err(DfuError::Refused(_))));
    let mut img = fw_image(100);
    img.version = "two".into();
    assert!(matches!(run(&mut m, &img), Err(DfuError::Refused(_))));
    assert!(m.log.is_empty());
}

#[test]
fn dfu_version_gate_needs_an_explicit_override() {
    for have in [(2, 7, 0), (2, 8, 1), (3, 0, 0)] {
        let mut m = Mock::new();
        m.version = have;
        let err = run(&mut m, &fw_image(100)).unwrap_err();
        assert!(
            matches!(err, DfuError::Refused(ref s) if s.contains("--allow-reflash")),
            "{have:?}: {err}"
        );
        assert_eq!(
            m.ids(),
            vec![dfu::READ_VERSION],
            "only the version read went out"
        );
        let mut m = Mock::new();
        m.version = have;
        dfu::flash(
            &mut m,
            &fw_image(100),
            &Options {
                allow_reflash: true,
            },
        )
        .unwrap();
        assert_eq!(m.count(dfu::EXIT), 1);
    }
    assert!(Version(2, 6, 9) < Version(2, 7, 0));
    assert_eq!(Version::parse("1.05.00"), Some(Version(1, 5, 0)));
    assert_eq!(Version::parse("1.05"), None);
    assert_eq!(Version(1, 5, 0).to_string(), "1.05.00");
}

#[test]
fn dfu_program_is_retried_once_then_gives_up_without_verifying_or_exiting() {
    let mut m = Mock::new();
    m.quirk = Quirk::RejectPrograms(1);
    run(&mut m, &fw_image(64)).unwrap();
    assert_eq!(m.count(dfu::PROGRAM), 2);

    let mut m = Mock::new();
    m.quirk = Quirk::RejectPrograms(2);
    let err = run(&mut m, &fw_image(64)).unwrap_err();
    assert!(
        matches!(
            err,
            DfuError::Rejected {
                step: "program",
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(m.count(dfu::PROGRAM), 2);
    assert_eq!(m.count(dfu::VERIFY), 0);
    assert_eq!(m.count(dfu::EXIT), 0);
}

#[test]
fn dfu_rejects_a_reply_for_another_transaction() {
    let mut m = Mock::new();
    m.quirk = Quirk::WrongTid;
    let err = run(&mut m, &fw_image(64)).unwrap_err();
    assert!(
        matches!(
            err,
            DfuError::Rejected {
                step: "read version",
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(m.log.len(), 1);
}

#[test]
fn dfu_waits_for_the_bootloader_between_enter_and_erase() {
    let mut m = Mock::new();
    run(&mut m, &fw_image(64)).unwrap();
    assert_eq!(m.waits, 1);
    let ids = m.ids();
    let enter = ids.iter().position(|&x| x == dfu::ENTER_MODE).unwrap();
    assert_eq!(ids[enter + 1], (0xEE, 0xEE));
    assert_eq!(ids[enter + 2], dfu::ERASE);
}

// ---------------------------------------------------------------------------------------------------------------------
// The binary

fn bin() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_razerctl"))
}

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("razerctl-fw-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn cli_inspect_prints_one_block_per_image_and_refuses_damage() {
    let sy = Synth::new();
    let dir = tmpdir("inspect");
    let exe = dir.join("u.exe");
    std::fs::write(&exe, sy.exe()).unwrap();
    let out = bin()
        .args(["firmware", "inspect"])
        .arg(&exe)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains("ZIP at offset 4001"), "{text}");
    assert!(text.contains("image Dev1"), "{text}");
    assert!(
        text.contains("target 1532:00f1  version 2.07.00  0x08000000-0x0800012c  300 bytes"),
        "{text}"
    );
    assert!(
        text.contains(&razerctl::sha256::hex_digest(&sy.dev[..]).unwrap()),
        "{text}"
    );
    assert!(
        text.contains("bootloader 1532:0bee (bcdDevice 00f1"),
        "{text}"
    );
    assert!(
        text.contains("image Flash1") && text.contains("stream CRC-16"),
        "{text}"
    );

    // truncated file: clean refusal, non-zero exit, no panic
    let bytes = sy.exe();
    let cut = dir.join("cut.exe");
    std::fs::write(&cut, &bytes[..bytes.len() - 30]).unwrap();
    let out = bin()
        .args(["firmware", "inspect"])
        .arg(&cut)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        err.contains("cut.exe") && err.contains("end-of-central-directory"),
        "{err}"
    );
    assert!(!err.contains("panicked"));

    let missing = bin()
        .args(["firmware", "inspect", "/nonexistent/u.exe"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("/nonexistent/u.exe"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn cli_flash_without_dry_run_is_refused_before_the_file_is_read() {
    let out = bin()
        .args(["firmware", "flash", "/nonexistent/u.exe", "--image", "Dev1"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("flashing is not supported yet"), "{err}");
    assert!(
        !err.contains("/nonexistent"),
        "it must refuse before touching the path: {err}"
    );
    assert!(out.stdout.is_empty());
}

#[test]
fn cli_flash_dry_run_prints_every_frame_and_sends_nothing() {
    let sy = Synth::new();
    let dir = tmpdir("dry");
    let exe = dir.join("u.exe");
    std::fs::write(&exe, sy.exe()).unwrap();
    let out = bin()
        .args(["firmware", "flash"])
        .arg(&exe)
        .args(["--image", "Dev1", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let lines: Vec<&str> = text.lines().filter(|l| l.starts_with('#')).collect();
    // 300 bytes = 5 packets: version, enter, erase, 5 program, 5 verify, exit
    assert_eq!(lines.len(), 1 + 1 + 1 + 5 + 5 + 1, "{text}");
    assert!(lines[0].contains("00:87 size 8 tid 0"));
    assert!(
        lines[2].contains("10:01 size 8 tid 0 args [08 00 00 00 08 00 01 2c]"),
        "{}",
        lines[2]
    );
    assert!(
        lines[3].contains("10:02 size 8 tid 0 args [40 08 00 00 00 "),
        "{}",
        lines[3]
    );
    assert!(lines.last().unwrap().contains("10:05 size 0"));
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        err.contains("WOULD be sent") && err.contains("dry run complete: 14 frames"),
        "{err}"
    );

    // the Flash image is not bootloader-flashable; an already-current device needs the override; unknown names list the choices
    for (extra, want) in [
        (
            &["--image", "Flash1"][..],
            "not written through the bootloader",
        ),
        (
            &["--image", "Dev1", "--device-version", "2.07.00"],
            "--allow-reflash",
        ),
        (&["--image", "Nope"], "no image named 'Nope' (it has"),
        (&["--image", "Dev1", "--device-version", "x"], "not X.YY.ZZ"),
    ] {
        let out = bin()
            .args(["firmware", "flash"])
            .arg(&exe)
            .args(extra)
            .arg("--dry-run")
            .output()
            .unwrap();
        assert!(!out.status.success(), "{extra:?}");
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(err.contains(want), "{extra:?}: {err}");
    }
    let ok = bin()
        .args(["firmware", "flash"])
        .arg(&exe)
        .args([
            "--image",
            "Dev1",
            "--dry-run",
            "--device-version",
            "2.07.00",
            "--allow-reflash",
        ])
        .output()
        .unwrap();
    assert!(ok.status.success());
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------------------------------------------------
// Phase 1 fence: nothing in the firmware module can reach a device.

fn firmware_sources() -> Vec<(String, String)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/firmware");
    let mut v = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "rs") {
            v.push((
                p.file_name().unwrap().to_string_lossy().into(),
                std::fs::read_to_string(&p).unwrap(),
            ));
        }
    }
    v
}

/// Source lines that are code or doc-less statements: everything except `//` comment lines.
fn code_lines(src: &str) -> impl Iterator<Item = &str> {
    src.lines().filter(|l| !l.trim_start().starts_with("//"))
}

#[test]
fn firmware_module_cannot_reach_a_device() {
    let forbidden = [
        "hidraw",
        "/dev/",
        "evdev",
        "libc::",
        "ioctl",
        "OpenOptions",
        "File::open",
        "File::create",
        "std::net",
        "Command::new",
    ];
    for (name, src) in firmware_sources() {
        for l in code_lines(&src) {
            for f in forbidden {
                // `std::fs::read(path)` for the updater file is the one file access the module may make (mod.rs)
                assert!(!l.contains(f), "src/firmware/{name} mentions `{f}`: {l}");
            }
        }
    }
}

#[test]
fn only_the_simulated_device_implements_the_dfu_transport() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut stack = vec![src_dir];
    let mut impls = Vec::new();
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let text = std::fs::read_to_string(&p).unwrap();
                for l in code_lines(&text) {
                    if l.contains("Transport for") {
                        impls.push(format!(
                            "{}: {}",
                            p.strip_prefix(env!("CARGO_MANIFEST_DIR"))
                                .unwrap()
                                .display(),
                            l.trim()
                        ));
                    }
                }
            }
        }
    }
    assert_eq!(impls.len(), 1, "{impls:#?}");
    assert!(impls[0].starts_with("src/firmware/sim.rs"), "{impls:#?}");
    // and the hardware layer does not know the module exists
    for f in ["hidraw.rs", "device.rs", "protocol.rs", "onboard.rs"] {
        let text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src")
                .join(f),
        )
        .unwrap();
        assert!(
            !text.contains("firmware::"),
            "src/{f} must not use the firmware module"
        );
    }
}
