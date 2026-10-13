//! razerctl-gui — desktop front-end for razerctl (egui/eframe, pure Rust).
//!
//! Tabs:
//!   Buttons   – remaps stored in the mouse's onboard memory (work with no software running)
//!   Device    – battery, firmware, DPI, polling rate
//!   Software  – optional computer-side remapper (uinput) config + systemd service
//! A live "button tester" at the bottom shows what the computer actually receives.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use eframe::egui;
use razerctl::device::{self, Backend, Info, Prefer};
use razerctl::hidraw::Hidraw;
use razerctl::onboard::{self, Assignment, Onboard, MODE_NORMAL};
use razerctl::{battery, remap};

const TID: u8 = 0x1f;
const SERVICE: &str = "razerctl-remap.service";

/// Choices offered in the per-button dropdown: (label, action string). `None` = custom key.
const CHOICES: &[(&str, Option<&str>)] = &[
    ("Factory default", Some("default")),
    ("Disabled", Some("disable")),
    ("Left click", Some("left-click")),
    ("Right click", Some("right-click")),
    ("Middle click", Some("middle-click")),
    ("Back", Some("back")),
    ("Forward", Some("forward")),
    ("Scroll up", Some("wheel-up")),
    ("Scroll down", Some("wheel-down")),
    ("Double click", Some("double-click")),
    ("DPI up", Some("dpi-up")),
    ("DPI down", Some("dpi-down")),
    ("DPI cycle up", Some("dpi-cycle-up")),
    ("DPI cycle down", Some("dpi-cycle-down")),
    ("Keyboard key…", None),
];

struct ButtonRow {
    id: u8,
    current: Assignment,
    choice: usize,
    key_text: String,
}

impl ButtonRow {
    fn from_assignment(a: Assignment) -> Self {
        let s = if a.is_default() { Some("default".to_string()) } else { a.action_string() };
        let (choice, key_text) = match s.as_deref() {
            Some(k) if k.starts_with("key:") => (CHOICES.len() - 1, k[4..].to_string()),
            Some(s) => (CHOICES.iter().position(|(_, v)| *v == Some(s)).unwrap_or(0), String::new()),
            None => (0, String::new()),
        };
        ButtonRow { id: a.button, current: a, choice, key_text }
    }
    fn wanted_action(&self) -> String {
        match CHOICES[self.choice].1 {
            Some(s) => s.to_string(),
            None => format!("key:{}", self.key_text.trim().to_lowercase()),
        }
    }
    fn dirty(&self) -> bool {
        let want = self.wanted_action();
        match onboard::parse_action(self.id, &want) {
            Ok(a) => a.fn_id != self.current.fn_id || a.data[..a.size as usize] != self.current.data[..a.size as usize],
            Err(_) => true,
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Tab { Buttons, Device, Software }

struct App {
    tab: Tab,
    status: String,
    status_is_error: bool,
    info: Option<Info>,
    backend_kind: String,
    dpi_edit: u16,
    poll_edit: u32,
    profile: u8,
    rows: Vec<ButtonRow>,
    conf_path: PathBuf,
    conf_text: String,
    conf_error: Option<String>,
    service_state: String,
    presses: Arc<Mutex<VecDeque<String>>>,
    watching: bool,
    batt_cfg: battery::Config,
    batt_service: String,
    /// Test hook: RAZERCTL_GUI_SNAPSHOT=<dir> renders each tab, saves PNGs, exits.
    snapshot: Option<(PathBuf, usize, u32)>,
}

fn open_hidraw() -> Result<Hidraw, String> {
    let devs = device::open_all(Prefer::Hidraw, TID).map_err(|e| e.to_string())?;
    let (node, _) = devs.into_iter().next().ok_or("No Razer mouse found. Is the receiver plugged in?")?;
    Hidraw::open(&node.dev).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            format!("No permission for {} — install udev/99-razerctl.rules (see README)", node.dev.display())
        } else {
            e.to_string()
        }
    })
}

fn open_backend() -> Result<Box<dyn Backend>, String> {
    let devs = device::open_all(Prefer::Auto, TID).map_err(|e| e.to_string())?;
    let (_, b) = devs.into_iter().next().ok_or("No Razer mouse found. Is the receiver plugged in?")?;
    b.map_err(|e| e.to_string())
}

