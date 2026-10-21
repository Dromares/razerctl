//! The simulated device behind `firmware flash --dry-run`: it answers every frame with success, remembers what was
//! "programmed" so the read-back matches, and prints each frame as it goes by. Nothing here opens a device or a file
//! under /dev; the replies are fabricated and say nothing about what a real bootloader would do.

use super::dfu::{self, BootloaderSeen, Transport};
use super::Bootloader;
use crate::protocol::Report;
use std::collections::BTreeMap;

pub struct DryRun<W: std::io::Write> {
    pub out: W,
    pub device: (u16, u16),
    pub version: dfu::Version,
    mem: BTreeMap<u32, u8>,
    sent: usize,
}

impl<W: std::io::Write> DryRun<W> {
    pub fn new(out: W, device: (u16, u16), version: dfu::Version) -> Self {
        DryRun {
            out,
            device,
            version,
            mem: BTreeMap::new(),
            sent: 0,
        }
    }
    pub fn frames_sent(&self) -> usize {
        self.sent
    }
}

/// `#0007 10:02 size 8 tid 0 args 40 20 00 ...`: the args up to the last non-zero byte, or all the bytes the step uses.
pub fn describe_frame(n: usize, f: &Report) -> String {
    let used = f.args.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let hex: Vec<String> = f.args[..used].iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "#{n:04} {:02x}:{:02x} size {} tid {} args [{}]",
        f.command_class,
        f.command_id,
        f.data_size,
        f.transaction_id,
        hex.join(" ")
    )
}

impl<W: std::io::Write> Transport for DryRun<W> {
    fn device_id(&self) -> (u16, u16) {
        self.device
    }

    fn exchange(&mut self, frame: &Report) -> Result<Report, String> {
        self.sent += 1;
        writeln!(self.out, "{}", describe_frame(self.sent, frame)).map_err(|e| e.to_string())?;
        let mut reply = frame.clone();
        reply.status = 2;
        match (frame.command_class, frame.command_id) {
            dfu::READ_VERSION => {
                reply.args[..3].copy_from_slice(&[self.version.0, self.version.1, self.version.2]);
            }
            dfu::PROGRAM => {
                let addr = u32::from_be_bytes(frame.args[1..5].try_into().unwrap());
                for (i, &b) in frame.args[5..5 + dfu::PACKET].iter().enumerate() {
                    self.mem.insert(addr + i as u32, b);
                }
            }
            dfu::VERIFY => {
                let addr = u32::from_be_bytes(frame.args[1..5].try_into().unwrap());
                for i in 0..dfu::PACKET {
                    reply.args[5 + i] = self.mem.get(&(addr + i as u32)).copied().unwrap_or(0xFF);
                }
            }
            _ => {}
        }
        Ok(reply)
    }

    fn wait_for_bootloader(&mut self, expect: &Bootloader) -> Result<BootloaderSeen, String> {
        writeln!(
            self.out,
            "      (wait for bootloader {:04x}:{:04x} bcdDevice {:04x}; simulated, it appears at once)",
            expect.vid, expect.pid, expect.bcd
        )
        .map_err(|e| e.to_string())?;
        Ok(BootloaderSeen {
            vid: expect.vid,
            pid: expect.pid,
            bcd: expect.bcd,
        })
    }
}
