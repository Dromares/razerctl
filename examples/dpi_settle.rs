//! Read-only: send GET_DPI_STAGES (04:86) once and keep re-reading the response for ~150 ms,
//! printing how many stage entries carry a non-zero DPI on every read. Shows whether a
//! Success (0x02) answer can still be filling in. `--after-dpi` first completes a GET_DPI_XY
//! (04:85) and sends 04:86 immediately after, like `Backend::info()` does. Writes nothing.
//!
//!   dpi_settle [rounds] [--after-dpi]
use razerctl::hidraw::{self, Hidraw};
use razerctl::protocol::{cmd, Report};
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rounds: usize = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(10);
    let after_dpi = args.iter().any(|a| a == "--after-dpi");
    let gap: u64 = args
        .iter()
        .position(|a| a == "--gap-ms")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let nodes = hidraw::enumerate().unwrap();
    let node = nodes
        .iter()
        .find(|n| n.has_feature_reports && n.interface == Some(0))
        .expect("no node");
    let h = Hidraw::open(&node.dev).unwrap();
    for r in 0..rounds {
        if after_dpi {
            let _ = h.transact(&Report::new(cmd::GET_DPI_XY, 0x07, 0x1f).with_args(&[1]));
        }
        std::thread::sleep(Duration::from_millis(gap));
        let req = Report::new(cmd::GET_DPI_STAGES, 0x26, 0x1f).with_args(&[1]);
        h.send_feature(&req.to_bytes()).unwrap();
        let t0 = Instant::now();
        let mut line = String::new();
        let mut last = String::new();
        while t0.elapsed() < Duration::from_millis(150) {
            std::thread::sleep(Duration::from_millis(3));
            let resp = Report::parse(&h.get_feature().unwrap()).unwrap();
            let filled = (0..5).filter(|i| resp.args[4 + i * 7] != 0).count();
            let cur = format!("s{:x}/{}/{}", resp.status, resp.command_id, filled);
            if cur != last {
                line += &format!(" {}ms:{cur}", t0.elapsed().as_millis());
                last = cur;
            }
        }
        println!("settle after_dpi={after_dpi} gap={gap} round={r}{line}");
        std::thread::sleep(Duration::from_millis(400));
    }
}
