//! razerctl — a Linux rewrite of the device-control core of Razer Synapse 4.
//!
//! See README.md for what was reverse engineered and what was intentionally left out.

use razerctl::cli::{self, Fail};
use razerctl::{battery, device, hidraw, onboard, protocol, update};

use device::{Backend, Prefer};
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
    update [--check]         Ask GitHub whether a newer release exists (only when you run this).
                             Without --check: show what is new, then ask y/N before replacing an
                             AppImage; other installs get the release link or the package command

OPTIONS:
    --backend   auto (default): use OpenRazer sysfs when its driver is bound, else hidraw
    --tid       transaction id byte (default 0x1f, what Synapse/OpenRazer use for modern mice)
    -d N        select device index from `list` (default 0)
    -V, --version  print the version and exit
";

fn parse_u8(s: &str) -> Result<u8, String> {
    let t = s.trim_start_matches("0x");
    u8::from_str_radix(t, 16).map_err(|e| format!("bad hex byte '{s}': {e}"))
}

fn run() -> Result<(), Fail> {
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
                    o => return Err(format!("bad --backend {o:?}").into()),
                };
                args.drain(i..i + 2);
            }
            "--tid" => {
                tid = parse_u8(args.get(i + 1).ok_or("--tid needs a value")?)?;
                args.drain(i..i + 2);
            }
            "-d" => {
                idx = args
                    .get(i + 1)
                    .ok_or("-d needs a value")?
                    .parse()
                    .map_err(|e| format!("{e}"))?;
                args.drain(i..i + 2);
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            // like --help, this returns before device discovery and any config read
            "-V" | "--version" => {
                println!("razerctl {}", env!("CARGO_PKG_VERSION"));
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
        return cli::remap_command(&command, rest);
    }

    if command == "macros" {
        return cli::macros_cmd(rest).map_err(Fail::from);
    }

    if command == "update" {
        let stdin = std::io::stdin();
        return update::cli::run(
            rest,
            &update::Source::from_env(),
            &update::current_version(),
            &update::InstallType::detect(),
            &mut stdin.lock(),
            &mut std::io::stdout(),
        )
        .map_err(Fail::from);
    }

    if command == "battery-watch" {
        battery::watch_forever();
    }
    if command == "battery-notify-test" {
        let (name, pct, _) = battery::read_battery().unwrap_or(("Razer mouse".into(), 0, false));
        return battery::notify(
            &format!("{name}: test notification"),
            &format!("Battery is at {pct}%. Low-battery alerts will look like this."),
            false,
        )
        .map_err(|e| Fail::from(format!("couldn't send notification: {e}")));
    }

    if command == "list" {
        for (i, n) in hidraw::enumerate()
            .map_err(|e| e.to_string())?
            .iter()
            .enumerate()
        {
            println!(
                "  iface  {} {:04x}:{:04x} if={} feature={} {}",
                n.dev.display(),
                n.vid,
                n.pid,
                n.interface.map(|v| v.to_string()).unwrap_or("?".into()),
                n.has_feature_reports,
                n.name
            );
            let _ = i;
        }
        for (i, (node, b)) in device::open_all(prefer, tid)
            .map_err(|e| e.to_string())?
            .iter()
            .enumerate()
        {
            match b {
                Ok(b) => println!(
                    "[{i}] {} via {} ({})",
                    b.name(),
                    b.kind(),
                    b.path().display()
                ),
                Err(e) => println!(
                    "[{i}] {:04x}:{:04x} {} — unavailable: {e}",
                    node.vid,
                    node.pid,
                    node.dev.display()
                ),
            }
        }
        return Ok(());
    }

    let mut devs = device::open_all(prefer, tid).map_err(|e| e.to_string())?;
    if devs.is_empty() {
        return Err("no Razer (VID 1532) HID devices found".into());
    }
    if idx >= devs.len() {
        return Err(format!("device index {idx} out of range (found {})", devs.len()).into());
    }
    let (node, dev) = devs.swap_remove(idx);
    let dev: Box<dyn Backend> = dev.map_err(|e| {
        let hint = if e.kind() == std::io::ErrorKind::PermissionDenied {
            " — install udev/70-razerctl.rules or use --backend openrazer"
        } else {
            ""
        };
        format!("{}: {e}{hint}", node.dev.display())
    })?;
    let e = |e: std::io::Error| e.to_string();

    match command.as_str() {
        "info" => {
            let i = dev.info();
            println!(
                "Device:      {}  [{} {}]",
                i.name,
                dev.kind(),
                dev.path().display()
            );
            let show = |k: &str, v: Option<String>| {
                println!("{k:<12} {}", v.unwrap_or_else(|| "n/a".into()))
            };
            show("Firmware:", i.firmware);
            show("Serial:", i.serial);
            show("DPI:", i.dpi.map(|(x, y)| format!("{x} x {y}")));
            show(
                "DPI stages:",
                i.dpi_stages.map(|s| {
                    s.stages
                        .iter()
                        .enumerate()
                        .map(|(n, (x, y))| {
                            let mark = if n + 1 == s.active as usize { "*" } else { "" };
                            if x == y {
                                format!("{mark}{x}")
                            } else {
                                format!("{mark}{x}x{y}")
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                }),
            );
            if let Some(e) = &i.dpi_stages_error {
                println!("{:<12} ({e})", "");
            }
            show("Poll rate:", i.poll_rate.map(|p| format!("{p} Hz")));
            show(
                "Battery:",
                i.battery_pct.map(|p| {
                    format!(
                        "{p}%{}",
                        match i.charging {
                            Some(true) => " (charging)",
                            Some(false) => "",
                            None => "",
                        }
                    )
                }),
            );
        }
        "dpi" => match rest {
            [] => {
                let (x, y) = dev.dpi().map_err(e)?;
                println!("{x} {y}");
            }
            [x] | [x, _] => {
                let x: u16 = x.parse().map_err(|_| "DPI must be a number")?;
                let y: u16 = rest
                    .get(1)
                    .map(|y| y.parse())
                    .transpose()
                    .map_err(|_| "DPI must be a number")?
                    .unwrap_or(x);
                dev.set_dpi(x, y).map_err(e)?;
                let (rx, ry) = dev.dpi().map_err(e)?;
                println!("DPI set: {rx} {ry}");
            }
            _ => return Err("usage: dpi [X [Y]]".into()),
        },
        "poll" => match rest {
            [] => println!("{}", dev.poll_rate().map_err(e)?),
            [hz] => {
                dev.set_poll_rate(hz.parse().map_err(|_| "poll rate must be a number")?)
                    .map_err(e)?;
                println!("Poll rate set: {} Hz", dev.poll_rate().map_err(e)?);
            }
            _ => return Err("usage: poll [HZ]".into()),
        },
        "battery" => {
            let pct = device::raw_to_pct(dev.battery_raw().map_err(e)?);
            let ch = dev
                .charging()
                .map(|c| if c { " (charging)" } else { "" })
                .unwrap_or("");
            println!("{pct}%{ch}");
        }
        "onboard" => {
            let h = hidraw::Hidraw::open(&node.dev).map_err(e)?;
            let ob = onboard::Onboard { dev: &h, tid };
            if rest.first().map(|s| s.as_str()) == Some("set") {
                let hyper = rest.iter().any(|s| s == "--hypershift");
                let pos: Vec<&String> = rest[1..].iter().filter(|s| !s.starts_with("--")).collect();
                if pos.len() != 2 {
                    return Err("usage: onboard set BUTTON ACTION [--hypershift]".into());
                }
                let button = onboard::parse_button(pos[0])?;
                if button == 1 {
                    return Err(
                        "refusing to remap the left button (you could lose your only way to click)"
                            .into(),
                    );
                }
                let want = onboard::parse_action(button, pos[1])?;
                let mode = if hyper {
                    onboard::MODE_HYPERSHIFT
                } else {
                    onboard::MODE_NORMAL
                };
                let profiles = ob.profile_ids().map_err(e)?;
                let profile = *profiles.first().ok_or("mouse reports no onboard profile")?;
                let before = ob.assignment(profile, button, mode).map_err(e)?;
                println!(
                    "{} (profile {profile}, {}): {before}  ->  {want}",
                    onboard::button_name(button),
                    if hyper { "hypershift" } else { "normal" }
                );
                let got = ob.set_assignment(profile, mode, &want).map_err(e)?;
                println!("saved to mouse; read back: {got}");
                return Ok(());
            }
            let profiles = ob.profile_ids().map_err(e)?;
            let buttons = ob.button_ids().map_err(e)?;
            println!("Onboard profiles: {profiles:?}");
            println!(
                "Buttons:          {}",
                buttons
                    .iter()
                    .map(|&b| onboard::button_name(b))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            for &p in &profiles {
                for (mode, label) in [
                    (onboard::MODE_NORMAL, "normal"),
                    (onboard::MODE_HYPERSHIFT, "hypershift"),
                ] {
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
            if rest.len() < 3 {
                return Err("usage: raw CLASS ID SIZE [ARGS..]".into());
            }
            let b: Vec<u8> = rest.iter().map(|s| parse_u8(s)).collect::<Result<_, _>>()?;
            let h = hidraw::Hidraw::open(&node.dev).map_err(e)?;
            let req = protocol::Report::new((b[0], b[1]), b[2], tid).with_args(&b[3..]);
            println!("-> {req:?}");
            println!("<- {:?}", h.transact(&req).map_err(e)?);
        }
        other => return Err(format!("unknown command '{other}'\n\n{USAGE}").into()),
    }
    Ok(())
}

fn main() -> ExitCode {
    ExitCode::from(cli::report(run()))
}
