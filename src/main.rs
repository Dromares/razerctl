//! razerctl — a Linux rewrite of the device-control core of Razer Synapse 4.
//!
//! See README.md for what was reverse engineered and what was intentionally left out.

use razerctl::{battery, device, hidraw, macros, onboard, protocol, remap};

use device::{Backend, Prefer};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
razerctl — control Razer peripherals on Linux (Synapse 4 Protocol25 reimplementation)

USAGE:
    razerctl [--backend auto|hidraw|openrazer] [--tid 0xNN] [-d N] <COMMAND>

COMMANDS:
    list                     List Razer HID interfaces and the chosen backend
    info                     Firmware, serial, DPI, DPI stages, poll rate, battery
    dpi [X [Y]]              Get or set DPI (100-45000)
    poll [HZ]                Get or set polling rate (125/250/500/1000/2000/4000/8000)
    battery                  Battery percentage and charging state
    battery-watch            Low-battery notifier loop (run by razerctl-battery.service;
                             settings in ~/.config/razerctl/battery.conf)
    battery-notify-test      Send a sample low-battery notification
    raw CLASS ID SIZE [ARGS] Send a raw Protocol25 command (hidraw backend only), hex bytes
    onboard                  Show button assignments stored in the mouse's onboard memory
    onboard set BUTTON ACTION [--hypershift]
                             Save a remap INTO the mouse (persists without software).
                             BUTTON: left right middle back forward dpi wheel-up wheel-down
                             ACTION: default disable left-click right-click middle-click back
                                     forward wheel-up wheel-down dpi-up dpi-down dpi-cycle-up
                                     dpi-cycle-down double-click key:[ctrl+shift+alt+super+]KEY
    buttons                  Print button names as you press them (find what to remap)
    remap [--map FROM=TO]... [--config FILE] [--device NAME] [--macros FILE]
                             Grab the mouse and remap buttons (Ctrl+C to stop).
                             Default config: ~/.config/razerctl/remap.conf, lines like
                               back = ctrl+c   forward = KEY_F13   middle = disable
                             Actions: key combos (ctrl+shift+t), any KEY_*/BTN_* name,
                             scroll_up/down/left/right, disable,
                             macro:NAME[,once|xN|held|toggle] (played on this computer)
    macros [list] [--file FILE]
                             List macros: events, run time incl. padding
                             Default file: ~/.config/razerctl/macros.conf
    macros show NAME         Print one macro in canonical form
    macros check [--config FILE]
                             Validate macros.conf and the macro:NAME refs in remap.conf
    macros play NAME --dry-run
                             Print the exact frames and sleeps a press would send
                             (live playback only via remap; no device is opened)

OPTIONS:
    --backend   auto (default): use OpenRazer sysfs when its driver is bound, else hidraw
    --tid       transaction id byte (default 0x1f, what Synapse/OpenRazer use for modern mice)
    -d N        select device index from `list` (default 0)
";

fn parse_u8(s: &str) -> Result<u8, String> {
    let t = s.trim_start_matches("0x");
    u8::from_str_radix(t, 16).map_err(|e| format!("bad hex byte '{s}': {e}"))
}

/// The macros `remap` needs: None if the mapping has no macro:NAME (the file isn't read),
/// else the loaded file with every referenced name present. Runs before any grab.
fn remap_macros(map: &remap::Mapping, file: &Path) -> Result<Option<macros::Macros>, String> {
    if !map.values().any(|a| matches!(a, remap::Action::Macro { .. })) {
        return Ok(None);
    }
    if !file.exists() {
        return Err(format!("{}: not found (the mapping uses macro:NAME; create it or pass --macros FILE)", file.display()));
    }
    let m = macros::load_macros(file)?;
    remap::check_macros(map, &m).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(Some(m))
}

