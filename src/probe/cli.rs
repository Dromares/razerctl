//! `razerctl firmware probe-bootloader [--yes-enter-bootloader] [--out FILE]`: argument handling, the refusals that come
//! before the device is touched, and the output file. The order is fixed and every step before the last can only refuse:
//!
//! 1. without `--yes-enter-bootloader`: print the plan, exit non-zero, open nothing;
//! 2. find the starting identity: exactly one of the dongle (`1532:009F`) and the cable (`1532:009E`) must be on hidraw (both, or
//!    neither, is refused), and it must be exactly one device with a usable control node;
//! 3. refuse if other razerctl or openrazer traffic is on it ([`super::refusals`]);
//! 4. create the output file (it must not exist yet), so an unwritable path is found before the device is touched;
//! 5. open the control node and run the steps ([`super::run`]).

use super::hw::{self, RealProbe, Roots};
use super::{plan_text, refusals, report::ProbeLog, run, Build};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub const USAGE: &str = "usage: firmware probe-bootloader [--yes-enter-bootloader] [--out FILE]";

#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    pub yes: bool,
    pub out: Option<PathBuf>,
}

pub fn parse(args: &[String]) -> Result<Args, String> {
    let (mut yes, mut out) = (false, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--yes-enter-bootloader" => yes = true,
            "--out" => out = Some(PathBuf::from(it.next().ok_or("--out needs a file name")?)),
            o => return Err(format!("unexpected argument '{o}'\n{USAGE}")),
        }
    }
    Ok(Args { yes, out })
}

pub fn build() -> Build {
    Build {
        version: env!("CARGO_PKG_VERSION").into(),
        git_sha: option_env!("RAZERCTL_GIT_SHA").unwrap_or("unknown").into(),
    }
}

fn default_out() -> PathBuf {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    PathBuf::from(format!("razerctl-bootloader-probe-{secs}.txt"))
}

/// Create the report file, refusing to overwrite one.
pub fn create_out(path: &Path) -> Result<(), String> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map(|_| ())
        .map_err(|e| format!("{}: {e} (nothing was sent to the device)", path.display()))
}

pub fn probe_bootloader(args: &[String]) -> Result<(), String> {
    let a = parse(args)?;
    if !a.yes {
        println!("{}", plan_text());
        return Err("refusing to run without --yes-enter-bootloader; nothing was sent".into());
    }

    let all = hw::probe_nodes().map_err(|e| format!("listing HID devices: {e}"))?;
    let start = hw::select_start(&all)?;
    let nodes: Vec<_> = all
        .into_iter()
        .filter(|n| (n.vid, n.pid) == start.id())
        .collect();
    let devices: std::collections::HashSet<_> = nodes
        .iter()
        .filter_map(|n| {
            fs::canonicalize(&n.sysfs)
                .ok()?
                .parent()?
                .parent()
                .map(Path::to_path_buf)
        })
        .collect();
    if devices.len() > 1 {
        return Err(format!(
            "refusing: more than one {} device is connected; unplug the others. Nothing was sent",
            start.id_text()
        ));
    }
    let Some(control) = hw::control_node(&nodes) else {
        return Err(format!(
            "refusing: no {} interface with a feature report was found; nothing was sent",
            start.id_text()
        ));
    };

    let (traffic, unchecked) = hw::collect_traffic(Path::new("/proc"), &nodes, std::process::id());
    let why = refusals(&traffic);
    if !why.is_empty() {
        let mut msg =
            String::from("refusing: other programs are using the device; nothing was sent\n");
        for w in why {
            msg.push_str(&format!("  - {w}\n"));
        }
        return Err(msg.trim_end().into());
    }
    if unchecked > 0 {
        eprintln!(
            "note: the open files of {unchecked} process(es) owned by other users could not be read, so a program of theirs holding {} would not have been seen",
            control.dev.display()
        );
    }

    let out = a.out.unwrap_or_else(default_out);
    create_out(&out)?;
    let mut probe = RealProbe::open(control, Roots::system()).map_err(|e| {
        format!(
            "refusing: cannot open {} for read/write: {e} (install udev/70-razerctl.rules); nothing was sent",
            control.dev.display()
        )
    })?;

    eprintln!(
        "starting from the {}; entering the bootloader of {} now; the mouse stops working until this ends (writing {})",
        start.label(),
        control.dev.display(),
        out.display()
    );
    let mut write_failed = false;
    let log = run(&mut probe, &build(), &mut |l: &ProbeLog| {
        if let Err(e) = fs::write(&out, l.render()) {
            if !write_failed {
                eprintln!(
                    "warning: cannot write {}: {e}; the report will be printed below",
                    out.display()
                );
            }
            write_failed = true;
        }
    });
    let text = log.render();
    println!("{text}");
    if !write_failed {
        println!(
            "report written to {}; paste it onto DAN-1878",
            out.display()
        );
    }
    let _ = std::io::stdout().flush();
    let outcome = log.outcome.as_ref();
    for n in outcome.map(|o| o.next_steps(start)).unwrap_or_default() {
        eprintln!("{n}");
    }
    match outcome {
        Some(o) if o.is_success() => Ok(()),
        _ => Err("the probe did not complete; see the report above".into()),
    }
}