fn sleep_hint(e: impl ToString) -> String {
    let s = e.to_string();
    if s.contains("asleep") || s.contains("0x04") {
        "The mouse is asleep. Move it or click, then press Refresh.".into()
    } else {
        s
    }
}

impl App {
    fn new() -> Self {
        let conf_path = remap::config_path();
        let conf_text = std::fs::read_to_string(&conf_path).unwrap_or_else(|_| {
            "# One rule per line:  BUTTON = ACTION\n# back = ctrl+c\n# forward = KEY_F13\n# middle = disable\n".into()
        });
        let mut app = App {
            tab: Tab::Buttons,
            status: String::new(),
            status_is_error: false,
            info: None,
            backend_kind: String::new(),
            dpi_edit: 800,
            poll_edit: 1000,
            profile: 1,
            rows: Vec::new(),
            conf_path,
            conf_text,
            conf_error: None,
            service_state: String::new(),
            presses: Arc::new(Mutex::new(VecDeque::new())),
            watching: false,
            batt_cfg: battery::load(),
            batt_service: String::new(),
            snapshot: std::env::var_os("RAZERCTL_GUI_SNAPSHOT").map(|d| (PathBuf::from(d), 0, 0)),
        };
        app.refresh_all();
        app.refresh_service();
        app
    }

    fn ok(&mut self, s: impl Into<String>) { self.status = s.into(); self.status_is_error = false; }
    fn err(&mut self, s: impl Into<String>) { self.status = s.into(); self.status_is_error = true; }

    fn refresh_all(&mut self) {
        self.refresh_device();
        self.refresh_buttons();
    }

    fn refresh_device(&mut self) {
        match open_backend() {
            Ok(b) => {
                let info = b.info();
                self.backend_kind = b.kind().to_string();
                if let Some((x, _)) = info.dpi { self.dpi_edit = x; }
                if let Some(p) = info.poll_rate { self.poll_edit = p; }
                self.info = Some(info);
            }
            Err(e) => self.err(e),
        }
    }

    fn refresh_buttons(&mut self) {
        let r = (|| -> Result<(u8, Vec<ButtonRow>), String> {
            let h = open_hidraw()?;
            let ob = Onboard { dev: &h, tid: TID };
            let profile = *ob.profile_ids().map_err(sleep_hint)?.first().ok_or("mouse reports no onboard profile")?;
            let ids = ob.button_ids().map_err(sleep_hint)?;
            let rows = ob.assignments(profile, &ids, MODE_NORMAL).map_err(sleep_hint)?
                .into_iter().map(ButtonRow::from_assignment).collect();
            Ok((profile, rows))
        })();
        match r {
            Ok((p, rows)) => {
                self.profile = p;
                self.rows = rows;
                self.ok("Read button assignments from the mouse.");
            }
            Err(e) => self.err(e),
        }
    }

    fn save_buttons(&mut self) {
        let r = (|| -> Result<usize, String> {
            let h = open_hidraw()?;
            let ob = Onboard { dev: &h, tid: TID };
            let mut n = 0;
            for row in self.rows.iter_mut().filter(|r| r.id != 1) {
                if !row.dirty() { continue; }
                let want = onboard::parse_action(row.id, &row.wanted_action())
                    .map_err(|e| format!("{}: {e}", onboard::button_name(row.id)))?;
                let got = ob.set_assignment(self.profile, MODE_NORMAL, &want)
                    .map_err(|e| format!("{}: {}", onboard::button_name(row.id), sleep_hint(e)))?;
                row.current = got;
                n += 1;
            }
            Ok(n)
        })();
        match r {
            Ok(0) => self.ok("Nothing to save."),
            Ok(n) => self.ok(format!("Saved {n} button(s) to the mouse and verified them. They work without this app.")),
            Err(e) => self.err(e),
        }
    }

    fn reset_all_buttons(&mut self) {
        for row in self.rows.iter_mut() { row.choice = 0; }
        self.save_buttons();
    }