/// `macros [list|show NAME|check|play NAME --dry-run]`. Reads text files only, never a device.
/// A missing default file means "no macros"/"no remap rules"; a missing explicit one is an error.
fn macros_cmd(args: &[String]) -> Result<(), String> {
    let (mut file, mut cfg, mut dry, mut pos) = (None::<PathBuf>, None::<PathBuf>, false, Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--file" => file = Some(it.next().ok_or("--file needs a path")?.into()),
            "--config" => cfg = Some(it.next().ok_or("--config needs a path")?.into()),
            "--dry-run" => dry = true,
            o if o.starts_with('-') => return Err(format!("unexpected argument '{o}'")),
            o => pos.push(o),
        }
    }
    let sub = pos.first().copied().unwrap_or("list");
    if cfg.is_some() && sub != "check" {
        return Err("--config only applies to `macros check`".into());
    }
    if dry && sub != "play" {
        return Err("--dry-run only applies to `macros play`".into());
    }
    if sub == "play" && !dry {
        return Err("live playback only happens inside `razerctl remap` (bind it with BUTTON = macro:NAME in \
                    remap.conf), and testing it on the device needs the board's go-ahead; \
                    use `macros play NAME --dry-run` to print what would be sent"
            .into());
    }
    let explicit = file.is_some();
    let file = file.unwrap_or_else(macros::macros_path);
    if explicit && !file.exists() {
        return Err(format!("{}: not found", file.display()));
    }
    let m = macros::load_macros(&file)?;
    let get = |name: &str| m.get(name).ok_or_else(|| format!("no macro '{name}' in {}", file.display()));
    match pos.as_slice() {
        [] | ["list"] => {
            if m.is_empty() { eprintln!("no macros in {}", file.display()); }
            for l in macros::describe_macros(&m) { println!("{l}"); }
        }
        ["show", name] => {
            let mac = get(name)?;
            let s = macros::serialize_macros(&macros::Macros::from([(mac.name.clone(), mac.clone())]));
            // drop the file header comment
            print!("{}", s.split_once("\n\n").map_or(s.as_str(), |(_, body)| body));
        }
        ["check"] => {
            println!("{}: {} macro{} OK", file.display(), m.len(), if m.len() == 1 { "" } else { "s" });
            let explicit = cfg.is_some();
            let cfg = cfg.unwrap_or_else(remap::config_path);
            let text = match std::fs::read_to_string(&cfg) {
                Ok(t) => t,
                Err(e) if !explicit && e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(format!("{}: {e}", cfg.display())),
            };
            let map = remap::parse_config(&text).map_err(|e| format!("{}: {e}", cfg.display()))?;
            remap::check_macros(&map, &m).map_err(|e| format!("{}: {e} (not in {})", cfg.display(), file.display()))?;
            let n = map.values().filter(|a| matches!(a, remap::Action::Macro { .. })).count();
            println!("{}: {n} macro reference{} OK", cfg.display(), if n == 1 { "" } else { "s" });
        }
        ["play", name] => {
            let mac = get(name)?;
            eprintln!("{name}: {} events, {} ms (dry run, nothing is sent)", mac.events.len(), macros::play_ms(&mac.events));
            for l in macros::describe_steps(&macros::to_steps(&macros::pad(&mac.events))) { println!("{l}"); }
        }
        _ => return Err("usage: macros [list | show NAME | check [--config FILE] | play NAME --dry-run] [--file FILE]".into()),
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut prefer = Prefer::Auto;
    let mut tid = 0x1f;
    let mut idx = 0usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--backend" => {
                prefer = match args.get(i + 1).map(String::as_str) {
                    Some("auto") => Prefer::Auto,
                    Some("hidraw") => Prefer::Hidraw,
                    Some("openrazer") => Prefer::OpenRazer,
                    o => return Err(format!("bad --backend {o:?}")),
                };
                args.drain(i..i + 2);
            }
            "--tid" => {
                tid = parse_u8(args.get(i + 1).ok_or("--tid needs a value")?)?;
                args.drain(i..i + 2);
            }
            "-d" => {
                idx = args.get(i + 1).ok_or("-d needs a value")?.parse().map_err(|e| format!("{e}"))?;
                args.drain(i..i + 2);
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            _ => i += 1,
        }
    }
    let Some(command) = args.first().cloned() else {
        print!("{USAGE}");
        return Ok(());
    };
    let rest = &args[1..];

    if command == "buttons" || command == "remap" {
        let mut inline = Vec::new();
        let mut cfg = remap::config_path();
        let mut filter: Option<String> = None;
        let mut mfile = macros::macros_path();
        let mut it = rest.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--map" => inline.push(it.next().ok_or("--map needs FROM=TO")?.clone()),
                "--config" => cfg = it.next().ok_or("--config needs a path")?.into(),
                "--device" => filter = Some(it.next().ok_or("--device needs a name")?.clone()),
                "--macros" if command == "remap" => mfile = it.next().ok_or("--macros needs a path")?.into(),
                o => return Err(format!("unexpected argument '{o}'")),
            }
        }
        let sources = || -> Result<Vec<remap::Source>, String> {
            let s = remap::find_sources(filter.as_deref()).map_err(|e| e.to_string())?;
            if s.is_empty() {
                return Err("no readable Razer input devices in /dev/input (are you in the openrazer/input group?)".into());
            }
            Ok(s)
        };
        if command == "buttons" {
            let sources = sources()?;
            for s in &sources { eprintln!("watching {} ({})", s.path.display(), s.name); }
            eprintln!("press buttons; Ctrl+C to stop");
            return remap::watch(&sources).map_err(|e| e.to_string());
        }
        // every config error before anything is grabbed
        let map = remap::load_mapping(&cfg, &inline)?;
        if map.is_empty() {
            return Err("no remap rules".into());
        }
        let macros = remap_macros(&map, &mfile)?;
        for (k, v) in &map { eprintln!("  {k:?} -> {v:?}"); }
        let sources = sources()?;
        return match macros {
            Some(m) => remap::run_with_macros(sources, map, m),
            None => remap::run(sources, map),
        }
        .map_err(|e| e.to_string());
    }

    if command == "macros" {
        return macros_cmd(rest);
    }

    if command == "battery-watch" {
        battery::watch_forever();
    }
    if command == "battery-notify-test" {
        let (name, pct, _) = battery::read_battery().unwrap_or(("Razer mouse".into(), 0, false));
        return battery::notify(&format!("{name}: test notification"), &format!("Battery is at {pct}%. Low-battery alerts will look like this."), false)
            .map_err(|e| format!("couldn't send notification: {e}"));
    }

    if command == "list" {
        for (i, n) in hidraw::enumerate().map_err(|e| e.to_string())?.iter().enumerate() {
            println!(
                "  iface  {} {:04x}:{:04x} if={} feature={} {}",
                n.dev.display(), n.vid, n.pid,
                n.interface.map(|v| v.to_string()).unwrap_or("?".into()),
                n.has_feature_reports, n.name
            );
            let _ = i;
        }
        for (i, (node, b)) in device::open_all(prefer, tid).map_err(|e| e.to_string())?.iter().enumerate() {
            match b {
                Ok(b) => println!("[{i}] {} via {} ({})", b.name(), b.kind(), b.path().display()),
                Err(e) => println!("[{i}] {:04x}:{:04x} {} — unavailable: {e}", node.vid, node.pid, node.dev.display()),
            }
        }
        return Ok(());
    }

    let mut devs = device::open_all(prefer, tid).map_err(|e| e.to_string())?;
    if devs.is_empty() {
        return Err("no Razer (VID 1532) HID devices found".into());
    }
    if idx >= devs.len() {
        return Err(format!("device index {idx} out of range (found {})", devs.len()));
    }
    let (node, dev) = devs.swap_remove(idx);
    let dev: Box<dyn Backend> = dev.map_err(|e| {
        let hint = if e.kind() == std::io::ErrorKind::PermissionDenied {
            " — install udev/70-razerctl.rules or use --backend openrazer"
        } else { "" };
        format!("{}: {e}{hint}", node.dev.display())
    })?;
    let e = |e: std::io::Error| e.to_string();

    match command.as_str() {
        "info" => {
            let i = dev.info();
            println!("Device:      {}  [{} {}]", i.name, dev.kind(), dev.path().display());
            let show = |k: &str, v: Option<String>| println!("{k:<12} {}", v.unwrap_or_else(|| "n/a".into()));
            show("Firmware:", i.firmware);
            show("Serial:", i.serial);
            show("DPI:", i.dpi.map(|(x, y)| format!("{x} x {y}")));
            show("DPI stages:", i.dpi_stages.map(|s| {
                s.stages.iter().enumerate()
                    .map(|(n, (x, y))| {
                        let mark = if n + 1 == s.active as usize { "*" } else { "" };
                        if x == y { format!("{mark}{x}") } else { format!("{mark}{x}x{y}") }
                    })
                    .collect::<Vec<_>>().join(" ")
            }));
            show("Poll rate:", i.poll_rate.map(|p| format!("{p} Hz")));
            show("Battery:", i.battery_pct.map(|p| format!(
                "{p}%{}", match i.charging { Some(true) => " (charging)", Some(false) => "", None => "" })));
        }
        "dpi" => match rest {
            [] => { let (x, y) = dev.dpi().map_err(e)?; println!("{x} {y}"); }
            [x] | [x, _] => {
                let x: u16 = x.parse().map_err(|_| "DPI must be a number")?;
                let y: u16 = rest.get(1).map(|y| y.parse()).transpose().map_err(|_| "DPI must be a number")?.unwrap_or(x);
                dev.set_dpi(x, y).map_err(e)?;
                let (rx, ry) = dev.dpi().map_err(e)?;
                println!("DPI set: {rx} {ry}");
            }
            _ => return Err("usage: dpi [X [Y]]".into()),
        },
        "poll" => match rest {
            [] => println!("{}", dev.poll_rate().map_err(e)?),
            [hz] => {
                dev.set_poll_rate(hz.parse().map_err(|_| "poll rate must be a number")?).map_err(e)?;
                println!("Poll rate set: {} Hz", dev.poll_rate().map_err(e)?);
            }
            _ => return Err("usage: poll [HZ]".into()),
        },
        "battery" => {
            let pct = device::raw_to_pct(dev.battery_raw().map_err(e)?);
            let ch = dev.charging().map(|c| if c { " (charging)" } else { "" }).unwrap_or("");
            println!("{pct}%{ch}");
        }
        "onboard" => {
            let h = hidraw::Hidraw::open(&node.dev).map_err(e)?;
            let ob = onboard::Onboard { dev: &h, tid };
            if rest.first().map(|s| s.as_str()) == Some("set") {
                let hyper = rest.iter().any(|s| s == "--hypershift");
                let pos: Vec<&String> = rest[1..].iter().filter(|s| !s.starts_with("--")).collect();
                if pos.len() != 2 { return Err("usage: onboard set BUTTON ACTION [--hypershift]".into()); }
                let button = onboard::parse_button(pos[0])?;
                if button == 1 { return Err("refusing to remap the left button (you could lose your only way to click)".into()); }
                let want = onboard::parse_action(button, pos[1])?;
                let mode = if hyper { onboard::MODE_HYPERSHIFT } else { onboard::MODE_NORMAL };
                let profiles = ob.profile_ids().map_err(e)?;
                let profile = *profiles.first().ok_or("mouse reports no onboard profile")?;
                let before = ob.assignment(profile, button, mode).map_err(e)?;
                println!("{} (profile {profile}, {}): {before}  ->  {want}", onboard::button_name(button), if hyper { "hypershift" } else { "normal" });
                let got = ob.set_assignment(profile, mode, &want).map_err(e)?;
                println!("saved to mouse; read back: {got}");
                return Ok(());
            }
            let profiles = ob.profile_ids().map_err(e)?;
            let buttons = ob.button_ids().map_err(e)?;
            println!("Onboard profiles: {profiles:?}");
            println!("Buttons:          {}", buttons.iter().map(|&b| onboard::button_name(b)).collect::<Vec<_>>().join(", "));
            for &p in &profiles {
                for (mode, label) in [(onboard::MODE_NORMAL, "normal"), (onboard::MODE_HYPERSHIFT, "hypershift")] {
                    println!("\nProfile {p}, {label} layer:");
                    for a in ob.assignments(p, &buttons, mode).map_err(e)? {
                        let tag = if a.is_default() { "" } else { "   <- changed" };
                        println!("  {:<11} {}{tag}", onboard::button_name(a.button), a);
                    }
                }
            }
        }
        "raw" => {
            if dev.kind() != "hidraw" {
                return Err("raw needs --backend hidraw".into());
            }
            if rest.len() < 3 { return Err("usage: raw CLASS ID SIZE [ARGS..]".into()); }
            let b: Vec<u8> = rest.iter().map(|s| parse_u8(s)).collect::<Result<_, _>>()?;
            let h = hidraw::Hidraw::open(&node.dev).map_err(e)?;
            let req = protocol::Report::new((b[0], b[1]), b[2], tid).with_args(&b[3..]);
            println!("-> {req:?}");
            println!("<- {:?}", h.transact(&req).map_err(e)?);
        }
        other => return Err(format!("unknown command '{other}'\n\n{USAGE}")),
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(m) => {
            eprintln!("razerctl: {m}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remap_reads_macros_only_when_mapped_and_checks_names() {
        let dir = std::env::temp_dir().join(format!("razerctl-main-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("macros.conf");
        let plain = remap::parse_config("back = ctrl+c").unwrap();
        let uses = remap::parse_config("back = macro:copy,held").unwrap();
        // no macro actions: the file is never needed
        assert_eq!(remap_macros(&plain, &file), Ok(None));
        assert!(remap_macros(&uses, &file).unwrap_err().contains("not found"));
        std::fs::write(&file, "[other]\ntap a\n").unwrap();
        assert!(remap_macros(&uses, &file).unwrap_err().ends_with("undefined macro 'copy'"));
        std::fs::write(&file, "[copy]\ndown a\n").unwrap();
        assert!(remap_macros(&uses, &file).unwrap_err().contains("leaves KEY_A pressed"));
        std::fs::write(&file, "[copy]\ntap ctrl\n").unwrap();
        assert_eq!(remap_macros(&uses, &file).unwrap().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
