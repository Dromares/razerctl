//! Razer "Protocol25" feature-report framing.
//!
//! Reconstructed from the shipped Synapse 4 (RazerAppEngine 4.4.25) app.asar:
//!   * electron/Protocol/protocol25Const.js  – status + command enums
//!   * electron/Protocol/protocol25.js       – getCommandSendStatus(): a 90-byte
//!     response has status at [0] and the echoed command class/id at [6],[7]; a
//!     91-byte one (report-ID prefixed) shifts everything by one.
//!   * electron/UsbRzDeviceAction.js         – "hid.sendFeatureReport" prepends
//!     report-ID 0 on non-Windows hosts to make a 91-byte HID feature report.
//! Field layout and the XOR checksum match OpenRazer's `struct razer_report`.

pub const REPORT_LEN: usize = 90;
pub const ARGS_LEN: usize = 80;

/// enumProtocol25Status from protocol25Const.js
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    NewCommand,
    Busy,
    Success,
    Failed,
    Timeout,
    NotSupported,
    /// Response echoed a different class/id than was sent (JS returns notSupported).
    Mismatch,
}

impl Status {
    fn from_byte(b: u8) -> Status {
        match b {
            0 => Status::NewCommand,
            1 => Status::Busy,
            2 => Status::Success,
            4 => Status::Timeout,
            5 => Status::NotSupported,
            _ => Status::Failed, // 3 and anything unknown -> failed, same as JS default
        }
    }
}

/// Command (class, id) pairs named in enumProtocol25Command plus the ones the
/// Synapse web UI sends for mice (cross-checked against OpenRazer).
#[allow(dead_code)]
pub mod cmd {
    pub const GET_FIRMWARE_VERSION: (u8, u8) = (0x00, 0x81);
    pub const GET_SERIAL: (u8, u8) = (0x00, 0x82);
    pub const GET_DEVICE_MODE: (u8, u8) = (0x00, 0x84);
    pub const GET_EDITION: (u8, u8) = (0x00, 0x86);
    pub const SET_DEVICE_MODE: (u8, u8) = (0x00, 0x04);
    pub const GET_POLL_RATE2: (u8, u8) = (0x00, 0xC0);
    pub const SET_POLL_RATE2: (u8, u8) = (0x00, 0x40);
    pub const SET_LED_MATRIX_RGB: (u8, u8) = (0x03, 0x0A);
    pub const GET_DPI_XY: (u8, u8) = (0x04, 0x85);
    pub const SET_DPI_XY: (u8, u8) = (0x04, 0x05);
    pub const GET_DPI_STAGES: (u8, u8) = (0x04, 0x86);
    pub const GET_BATTERY: (u8, u8) = (0x07, 0x80);
    pub const GET_CHARGING: (u8, u8) = (0x07, 0x84);
    pub const SET_CHROMA_EFFECT: (u8, u8) = (0x0F, 0x02);
}

#[derive(Clone, PartialEq, Eq)]
pub struct Report {
    pub status: u8,
    pub transaction_id: u8,
    pub remaining_packets: u16,
    pub protocol_type: u8,
    pub data_size: u8,
    pub command_class: u8,
    pub command_id: u8,
    pub args: [u8; ARGS_LEN],
}

impl std::fmt::Debug for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = (self.data_size as usize).min(ARGS_LEN);
        write!(
            f,
            "Report{{status:{:#04x} tid:{:#04x} cmd:{:02x}:{:02x} size:{} args:{:02x?}}}",
            self.status,
            self.transaction_id,
            self.command_class,
            self.command_id,
            self.data_size,
            &self.args[..n]
        )
    }
}

impl Report {
    pub fn new(cmd: (u8, u8), data_size: u8, transaction_id: u8) -> Report {
        Report {
            status: 0,
            transaction_id,
            remaining_packets: 0,
            protocol_type: 0,
            data_size,
            command_class: cmd.0,
            command_id: cmd.1,
            args: [0; ARGS_LEN],
        }
    }

    pub fn with_args(mut self, args: &[u8]) -> Report {
        self.args[..args.len()].copy_from_slice(args);
        self
    }

    /// XOR of bytes 2..88 (everything after status/transaction id, before crc).
    pub fn crc(bytes: &[u8; REPORT_LEN]) -> u8 {
        bytes[2..88].iter().fold(0, |a, b| a ^ b)
    }