    fn refresh_service(&mut self) {
        let state = |unit: &str| match Command::new("systemctl").args(["--user", "is-active", unit]).output() {
            Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            Err(e) => format!("systemctl unavailable: {e}"),
        };
        self.service_state = state(SERVICE);
        self.batt_service = state(battery::SERVICE);
    }

    /// Save battery settings, and start/stop the background notifier to match the toggle.
    fn apply_battery_settings(&mut self) {
        if let Err(e) = battery::save(&self.batt_cfg) {
            self.err(format!("couldn't save battery settings: {e}"));
            return;
        }
        let args: &[&str] = if self.batt_cfg.enabled {
            &["enable", "--now", battery::SERVICE]
        } else {
            &["disable", "--now", battery::SERVICE]
        };
        match Command::new("systemctl").arg("--user").args(args).output() {
            Ok(o) if o.status.success() => self.ok(if self.batt_cfg.enabled {
                format!("Low-battery alerts on: you'll be notified at {}% or below.", self.batt_cfg.threshold)
            } else {
                "Low-battery alerts off.".to_string()
            }),
            Ok(o) => self.err(format!(
                "Settings saved, but the notifier service didn't {}: {} (run ./install.sh to install it)",
                args[0], String::from_utf8_lossy(&o.stderr).trim()
            )),
            Err(e) => self.err(e.to_string()),
        }
        self.refresh_service();
    }

    fn systemctl(&mut self, args: &[&str]) {
        match Command::new("systemctl").arg("--user").args(args).output() {
            Ok(o) if o.status.success() => self.ok(format!("systemctl {} ok", args.join(" "))),
            Ok(o) => self.err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
            Err(e) => self.err(e.to_string()),
        }
        self.refresh_service();
    }

