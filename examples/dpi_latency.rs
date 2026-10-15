//! Read-only: send one GET_FIRMWARE_VERSION (00:81) per round, poll up to `--max-ms` (default 6000)
//! and print how long the answer took (Busy 0x01 -> Success 0x02), after `--idle-ms` of silence
//! (default 8000). Shows how long an idle wireless mouse keeps the receiver busy. Writes nothing.
//!
//!   dpi_latency [rounds] [--idle-ms N] [--max-ms N] [--resend-ms N]   (resend the query every N ms while Busy)
use razerctl::hidraw::{self, Hidraw};
use razerctl::protocol::{cmd, Report};
use std::time::{Duration, Instant};

fn flag(args: &[String], name: &str, default: u64) -> u64 {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rounds: usize = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(10);
    let idle = flag(&args, "--idle-ms", 8000);
    let max = Duration::from_millis(flag(&args, "--max-ms", 6000));
    let resend = flag(&args, "--resend-ms", 0);
    let nodes = hidraw::enumerate().unwrap();
    let node = nodes
        .iter()
        .find(|n| n.has_feature_reports && n.interface == Some(0))
        .expect("no node");
    let h = Hidraw::open(&node.dev).unwrap();
    for r in 0..rounds {
        std::thread::sleep(Duration::from_millis(idle));
        h.send_feature(&Report::new(cmd::GET_FIRMWARE_VERSION, 0x02, 0x1f).to_bytes())
            .unwrap();
        let t0 = Instant::now();
        let (mut status, mut polls, mut sends, mut since) = (0, 0, 1, Instant::now());
        while t0.elapsed() < max {
            std::thread::sleep(Duration::from_millis(5));
            let resp = Report::parse(&h.get_feature().unwrap()).unwrap();
            status = resp.status;
            polls += 1;
            if resp.status == 1 && resend > 0 && since.elapsed() >= Duration::from_millis(resend) {
                h.send_feature(&Report::new(cmd::GET_FIRMWARE_VERSION, 0x02, 0x1f).to_bytes())
                    .unwrap();
                sends += 1;
                since = Instant::now();
            }
            if resp.status != 1 {
                break;
            }
        }
        println!(
            "latency round={r} idle_ms={idle} answered_after_ms={} polls={polls} sends={sends} resend_ms={resend} status={status:#04x}",
            t0.elapsed().as_millis()
        );
    }
}