    pub fn to_bytes(&self) -> [u8; REPORT_LEN] {
        let mut b = [0u8; REPORT_LEN];
        b[0] = self.status;
        b[1] = self.transaction_id;
        b[2..4].copy_from_slice(&self.remaining_packets.to_be_bytes());
        b[4] = self.protocol_type;
        b[5] = self.data_size;
        b[6] = self.command_class;
        b[7] = self.command_id;
        b[8..88].copy_from_slice(&self.args);
        b[88] = Self::crc(&b);
        b
    }

    /// Accepts the 90-byte payload, or a 91-byte buffer with a leading report ID,
    /// mirroring protocol25.js' length switch.
    pub fn parse(buf: &[u8]) -> Result<Report, String> {
        let b = match buf.len() {
            REPORT_LEN => buf,
            91 => &buf[1..],
            n => return Err(format!("unexpected report length {n}")),
        };
        let mut args = [0u8; ARGS_LEN];
        args.copy_from_slice(&b[8..88]);
        Ok(Report {
            status: b[0],
            transaction_id: b[1],
            remaining_packets: u16::from_be_bytes([b[2], b[3]]),
            protocol_type: b[4],
            data_size: b[5],
            command_class: b[6],
            command_id: b[7],
            args,
        })
    }

    /// Port of Protocol25.getCommandSendStatus(data, command).
    pub fn status_for(&self, sent: (u8, u8)) -> Status {
        if (self.command_class, self.command_id) != sent {
            return Status::Mismatch;
        }
        Status::from_byte(self.status)
    }
}

/// Polling-rate byte encoding used by command 00:40 / 00:C0.
pub fn poll_rate_to_byte(hz: u32) -> Option<u8> {
    Some(match hz {
        8000 => 0x01,
        4000 => 0x02,
        2000 => 0x04,
        1000 => 0x08,
        500 => 0x10,
        250 => 0x20,
        125 => 0x40,
        _ => return None,
    })
}

pub fn byte_to_poll_rate(b: u8) -> Option<u32> {
    [8000, 4000, 2000, 1000, 500, 250, 125]
        .into_iter()
        .find(|&hz| poll_rate_to_byte(hz) == Some(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_openrazer_firmware_request() {
        // get_firmware_version with tid 0x1f: crc = 0x02 ^ 0x00 ^ 0x81 = 0x83
        let r = Report::new(cmd::GET_FIRMWARE_VERSION, 0x02, 0x1f).to_bytes();
        assert_eq!(r[88], 0x83);
        assert_eq!(&r[..8], &[0x00, 0x1f, 0, 0, 0, 0x02, 0x00, 0x81]);
    }

    #[test]
    fn roundtrip_and_report_id_prefix() {
        let r = Report::new(cmd::SET_DPI_XY, 7, 0x1f).with_args(&[1, 0x07, 0x08, 0x07, 0x08]);
        let b = r.to_bytes();
        assert_eq!(Report::parse(&b).unwrap(), r);
        let mut with_id = vec![0u8];
        with_id.extend_from_slice(&b);
        assert_eq!(Report::parse(&with_id).unwrap(), r);
        assert!(Report::parse(&b[..50]).is_err());
    }

    #[test]
    fn status_mirrors_protocol25_js() {
        let mut r = Report::new(cmd::GET_BATTERY, 2, 0x1f);
        r.status = 2;
        assert_eq!(r.status_for(cmd::GET_BATTERY), Status::Success);
        assert_eq!(r.status_for(cmd::GET_CHARGING), Status::Mismatch);
        r.status = 1;
        assert_eq!(r.status_for(cmd::GET_BATTERY), Status::Busy);
        r.status = 3;
        assert_eq!(r.status_for(cmd::GET_BATTERY), Status::Failed);
        r.status = 0x99;
        assert_eq!(r.status_for(cmd::GET_BATTERY), Status::Failed);
    }

    #[test]
    fn poll_rate_codes() {
        for hz in [125, 250, 500, 1000, 2000, 4000, 8000] {
            assert_eq!(byte_to_poll_rate(poll_rate_to_byte(hz).unwrap()), Some(hz));
        }
        assert_eq!(poll_rate_to_byte(333), None);
    }
}