    fn start_watch(&mut self, ctx: &egui::Context) {
        if self.watching { return; }
        let sources = match remap::find_sources(None) {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => { self.err("No Razer input devices found"); return; }
            Err(e) => { self.err(e.to_string()); return; }
        };
        self.watching = true;
        for s in sources {
            let log = self.presses.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let Ok(mut d) = evdev::raw_stream::RawDevice::open(&s.path) else { return };
                while let Ok(evs) = d.fetch_events() {
                    for ev in evs {
                        if ev.event_type() == evdev::EventType::KEY && ev.value() == 1 {
                            let mut l = log.lock().unwrap();
                            l.push_front(format!("{:?}", evdev::KeyCode::new(ev.code())));
                            l.truncate(12);
                            ctx.request_repaint();
                        }
                    }
                }
            });
        }
    }

    // ------------------------------------------------------------------ UI

    fn ui_buttons(&mut self, ui: &mut egui::Ui) {
        ui.heading("Onboard button mapping");
        ui.label(format!(
            "Saved into the mouse's own memory (profile {}). Works on any computer, with no software running.",
            self.profile
        ));
        ui.add_space(8.0);
        if self.rows.is_empty() {
            ui.label("No button data yet. Wake the mouse and press Refresh.");
        }
        egui::Grid::new("buttons").num_columns(4).spacing([16.0, 8.0]).striped(true).show(ui, |ui| {
            ui.strong("Button"); ui.strong("Currently on mouse"); ui.strong("Change to"); ui.label(""); ui.end_row();
            for row in &mut self.rows {
                ui.label(pretty_button(row.id));
                ui.label(if row.current.is_default() { format!("{} (default)", row.current) } else { row.current.to_string() });
                if row.id == 1 {
                    ui.label("locked (keeps you able to click)");
                    ui.label("");
                } else {
                    egui::ComboBox::from_id_salt(("btn", row.id))
                        .width(170.0)
                        .selected_text(CHOICES[row.choice].0)
                        .show_ui(ui, |ui| {
                            for (i, (label, _)) in CHOICES.iter().enumerate() {
                                ui.selectable_value(&mut row.choice, i, *label);
                            }
                        });
                    ui.horizontal(|ui| {
                        if CHOICES[row.choice].1.is_none() {
                            ui.add(egui::TextEdit::singleline(&mut row.key_text).hint_text("e.g. ctrl+c, f13, space").desired_width(150.0));
                            if !row.key_text.trim().is_empty() {
                                if let Err(e) = onboard::parse_action(row.id, &row.wanted_action()) {
                                    ui.colored_label(egui::Color32::from_rgb(230, 90, 90), e);
                                }
                            }
                        }
                        if row.dirty() { ui.colored_label(egui::Color32::from_rgb(240, 180, 60), "unsaved"); }
                    });
                }
                ui.end_row();
            }
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let dirty = self.rows.iter().any(|r| r.id != 1 && r.dirty());
            if ui.add_enabled(dirty, egui::Button::new("💾 Save to mouse")).clicked() { self.save_buttons(); }
            if ui.button("⟲ Reset all to default").clicked() { self.reset_all_buttons(); }
            if ui.button("↻ Refresh").clicked() { self.refresh_buttons(); }
        });
        ui.add_space(4.0);
        ui.small("Keys: letters, digits, f1–f24, enter, esc, tab, space, arrows, home/end/pageup/pagedown, insert/delete; \
                  modifiers ctrl, shift, alt, super (one key + any modifiers).");
    }

    fn ui_device(&mut self, ui: &mut egui::Ui) {
        ui.heading("Device");
        let Some(info) = &self.info else {
            ui.label("No device info. Wake the mouse and press Refresh.");
            if ui.button("↻ Refresh").clicked() { self.refresh_device(); }
            return;
        };
        let opt = |v: &Option<String>| v.clone().unwrap_or_else(|| "n/a".into());
        egui::Grid::new("info").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
            ui.label("Name"); ui.label(&info.name); ui.end_row();
            ui.label("Firmware"); ui.label(opt(&info.firmware)); ui.end_row();
            ui.label("Serial"); ui.label(opt(&info.serial)); ui.end_row();
            ui.label("Battery"); ui.label(match (info.battery_pct, info.charging) {
                (Some(p), Some(true)) => format!("{p}% (charging)"),
                (Some(p), _) => format!("{p}%"),
                _ => "n/a".into(),
            }); ui.end_row();
            ui.label("DPI stages"); ui.label(match &info.dpi_stages {
                Some(s) => s.stages.iter().enumerate()
                    .map(|(i, (x, _))| if i + 1 == s.active as usize { format!("[{x}]") } else { x.to_string() })
                    .collect::<Vec<_>>().join("  "),
                None => "n/a".into(),
            }); ui.end_row();
            ui.label("Connection"); ui.label(&self.backend_kind); ui.end_row();
        });
        ui.add_space(10.0);
        let mut set_dpi = None;
        let mut set_poll = None;
        ui.horizontal(|ui| {
            ui.label("DPI");
            ui.add(egui::DragValue::new(&mut self.dpi_edit).range(100..=30000).speed(50));
            if ui.button("Apply").clicked() { set_dpi = Some(self.dpi_edit); }
            if let Some((x, _)) = info.dpi { ui.weak(format!("current {x}")); }
        });
        ui.horizontal(|ui| {
            ui.label("Polling rate");
            egui::ComboBox::from_id_salt("poll").selected_text(format!("{} Hz", self.poll_edit)).show_ui(ui, |ui| {
                for hz in [125u32, 500, 1000] { ui.selectable_value(&mut self.poll_edit, hz, format!("{hz} Hz")); }
            });
            if ui.button("Apply").clicked() { set_poll = Some(self.poll_edit); }
        });
        if ui.button("↻ Refresh").clicked() { self.refresh_device(); }

        ui.add_space(14.0);
        ui.separator();
        ui.heading("Low-battery notification");
        let mut changed = false;
        changed |= ui.checkbox(&mut self.batt_cfg.enabled, "Send a desktop notification when the battery is low").changed();
        ui.add_enabled_ui(self.batt_cfg.enabled, |ui| {
            ui.horizontal(|ui| {
                ui.label("Notify at or below");
                let r = ui.add(egui::Slider::new(&mut self.batt_cfg.threshold, 5..=50).suffix("%"));
                changed |= r.drag_stopped() || (r.changed() && !r.dragged());
            });
            ui.horizontal(|ui| {
                ui.label("Check every");
                let r = ui.add(egui::DragValue::new(&mut self.batt_cfg.interval_min).range(1..=60).suffix(" min"));
                changed |= r.drag_stopped() || r.lost_focus() || (r.changed() && !r.dragged() && !r.has_focus());
            });
            ui.weak(format!(
                "One alert when it reaches {}%, then a reminder every {} points lower. Resets once it's charging.",
                self.batt_cfg.threshold, battery::REMIND_STEP
            ));
        });
        ui.horizontal(|ui| {
            ui.label("Notifier:");
            let c = if self.batt_service == "active" { egui::Color32::from_rgb(90, 200, 120) } else { ui.visuals().weak_text_color() };
            ui.colored_label(c, if self.batt_service == "active" { "running" } else { self.batt_service.as_str() });
            if ui.button("Send test notification").clicked() {
                let pct = self.info.as_ref().and_then(|i| i.battery_pct).unwrap_or(0);
                let name = self.info.as_ref().map(|i| i.name.clone()).unwrap_or_else(|| "Razer mouse".into());
                match battery::notify(&format!("{name}: test notification"),
                    &format!("Battery is at {pct}%. Low-battery alerts will look like this."), false) {
                    Ok(()) => self.ok("Test notification sent."),
                    Err(e) => self.err(format!("couldn't send notification: {e}")),
                }
            }
        });
        if changed { self.apply_battery_settings(); }

        if let Some(d) = set_dpi {
            match open_backend().and_then(|b| b.set_dpi(d, d).map_err(sleep_hint)) {
                Ok(()) => { self.ok(format!("DPI set to {d}")); self.refresh_device(); }
                Err(e) => self.err(e),
            }
        }
        if let Some(p) = set_poll {
            match open_backend().and_then(|b| b.set_poll_rate(p).map_err(sleep_hint)) {
                Ok(()) => { self.ok(format!("Polling rate set to {p} Hz")); self.refresh_device(); }
                Err(e) => self.err(e),
            }
        }
    }

    fn ui_software(&mut self, ui: &mut egui::Ui) {
        ui.heading("Software remapping (optional)");
        ui.label("Runs on this computer only, while the service is running. Use it for things the mouse \
                  can't store itself, such as multi-key shortcuts on several buttons or any evdev key name.");
        ui.add_space(6.0);
        ui.label(format!("Config: {}", self.conf_path.display()));
        ui.add(egui::TextEdit::multiline(&mut self.conf_text).code_editor().desired_rows(8).desired_width(f32::INFINITY));
        let parsed = remap::parse_config(&self.conf_text);
        match &parsed {
            Ok(m) => { ui.weak(format!("{} rule(s), valid", m.len())); self.conf_error = None; }
            Err(e) => { ui.colored_label(egui::Color32::from_rgb(230, 90, 90), e); self.conf_error = Some(e.clone()); }
        }
        ui.horizontal(|ui| {
            if ui.add_enabled(self.conf_error.is_none(), egui::Button::new("💾 Save config")).clicked() {
                let r = self.conf_path.parent().map(std::fs::create_dir_all).transpose()
                    .and_then(|_| std::fs::write(&self.conf_path, &self.conf_text));
                match r {
                    Ok(_) => self.ok("Config saved. Restart the service to apply."),
                    Err(e) => self.err(e.to_string()),
                }
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label("Service:");
            let c = if self.service_state == "active" { egui::Color32::from_rgb(90, 200, 120) } else { ui.visuals().weak_text_color() };
            ui.colored_label(c, &self.service_state);
        });
        ui.horizontal(|ui| {
            if ui.button("Start / restart").clicked() { self.systemctl(&["restart", SERVICE]); }
            if ui.button("Stop").clicked() { self.systemctl(&["stop", SERVICE]); }
            if ui.button("Enable at login").clicked() { self.systemctl(&["enable", "--now", SERVICE]); }
            if ui.button("Disable").clicked() { self.systemctl(&["disable", "--now", SERVICE]); }
            if ui.button("↻").clicked() { self.refresh_service(); }
        });
    }
}

