//! The class `0x0A` "Flash" stream the vendor updater sends for the second 009F image (`Flash1`), decoded statically from
//! `WirelessSetFWU.exe` (`backgroundWorkerNordicProgram_DoWork`, `Common.CRC16`) and `FWUpdaterDLL.dll` (`SendCmd`); the full
//! write-up, with how each field was confirmed, is the `flash-format` document on DAN-1885.
//!
//! In short: the device is sent the image's Intel-HEX *text*, one record per frame, verbatim (`:` and all). There is no
//! per-record checksum field on the wire. Instead the host keeps one running CRC-16 over every record, each one
//! zero-padded to a 92-byte buffer, and sends it in a closing `0x3C` record. Only frame builders live here: nothing in
//! this module opens a device, and the real transport is not wired to it (phase 1 of DAN-1878).

use crate::protocol::{Report, ARGS_LEN};

/// `SendCommand(.., class 0x0A, id 0x00, ..)`: enter the flash-image mode (the app version then starts `"177."`).
pub const ENTER: (u8, u8) = (0x0A, 0x00);
/// Region query. The reply args are `[count, region id, type, _, size u32 BE]`; the vendor proceeds only if there is at
/// least one region and its type is 2 or 3.
pub const REGION_INFO: (u8, u8) = (0x0A, 0x80);
/// One record of the stream, and the closing CRC record.
pub const RECORD: (u8, u8) = (0x0A, 0x02);
/// Sent after the CRC record is accepted (class 0x0C id 0x00, size 0, up to ten tries): leave flash mode.
pub const FINISH: (u8, u8) = (0x0C, 0x00);

/// Bytes before the record text in a record frame's args: `[region id, 0, 0, 0, 0, 0, text length]`.
pub const RECORD_HEADER: usize = 7;
/// The vendor copies each line into a zeroed buffer of this size and feeds all of it to the CRC.
pub const CRC_BUFFER: usize = 92;
/// First byte of the closing record's text.
pub const CRC_MARKER: u8 = 0x3C;
/// `remaining_packets` of a data record (bytes 3 and 4 of the report), and of the closing CRC record.
pub const RECORD_PACKETS: u16 = 0xFFFF;
pub const CRC_PACKETS: u16 = 0x0000;
/// The CRC starts here.
pub const CRC_INIT: u16 = 0xFFFF;
/// Longest record text that fits the args after the 7-byte header.
pub const MAX_TEXT: usize = ARGS_LEN - RECORD_HEADER;

/// CRC-16/CCITT-FALSE (poly 0x1021, init 0xFFFF, no reflection, no final XOR; check value 0x29B1 for "123456789"),
/// continued from `crc`. The vendor writes it in the byte-swap form of AVR-libc's `_crc_ccitt_update`; the test module
/// checks this bitwise form against a literal port of that routine.
pub fn crc16_update(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Running CRC over the records of one stream, in the order they are sent.
pub fn stream_crc<'a>(lines: impl IntoIterator<Item = &'a str>) -> Result<u16, String> {
    let mut crc = CRC_INIT;
    for l in lines {
        let l = check_text(l)?;
        let mut buf = [0u8; CRC_BUFFER];
        buf[..l.len()].copy_from_slice(l.as_bytes());
        crc = crc16_update(crc, &buf);
    }
    Ok(crc)
}

fn check_text(line: &str) -> Result<&str, String> {
    let l = line.trim_matches(|c| c == '\r' || c == '\n');
    if !l.is_ascii() || l.len() > MAX_TEXT {
        return Err(format!(
            "flash record text must be ASCII and at most {MAX_TEXT} bytes: '{l}'"
        ));
    }
    Ok(l)
}

/// The frame carrying one Intel-HEX record line. Transaction id 0, as everywhere in the vendor updater.
pub fn record_frame(region: u8, line: &str) -> Result<Report, String> {
    let l = check_text(line)?;
    let mut args = vec![region, 0, 0, 0, 0, 0, l.len() as u8];
    args.extend_from_slice(l.as_bytes());
    let mut r = Report::new(RECORD, args.len() as u8, 0).with_args(&args);
    r.remaining_packets = RECORD_PACKETS;
    Ok(r)
}

/// The closing frame: a 3-byte text, `<` then the CRC big-endian.
pub fn crc_frame(region: u8, crc: u16) -> Report {
    let [hi, lo] = crc.to_be_bytes();
    let args = [region, 0, 0, 0, 0, 0, 3, CRC_MARKER, hi, lo];
    let mut r = Report::new(RECORD, args.len() as u8, 0).with_args(&args);
    r.remaining_packets = CRC_PACKETS;
    r
}

/// Every frame of the stream for `lines`, the closing CRC frame last.
pub fn stream_frames(region: u8, lines: &[&str]) -> Result<Vec<Report>, String> {
    let mut v = lines
        .iter()
        .map(|l| record_frame(region, l))
        .collect::<Result<Vec<_>, _>>()?;
    v.push(crc_frame(region, stream_crc(lines.iter().copied())?));
    Ok(v)
}
