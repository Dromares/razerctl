//! Test hooks for visual review. Nothing in here does anything unless a `RAZERCTL_GUI_*` variable is set, and no
//! hook changes what the app does on a normal launch.
//!
//!   RAZERCTL_GUI_SNAPSHOT=<dir>   render each tab (buttons / device / software .png), then exit
//!   RAZERCTL_GUI_TABS=a,b         limit the snapshot to some tabs
//!   RAZERCTL_GUI_THEME=dark|light force the palette (otherwise it follows the system theme)
//!   RAZERCTL_GUI_SIZE=WxH         initial window size
//!   RAZERCTL_GUI_DEMO=<scenario>  start from synthetic device state instead of reading hardware (see `apply`);
//!                                  `macro-*` scenarios also point the config paths into a temp dir
//!   RAZERCTL_GUI_POINTER=hover:<w>|press:<w>|click:<w>   synthetic pointer on a noted widget (hover / pressed /
//!                                  opened dropdown); <w> is save, refresh, combo<button id>
//!
//! Synthetic state only fills the fields the app normally fills from hardware; every screen is still drawn by the
//! real UI code, and Save / Apply / systemctl keep calling the real device and systemd paths.

use super::macro_ui::{Draft, MacroUi, Sub};
use super::service::Failure;
use super::theme::Mode;
use super::{App, ButtonRow, Tab, CHOICES, SERVICE};
use eframe::egui;
use razerctl::device::{DpiStages, Info};
use razerctl::macros;
use razerctl::onboard;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);
thread_local! { static RECTS: RefCell<HashMap<String, egui::Rect>> = RefCell::new(HashMap::new()); }

/// Remember a widget's rect under `key` so `RAZERCTL_GUI_POINTER` can aim at it. No-op unless a hook is active.
pub fn note(key: &str, resp: &egui::Response) {
    if ENABLED.load(Ordering::Relaxed) {
        RECTS.with(|r| r.borrow_mut().insert(key.to_string(), resp.rect));
    }
}

#[derive(Default)]
pub struct Demo {
    pub theme: Option<Mode>,
    /// (output dir, tab index, frames spent on this tab)
    snapshot: Option<(PathBuf, usize, u32)>,
    tabs: Vec<usize>,
    scenario: Option<String>,
    pointer: Option<(String, String)>,
}

pub const TABS: [(Tab, &str); 4] = [
    (Tab::Buttons, "buttons"),
    (Tab::Device, "device"),
    (Tab::Software, "software"),
    (Tab::App, "app"),
];

/// Any `RAZERCTL_GUI_*` hook set: the app then never starts the tray (the hooks close the window themselves, and a
/// screenshot run must not talk to D-Bus).
pub fn hooks_active() -> bool {
    std::env::vars_os().any(|(k, _)| k.to_string_lossy().starts_with("RAZERCTL_GUI_"))
}

impl Demo {
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let d = Demo {
            theme: var("RAZERCTL_GUI_THEME").map(|t| {
                if t == "light" {
                    Mode::Light
                } else {
                    Mode::Dark
                }
            }),
            snapshot: std::env::var_os("RAZERCTL_GUI_SNAPSHOT").map(|d| (PathBuf::from(d), 0, 0)),
            tabs: match var("RAZERCTL_GUI_TABS") {
                Some(t) => t
                    .split(',')
                    .filter_map(|n| TABS.iter().position(|(_, name)| *name == n))
                    .collect(),
                None => (0..TABS.len()).collect(),
            },
            scenario: var("RAZERCTL_GUI_DEMO"),
            pointer: var("RAZERCTL_GUI_POINTER").and_then(|p| {
                p.split_once(':')
                    .map(|(a, b)| (a.to_string(), b.to_string()))
            }),
        };
        if d.theme.is_some() || d.snapshot.is_some() || d.scenario.is_some() || d.pointer.is_some()
        {
            ENABLED.store(true, Ordering::Relaxed);
        }
        d
    }
}