fn pretty_button(id: u8) -> &'static str {
    match id {
        1 => "Left", 2 => "Right", 3 => "Middle (wheel click)", 4 => "Back (side, rear)",
        5 => "Forward (side, front)", 0x60 => "DPI (underside)", 9 => "Wheel up", 10 => "Wheel down",
        _ => "Other",
    }
}

fn save_png(path: &std::path::Path, img: &egui::ColorImage) -> std::io::Result<()> {
    // minimal uncompressed (stored-deflate) PNG writer, no extra deps
    fn crc32(data: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in data { c ^= b as u32; for _ in 0..8 { c = if c & 1 != 0 { 0xEDB88320 ^ (c >> 1) } else { c >> 1 }; } }
        !c
    }
    fn chunk(out: &mut Vec<u8>, ty: &[u8], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut c = ty.to_vec(); c.extend_from_slice(data);
        out.extend_from_slice(&c);
        out.extend_from_slice(&crc32(&c).to_be_bytes());
    }
    let [w, h] = img.size;
    let mut raw = Vec::with_capacity((w * 4 + 1) * h);
    for y in 0..h { raw.push(0); for p in &img.pixels[y * w..(y + 1) * w] { raw.extend_from_slice(&p.to_array()); } }
    let mut z = vec![0x78, 0x01];
    let (mut a, mut b) = (1u32, 0u32);
    for x in &raw { a = (a + *x as u32) % 65521; b = (b + a) % 65521; }
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
    ihdr.extend_from_slice(&(w as u32).to_be_bytes()); ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr); chunk(&mut out, b"IDAT", &z); chunk(&mut out, b"IEND", &[]);
    std::fs::write(path, out)
}

