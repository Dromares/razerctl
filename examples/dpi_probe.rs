//! Read-only repro harness for DAN-1526: runs the same device reads `razerctl-gui` does on
//! start (`refresh_device` -> `Backend::info()`, then `refresh_buttons` -> onboard profile/button
//! reads) once, then `--refreshes` more `info()` rounds, and prints one line per round saying
//! whether the DPI stages came back complete. Issues no writes: only `Backend::info()` getters
//! and `Onboard` read queries. Pair with `RAZERCTL_HID_TRACE=1` to see every raw response.
//!
//!   cargo run --release --example dpi_probe -- [--expect 5] [--refreshes 3] [--gap-ms 400] [--tag NAME]

use std::time::{Duration, Instant};

use razerctl::device::{self, Info, Prefer};
use razerctl::hidraw::Hidraw;
use razerctl::onboard::Onboard;

const TID: u8 = 0x1f;

fn arg(args: &[String], name: &str, default: u64) -> u64 {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn describe(i: &Info, expect: usize) -> (bool, String) {
    let stages = match &i.dpi_stages {
        None => "stages=none".to_string(),
        Some(s) => format!(
            "stages={}{} active={} values={:?}",
            s.stages.len(),
            if s.stages.len() == expect {
                ""
            } else {
                "(PARTIAL)"
            },
            s.active,
            s.stages.iter().map(|(x, _)| *x).collect::<Vec<_>>()
        ),
    };
    let ok = i
        .dpi_stages
        .as_ref()
        .is_some_and(|s| s.stages.len() == expect);
    let f = |b: bool| if b { "ok" } else { "MISSING" };
    (
        ok,
        format!(
            "{stages} fw={} serial={} dpi={} poll={} batt={}",
            f(i.firmware.is_some()),
            f(i.serial.is_some()),
            f(i.dpi.is_some()),
            f(i.poll_rate.is_some()),
            f(i.battery_pct.is_some())
        ),
    )
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let expect = arg(&args, "--expect", 5) as usize;
    let refreshes = arg(&args, "--refreshes", 3);
    let gap = Duration::from_millis(arg(&args, "--gap-ms", 400));
    let tag = args
        .iter()
        .position(|a| a == "--tag")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_default();

    for round in 0..=refreshes {
        let phase = if round == 0 { "cold" } else { "refresh" };
        let t0 = Instant::now();
        // refresh_device()
        let line = match device::open_all(Prefer::Auto, TID) {
            Ok(devs) => match devs.into_iter().next() {
                Some((_, Ok(b))) => {
                    let info = b.info();
                    let (ok, d) = describe(&info, expect);
                    format!("full={} backend={} {d}", ok as u8, b.kind())
                }
                Some((_, Err(e))) => format!("full=0 open_error={e}"),
                None => "full=0 no device".into(),
            },
            Err(e) => format!("full=0 enumerate_error={e}"),
        };
        println!(
            "probe tag={tag} round={round} phase={phase} ms={} {line}",
            t0.elapsed().as_millis()
        );
        // refresh_buttons(): only on the cold start, like App::new()
        if round == 0 {
            let t1 = Instant::now();
            let r = (|| -> Result<usize, String> {
                let devs = device::open_all(Prefer::Hidraw, TID).map_err(|e| e.to_string())?;
                let (node, _) = devs.into_iter().next().ok_or("no device")?;
                let h = Hidraw::open(&node.dev).map_err(|e| e.to_string())?;
                let ob = Onboard { dev: &h, tid: TID };
                let profile = *ob
                    .profile_ids()
                    .map_err(|e| e.to_string())?
                    .first()
                    .ok_or("no profile")?;
                let ids = ob.button_ids().map_err(|e| e.to_string())?;
                let rows = ob
                    .assignments(profile, &ids, 0)
                    .map_err(|e| e.to_string())?;
                Ok(rows.len())
            })();
            println!(
                "probe tag={tag} round={round} phase=buttons ms={} {}",
                t1.elapsed().as_millis(),
                match r {
                    Ok(n) => format!("buttons={n}"),
                    Err(e) => format!("buttons_error={e}"),
                }
            );
        }
        std::thread::sleep(gap);
    }
}