pub fn window_size() -> Option<(f32, f32)> {
    let s = std::env::var("RAZERCTL_GUI_SIZE").ok()?;
    let (w, h) = s.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

pub fn info(battery: u8, charging: bool) -> Info {
    Info {
        name: "Razer Viper Mini Signature Edition (Wireless)".into(),
        firmware: Some("v1.04.00".into()),
        serial: Some("PM0000H00000".into()),
        dpi: Some((800, 800)),
        dpi_stages: Some(DpiStages {
            active: 2,
            stages: [400, 800, 1600, 3200, 6400]
                .iter()
                .map(|x| (*x, *x))
                .collect(),
        }),
        poll_rate: Some(1000),
        battery_pct: Some(battery),
        charging: Some(charging),
    }
}

pub fn rows() -> Vec<ButtonRow> {
    [1u8, 2, 3, 4, 5, 0x60, 9, 10]
        .iter()
        .map(|id| {
            ButtonRow::from_assignment(
                onboard::parse_action(*id, "default").expect("default action"),
            )
        })
        .collect()
}

fn choice(label: &str) -> usize {
    CHOICES
        .iter()
        .position(|(l, _)| *l == label)
        .expect("known choice")
}

/// Scenarios: with-device ones fill `info` / `rows`; the others leave the app exactly as a launch on a machine with
/// no mouse looks. `save-error-real`, `config-error` and `service-error` then drive the app's own code paths.
pub fn apply(app: &mut App) {
    let Some(name) = app.demo.scenario.clone() else {
        return;
    };
    let with_device = !name.starts_with("nodevice") && name != "macro-nodevice";
    if with_device {
        app.info = Some(info(82, false));
        app.backend_kind = "hidraw".into();
        app.rows = rows();
        app.profile = 1;
        app.dpi_edit = 800;
        app.poll_edit = 1000;
        app.service_state = "inactive".into();
        app.batt_service = "active".into();
        app.batt_cfg = razerctl::battery::Config {
            enabled: true,
            threshold: 20,
            interval_min: 5,
        };
        app.ok("Read button assignments from the mouse.");
    }
    match name.as_str() {
        "mixed" | "save-error-real" | "save-error-asleep" => {
            // one saved non-default (Back = ctrl+z), three pending edits (Middle, Forward = invalid key, DPI)
            app.rows[3] =
                ButtonRow::from_assignment(onboard::parse_action(4, "key:ctrl+z").expect("action"));
            app.rows[2].choice = choice("Double click");
            app.rows[4].choice = CHOICES.len() - 1;
            app.rows[4].key_text = "f99".into();
            app.rows[5].choice = choice("DPI cycle down");
            app.selected = Some(5);
            match name.as_str() {
                "save-error-real" => {
                    app.rows[4].key_text = "f13".into();
                    // Real save path. Only ever against no hardware (run it with /sys/class/hidraw hidden), because
                    // with a mouse attached it would write these synthetic assignments to it.
                    if super::open_hidraw().is_ok() {
                        app.err("save-error-real skipped: a Razer mouse is attached and would be written to.");
                    } else {
                        app.save_buttons();
                    }
                }
                "save-error-asleep" => {
                    // what save_buttons() reports when the mouse goes to sleep mid-write on Back
                    app.rows[3].choice = choice("Disabled");
                    app.rows[4].key_text = "f13".into();
                    app.failed = Some(4);
                    app.selected = Some(4);
                    app.err("Back (side, rear): The mouse is asleep. Move it or click, then press Refresh.");
                }
                _ => {}
            }
        }
        "clean" => app.selected = Some(2),
        "charging" => {
            app.info = Some(info(14, true));
            app.ok("Low-battery alerts on: you'll be notified at 20% or below.");
        }
        "alertsoff" => {
            app.batt_cfg.enabled = false;
            app.batt_service = "inactive".into();
            app.ok("Low-battery alerts off.");
        }
        "software" => {
            app.conf_text = "# One rule per line:  BUTTON = ACTION\nback = ctrl+c\nforward = KEY_F13\nmiddle = disable\n".into();
            app.mac = MacroUi::from_macros(&macros::parse_macros(
                "[copy]\ndown ctrl\ndelay 20\ntap c\nup ctrl\n\n[scroll-3]\nwheel 1\ndelay 15\nwheel 1\ndelay 15\nwheel 1\n",
            ).expect("demo macros"));
            app.service_state = "active".into();
            app.ok("Config saved. Restart the service to apply.");
        }
        "config-error" => {
            app.conf_text = "# One rule per line:  BUTTON = ACTION\nback = ctrl+c\nforward KEY_F13\nmiddle = disable\n".into();
        }
        "service-error" => {
            app.conf_text = "# One rule per line:  BUTTON = ACTION\nback = ctrl+c\n".into();
            app.systemctl(&["restart", SERVICE]); // real systemctl --user call
        }
        n if n.starts_with("macro-") => macro_scenario(app, n),
        // the App tab of DAN-1452: tray found / no host / Quit selected; battery.conf and gui.conf are not touched
        n if n.starts_with("app") => {
            app.tab = Tab::App;
            app.service_state = "active".into();
            app.settings = super::tray::Settings {
                close_to_tray: n != "app-quit",
                notified: true,
            };
            app.tray = Some(super::tray::Host::fake(n != "app-notray"));
            app.watching = true;
            app.presses.lock().unwrap().extend(
                [
                    "BTN_SIDE",
                    "BTN_LEFT",
                    "BTN_LEFT",
                    "BTN_EXTRA",
                    "BTN_RIGHT",
                    "BTN_MIDDLE",
                ]
                .map(String::from),
            );
            app.ok(if n == "app-notray" {
                "No system tray found. Closing the window will quit razerctl."
            } else if n == "app-quit" {
                "Closing the window will quit razerctl."
            } else {
                "Closing the window will minimise razerctl to the tray."
            });
        }
        _ => {}
    }
}

const MACRO_RULES: &str = "# One rule per line:  BUTTON = ACTION\nback = macro:wiggle,toggle\nforward = macro:rapid-click,held\nmiddle = macro:paste-enter,once\n";

const MACRO_FILE: &str = "\
[rapid-click]
down BTN_LEFT
delay 12
up BTN_LEFT
delay 38

[paste-enter]
down KEY_LEFTCTRL
down KEY_V
delay 30
up KEY_V
up KEY_LEFTCTRL
delay 120
down KEY_ENTER
delay 20
up KEY_ENTER

[wiggle]
move 12 0
delay 60
move -12 0
delay 60

[wheel-flick]
wheel -3
delay 25
wheel -3
delay 25
wheel -3
";

/// The macro screens of the DAN-1426 design, filled with the same synthetic macros and rules. The editor and the
/// rules text are the real ones; only what they were loaded from is made up, and the two config files are pointed
/// into a temp dir so nothing the screens do can touch the real config. The `playing` screen is not here: nothing
/// reports a running macro to the GUI.
fn macro_scenario(app: &mut App, name: &str) {
    let dir = std::env::temp_dir().join("razerctl-gui-demo/.config/razerctl");
    app.conf_path = dir.join("remap.conf");
    app.macros_path = dir.join("macros.conf");
    app.conf_text = MACRO_RULES.into();
    app.service_state = "active".into();
    app.tab = Tab::Software;
    // the tester would open the real input nodes: show a made-up trail instead
    app.watching = true;
    app.presses.lock().unwrap().extend(
        [
            "BTN_SIDE",
            "BTN_LEFT",
            "BTN_LEFT",
            "BTN_EXTRA",
            "BTN_RIGHT",
            "BTN_MIDDLE",
        ]
        .map(String::from),
    );
    app.mac = MacroUi::from_macros(&macros::parse_macros(MACRO_FILE).expect("demo macros"));
    let pos = |app: &App, n: &str| {
        app.mac
            .drafts
            .iter()
            .position(|d| d.name == n)
            .expect("demo macro")
    };
    app.mac.sub = Sub::Macros;
    app.mac.bind.macro_name = "rapid-click".into();
    app.mac.bind.mode = 2;
    let saved = "Macros saved to ~/.config/razerctl/macros.conf. Restart the service to apply.";
    match name {
        "macro-rules" => {
            app.mac.sub = Sub::Rules;
            app.ok("Config saved. Restart the service to apply.");
        }
        "macro-editor" => {
            app.mac.sel = pos(app, "paste-enter");
            app.mac.sel_ev = 2;
            app.ok(saved);
        }
        "macro-empty" => {
            app.mac = MacroUi::empty();
            app.mac.sub = Sub::Macros;
            app.conf_text = "# One rule per line:  BUTTON = ACTION\nback = ctrl+c\n".into();
            app.ok("Read 0 macros from ~/.config/razerctl/macros.conf.");
        }
        "macro-invalid" => {
            use super::macro_ui::Ev::*;
            app.mac.push(Draft::new(
                "shift-select",
                vec![
                    Key("KEY_LEFTSHIFT".into(), true),
                    Key("KEY_END".into(), true),
                    Delay(20),
                    Key("KEY_END".into(), false),
                ],
            ));
            app.mac.push(Draft::new("untitled-1", vec![]));
            app.mac.sel = pos(app, "shift-select");
            app.mac.sel_ev = 0;
            app.err("2 macros have problems. Fix them to enable saving.");
        }
        "macro-dangling" => {
            app.mac.sub = Sub::Rules;
            app.mac.sel = pos(app, "wiggle");
            app.mac.delete(false);
            app.err("Macro 'wiggle' was deleted but the rule for BACK still uses it.");
        }
        "macro-delete" => {
            app.mac.sel = pos(app, "wiggle");
            app.mac.confirm_delete = true;
            app.ok("Nothing is deleted until you press Save macros.");
        }
        "macro-stopped" => {
            app.mac.sel = pos(app, "paste-enter");
            app.mac.sel_ev = 2;
            app.service_state = "inactive".into();
            app.ok(
                "Macros saved to ~/.config/razerctl/macros.conf. The remap service is not running.",
            );
        }
        "macro-svcfail" => {
            // what the board saw on DAN-1433: rules saved, restart pressed, the unit crash-looping on an old binary,
            // and this window on an XDG_CONFIG_HOME the service does not read
            let f = Failure {
                state: "activating (auto-restart), exit status 1, 3 restarts".into(),
                line: Some("razerctl: no remap rules".into()),
            };
            app.mac.sub = Sub::Rules;
            app.service_state = "activating".into();
            app.svc_conf = Some(PathBuf::from("/home/demo/.config/razerctl/remap.conf"));
            app.err(format!("The remap service failed: {}", f.full()));
            app.svc_failure = Some(f);
        }
        "macro-svcold" => {
            app.mac.sub = Sub::Macros;
            app.mac.sel = pos(app, "paste-enter");
            app.service_state = "inactive".into();
            app.err("Macros saved, but the service was not restarted. The installed razerctl at ~/.local/bin/razerctl is too old for macros (it has no `macros` command); reinstall it with ./install.sh.");
        }
        "macro-stopall" => {
            app.mac.sel = pos(app, "rapid-click");
            app.mac.sel_ev = 0;
            app.ok("Stopped all macros. Held keys and buttons were released.");
        }
        "macro-nodevice" => {
            app.mac.sel = pos(app, "paste-enter");
            app.mac.sel_ev = 2;
            app.presses.lock().unwrap().clear();
            app.err("No Razer mouse found. Is the receiver plugged in?");
        }
        _ => {}
    }
}

/// Snapshot driver: per tab, wait a few frames, request a screenshot, save it.
pub fn step(app: &mut App, ctx: &egui::Context) {
    let Some((dir, idx, frames)) = app.demo.snapshot.clone() else {
        return;
    };
    let tab = app.demo.tabs.get(idx).copied();
    let Some(tab) = tab else {
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        return;
    };
    let shot = ctx.input(|i| {
        i.raw.events.iter().find_map(|e| match e {
            egui::Event::Screenshot { image, .. } => Some(image.clone()),
            _ => None,
        })
    });
    if let Some(img) = shot {
        let _ = save_png(&dir.join(format!("{}.png", TABS[tab].1)), &img);
        if idx + 1 >= app.demo.tabs.len() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        app.demo.snapshot = Some((dir, idx + 1, 0));
    } else {
        app.tab = TABS[tab].0;
        // synthetic pointer scenarios wait longer: popups fade in over ~100 ms
        let settle = std::env::var("RAZERCTL_GUI_SETTLE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(if app.demo.pointer.is_some() { 120 } else { 10 });
        if frames == settle {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        app.demo.snapshot = Some((dir, idx, frames + 1));
    }
    ctx.request_repaint();
}

/// Synthetic pointer: move onto the target, optionally press (held) or click it.
pub fn inject(app: &mut App, _ctx: &egui::Context, raw: &mut egui::RawInput) {
    let (Some((kind, key)), Some((_, _, frames))) =
        (app.demo.pointer.clone(), app.demo.snapshot.as_ref())
    else {
        return;
    };
    let Some(rect) = RECTS.with(|r| r.borrow().get(&key).copied()) else {
        return;
    };
    let pos = rect.center();
    let button = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    };
    match (*frames, kind.as_str()) {
        (4, _) => raw.events.push(egui::Event::PointerMoved(pos)),
        (6, "press" | "click") => raw.events.push(button(true)),
        (7, "click") => raw.events.push(button(false)),
        _ => {}
    }
}

fn save_png(path: &std::path::Path, img: &egui::ColorImage) -> std::io::Result<()> {
    // minimal uncompressed (stored-deflate) PNG writer, no extra deps
    fn crc32(data: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB88320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, ty: &[u8], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut c = ty.to_vec();
        c.extend_from_slice(data);
        out.extend_from_slice(&c);
        out.extend_from_slice(&crc32(&c).to_be_bytes());
    }
    let [w, h] = img.size;
    let mut raw = Vec::with_capacity((w * 4 + 1) * h);
    for y in 0..h {
        raw.push(0);
        for p in &img.pixels[y * w..(y + 1) * w] {
            raw.extend_from_slice(&p.to_array());
        }
    }
    let mut z = vec![0x78, 0x01];
    let (mut a, mut b) = (1u32, 0u32);
    for x in &raw {
        a = (a + *x as u32) % 65521;
        b = (b + a) % 65521;
    }
    let mut chunks = raw.chunks(65535).peekable();
    while let Some(c) = chunks.next() {
        z.push(if chunks.peek().is_none() { 1 } else { 0 });
        z.extend_from_slice(&(c.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(c.len() as u16)).to_le_bytes());
        z.extend_from_slice(c);
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    std::fs::write(path, out)
}