impl App {
    /// Drive the snapshot test hook: wait a few frames per tab, request a screenshot, save it.
    fn snapshot_step(&mut self, ctx: &egui::Context) {
        let Some((dir, idx, frames)) = self.snapshot.clone() else { return };
        const TABS: [(Tab, &str); 3] = [(Tab::Buttons, "buttons"), (Tab::Device, "device"), (Tab::Software, "software")];
        let shot = ctx.input(|i| i.raw.events.iter().find_map(|e| match e {
            egui::Event::Screenshot { image, .. } => Some(image.clone()),
            _ => None,
        }));
        if let Some(img) = shot {
            let _ = save_png(&dir.join(format!("{}.png", TABS[idx].1)), &img);
            if idx + 1 == TABS.len() { ctx.send_viewport_cmd(egui::ViewportCommand::Close); return; }
            self.snapshot = Some((dir, idx + 1, 0));
        } else {
            self.tab = TABS[idx].0;
            if frames == 6 { ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default())); }
            self.snapshot = Some((dir, idx, frames + 1));
        }
        ctx.request_repaint();
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.snapshot_step(&ctx);
        self.start_watch(&ctx);
        egui::Panel::top("tabs").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("🐍 razerctl");
                ui.separator();
                ui.selectable_value(&mut self.tab, Tab::Buttons, "Buttons");
                ui.selectable_value(&mut self.tab, Tab::Device, "Device");
                ui.selectable_value(&mut self.tab, Tab::Software, "Software remap");
                if let Some(i) = &self.info {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(p) = i.battery_pct { ui.label(format!("🔋 {p}%")); }
                        ui.weak(&i.name);
                    });
                }
            });
            ui.add_space(4.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Button tester:");
                let l = self.presses.lock().unwrap();
                if l.is_empty() { ui.weak("press a mouse button to see what the computer receives"); }
                for (i, p) in l.iter().take(6).enumerate() {
                    if i == 0 { ui.strong(p); } else { ui.weak(p); }
                }
            });
            if !self.status.is_empty() {
                let c = if self.status_is_error { egui::Color32::from_rgb(230, 90, 90) } else { egui::Color32::from_rgb(90, 200, 120) };
                ui.colored_label(c, &self.status);
            }
            ui.add_space(4.0);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                Tab::Buttons => self.ui_buttons(ui),
                Tab::Device => self.ui_device(ui),
                Tab::Software => self.ui_software(ui),
            });
        });
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("razerctl")
            .with_app_id("razerctl")
            .with_inner_size([780.0, 560.0])
            .with_min_inner_size([620.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native("razerctl", options, Box::new(|_cc| Ok(Box::new(App::new()))))
}
