//! razerctl-gui — desktop front-end for razerctl (egui/eframe, pure Rust).
//!
//! Tabs:
//!   Buttons   – remaps stored in the mouse's onboard memory (work with no software running)
//!   Device    – battery, firmware, DPI, polling rate
//!   Software  – optional computer-side remapper (uinput): config, macros (macros.conf) + systemd service
//! A live "button tester" at the bottom shows what the computer actually receives.
//!
//! The look (tokens, fonts, widgets, mouse diagram) lives in `razerctl-gui/`; this file keeps the state, the
//! device / systemctl / config calls and the screen layouts that bind them together.

#[path = "razerctl-gui/demo.rs"]
mod demo;
#[path = "razerctl-gui/mouse.rs"]
mod mouse;
#[path = "razerctl-gui/theme.rs"]
mod theme;
#[path = "razerctl-gui/widgets.rs"]
mod widgets;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use eframe::egui::{self, epaint::RectShape, pos2, vec2, Align2, Color32, Frame, Margin, Rect, RichText, Sense, Shape, Stroke};
use razerctl::device::{self, Backend, Info, Prefer};
use razerctl::hidraw::Hidraw;
use razerctl::macros::{self, Macros};
use razerctl::onboard::{self, Assignment, Onboard, MODE_NORMAL};
use razerctl::{battery, remap};
use theme::*;
use widgets::*;

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

#[derive(PartialEq, Clone, Copy, Debug)]
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
    /// Raw text of macros.conf, edited as text (comments kept on save); `macros_error` is the last parse error.
    macros_path: PathBuf,
    macros_text: String,
    macros_error: Option<String>,
    service_state: String,
    presses: Arc<Mutex<VecDeque<String>>>,
    watching: bool,
    batt_cfg: battery::Config,
    batt_service: String,
    /// Button id of the highlighted table row / mouse-diagram tag.
    selected: Option<u8>,
    /// Button whose write failed in the last `save_buttons`, for the per-row marker.
    failed: Option<u8>,
    /// Palette in use and the mode its egui style was last built for.
    pal: Pal,
    styled: Option<Mode>,
    /// Test hooks (RAZERCTL_GUI_SNAPSHOT / _THEME / _DEMO / _SIZE): see `demo.rs`.
    demo: demo::Demo,
}

fn open_hidraw() -> Result<Hidraw, String> {
    let devs = device::open_all(Prefer::Hidraw, TID).map_err(|e| e.to_string())?;
    let (node, _) = devs.into_iter().next().ok_or("No Razer mouse found. Is the receiver plugged in?")?;
    Hidraw::open(&node.dev).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            format!("No permission for {} — install udev/70-razerctl.rules (see README)", node.dev.display())
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
        let macros_path = macros::macros_path();
        let macros_text = std::fs::read_to_string(&macros_path).unwrap_or_else(|_| MACROS_TEMPLATE.into());
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
            macros_path,
            macros_text,
            macros_error: None,
            service_state: String::new(),
            presses: Arc::new(Mutex::new(VecDeque::new())),
            watching: false,
            batt_cfg: battery::load(),
            batt_service: String::new(),
            selected: None,
            failed: None,
            pal: Pal::new(Mode::Dark),
            styled: None,
            demo: demo::Demo::from_env(),
        };
        app.refresh_all();
        app.refresh_service();
        demo::apply(&mut app);
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
                self.failed = None;
                self.ok("Read button assignments from the mouse.");
            }
            Err(e) => self.err(e),
        }
    }

    fn save_buttons(&mut self) {
        let mut failed = None;
        let r = (|| -> Result<usize, String> {
            let h = open_hidraw()?;
            let ob = Onboard { dev: &h, tid: TID };
            let mut n = 0;
            for row in self.rows.iter_mut().filter(|r| r.id != 1) {
                if !row.dirty() { continue; }
                failed = Some(row.id);
                let want = onboard::parse_action(row.id, &row.wanted_action())
                    .map_err(|e| format!("{}: {e}", onboard::button_name(row.id)))?;
                let got = ob.set_assignment(self.profile, MODE_NORMAL, &want)
                    .map_err(|e| format!("{}: {}", onboard::button_name(row.id), sleep_hint(e)))?;
                failed = None;
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
        self.failed = failed;
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

    /// Write macros.conf as typed (comments kept), only if it parses. Nothing is sent to the mouse: macros are
    /// played by the remap service, which reads the file when it starts.
    fn save_macros(&mut self) {
        if let Err(e) = macros::parse_macros(&self.macros_text) {
            self.err(format!("Macros not saved: {e}"));
            return;
        }
        match write_atomic(&self.macros_path, &self.macros_text) {
            Ok(()) => self.ok("Macros saved. Restart the service to apply."),
            Err(e) => self.err(format!("{}: {e}", self.macros_path.display())),
        }
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

    // ------------------------------------------------------------------ chrome

    /// Brand + tabs left, device identity + battery right. Always visible.
    fn header(&mut self, ui: &mut egui::Ui) {
        let p = self.pal;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = S5;
            ui.label(RichText::new("🐍").font(sans(18.0)).color(p.i100()));
            ui.label(tracked("RAZERCTL", mono_bold(12.0), p.i100(), 0.22));
            let labels: Vec<String> = ["Buttons", "Device", "Software remap"].iter().map(|s| s.to_string()).collect();
            let active = TABS.iter().position(|t| *t == self.tab);
            if let Some(i) = segmented(ui, &p, "tab", &labels, active, 104.0) { self.tab = TABS[i]; }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = S3;
                match &self.info {
                    Some(i) => {
                        if let Some(b) = i.battery_pct {
                            let glyph = if i.charging == Some(true) { "▲" } else { "●" };
                            chip(ui, &p, Some(glyph), &format!("{b}%"), ChipStyle::Outline, p.i100());
                        }
                        // the full name is always in Device > Status > NAME, so it is the first thing to go when narrow
                        if ui.available_width() > 420.0 { ui.label(RichText::new(&i.name).font(sans(T_BODY - 1.0)).color(p.i65())); }
                    }
                    None => { chip(ui, &p, Some("○"), "No device", ChipStyle::Outline, p.i65()); }
                }
            });
        });
    }

    /// Live device readouts, visible on every tab.
    fn device_strip(&self, ui: &mut egui::Ui) {
        let p = self.pal;
        let wide = ui.available_width() > 900.0;
        Frame::new().fill(p.raised).stroke(Stroke::new(1.0, p.i18())).inner_margin(Margin::symmetric(16, 10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = S5;
                ui.label(RichText::new("00 // Device").font(mono(T_LABEL)).color(p.i65()));
                let val = |ui: &mut egui::Ui, label: &str, v: String| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.label(caps(&p, label));
                        ui.label(RichText::new(v).font(mono_bold(T_DATA)).color(p.i100()));
                    });
                };
                let Some(i) = &self.info else {
                    ui.label(RichText::new("— no readings: mouse asleep or receiver unplugged —").font(mono(T_DATA)).color(p.i65()));
                    return;
                };
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(caps(&p, "Battery"));
                    match i.battery_pct {
                        Some(b) => {
                            meter(ui, &p, 72.0, 10.0, b as f32 / 100.0, 10);
                            let charging = if i.charging == Some(true) { " ▲ charging" } else { "" };
                            ui.label(RichText::new(format!("{b}%{charging}")).font(mono_bold(T_DATA)).color(p.i100()));
                        }
                        None => { ui.label(RichText::new("n/a").font(mono_bold(T_DATA)).color(p.i100())); }
                    }
                });
                val(ui, "DPI", i.dpi.map_or("n/a".into(), |(x, _)| x.to_string()));
                val(ui, "Poll", i.poll_rate.map_or("n/a".into(), |r| format!("{r} Hz")));
                if wide { val(ui, "Profile", self.profile.to_string()); }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("● {}", self.backend_kind)).font(mono_bold(T_DATA)).color(p.i100()));
                    if wide { ui.label(caps(&p, "Connection")); }
                });
            });
        });
    }

    /// Live button tester: newest press inverted, the five before it outlined.
    fn tester(&self, ui: &mut egui::Ui) {
        let p = self.pal;
        let presses: Vec<String> = self.presses.lock().unwrap().iter().take(6).cloned().collect();
        ui.spacing_mut().interact_size.y = 20.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = S2;
            ui.label(caps(&p, "Button tester"));
            ui.add_space(S2);
            if presses.is_empty() {
                ui.label(RichText::new("Press a mouse button to see what the computer receives.").font(sans(T_SMALL)).color(p.i65()));
            }
            for (i, s) in presses.iter().enumerate() {
                if i == 0 {
                    chip(ui, &p, None, s, ChipStyle::Stamp, p.stamp_fg);
                } else {
                    chip(ui, &p, None, s, ChipStyle::Outline, p.i65());
                }
            }
        });
    }

    /// One-line status. Success is a quiet `●` in the ok colour; an error inverts the whole bar (it is the only
    /// inverted bar on screen), so it never relies on colour.
    fn status_bar(&self, ui: &mut egui::Ui) {
        let p = self.pal;
        ui.spacing_mut().interact_size.y = 16.0;
        let err = self.status_is_error;
        let (fg, glyph) = if err { (p.stamp_fg, "×") } else { (p.i65(), if self.status.is_empty() { "" } else { "●" }) };
        let text = if self.status.is_empty() { "Ready" } else { self.status.as_str() };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let glyph_c = if err { fg } else { p.ok };
            if !glyph.is_empty() { ui.label(RichText::new(glyph).font(mono(T_LABEL + 1.0)).color(glyph_c)); }
            // long systemctl / OS errors wrap onto more lines instead of running off the window
            ui.add(egui::Label::new(RichText::new(text).font(mono(T_LABEL + 0.5)).color(fg)).wrap());
        });
    }

    // ------------------------------------------------------------------ Buttons

    fn ui_buttons(&mut self, ui: &mut egui::Ui) {
        let p = self.pal;
        let t0 = ui.available_width();
        title(
            ui, &p, "Buttons.",
            &format!("Saved into the mouse's own memory (profile {}). Works on any computer, with no software running.", self.profile),
            (t0 * 0.34).clamp(250.0, 360.0) + S4,
        );
        if self.rows.is_empty() {
            self.buttons_empty(ui);
            return;
        }
        let selected = self.selected_id();
        let total = ui.available_width();
        let left_w = (total * 0.34).clamp(250.0, 360.0);
        let gap = S4;
        let right_w = total - left_w - gap;
        if total < 900.0 {
            // narrow window: the mouse diagram collapses away, the table keeps full width, key syntax drops underneath
            section(ui, &p, "01", "Onboard assignments");
            self.assignments_table(ui, selected);
            ui.add_space(S2);
            section(ui, &p, "02", "Key syntax");
            panel(ui, &p, 12, |ui| key_syntax(ui, &p));
            return;
        }
        let tags: Vec<mouse::Tag> = self.rows.iter().map(|r| mouse::Tag {
            id: r.id,
            name: pretty_button(r.id).0,
            current: r.current.to_string(),
            is_default: r.current.is_default(),
            dirty: r.id != 1 && r.dirty(),
        }).collect();
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            ui.allocate_ui_with_layout(vec2(left_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(left_w);
                section(ui, &p, "01", "Mouse");
                if let Some(id) = mouse::mouse_panel(ui, &p, &tags, selected) { self.selected = Some(id); }
                ui.add_space(S2);
                section(ui, &p, "03", "Key syntax");
                panel(ui, &p, 12, |ui| key_syntax(ui, &p));
            });
            ui.allocate_ui_with_layout(vec2(right_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(right_w);
                section(ui, &p, "02", "Onboard assignments");
                self.assignments_table(ui, selected);
            });
        });
    }

    fn selected_id(&self) -> Option<u8> {
        self.selected.filter(|id| self.rows.iter().any(|r| r.id == *id))
            .or_else(|| self.rows.iter().find(|r| r.id != 1).or(self.rows.first()).map(|r| r.id))
    }

    fn buttons_empty(&mut self, ui: &mut egui::Ui) {
        let p = self.pal;
        section(ui, &p, "01", "Onboard assignments");
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 200.0), Sense::hover());
        ui.painter().add(Shape::Rect(RectShape::new(rect, R0, p.raised, Stroke::new(1.0, p.i18()), egui::StrokeKind::Inside)));
        let inner = rect.shrink2(vec2(S6, S5));
        let mut cui = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::top_down(egui::Align::Min)));
        cui.label(RichText::new("No button data yet.").font(serif(T_TITLE)).color(p.i100()));
        cui.add_space(4.0);
        cui.label(RichText::new("Wake the mouse and press Refresh.").font(sans(T_BODY)).color(p.i65()));
        cui.add_space(S4);
        cui.label(caps(&p, "If it still isn't found"));
        cui.add_space(2.0);
        for l in ["Is the receiver plugged in?", "A permission error means udev/70-razerctl.rules is not installed (see README)."] {
            cui.label(RichText::new(format!("·  {l}")).font(sans(T_SMALL)).color(p.i65()));
        }
        ui.add_space(S3);
        self.action_bar(ui, 0);
        ui.add_space(S3);
        section(ui, &p, "02", "Key syntax");
        panel(ui, &p, 12, |ui| key_syntax(ui, &p));
    }

    fn action_bar(&mut self, ui: &mut egui::Ui, dirty: usize) {
        let p = self.pal;
        let has_rows = !self.rows.is_empty();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = S2;
            let save = btn(ui, &p, "💾  Save to mouse", Kind::Primary, dirty > 0);
            demo::note("save", &save);
            if save.clicked() { self.save_buttons(); }
            if btn(ui, &p, "⟲  Reset all to default", Kind::Secondary, true).clicked() { self.reset_all_buttons(); }
            let refresh = btn(ui, &p, "↻  Refresh", if has_rows { Kind::Secondary } else { Kind::Primary }, true);
            demo::note("refresh", &refresh);
            if refresh.clicked() { self.refresh_buttons(); }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if dirty > 0 {
                    chip(ui, &p, Some("◐"), &format!("{dirty} unsaved"), ChipStyle::Outline, p.warn);
                } else if has_rows {
                    chip(ui, &p, Some("●"), "In sync with the mouse", ChipStyle::Plain, p.i65());
                }
            });
        });
    }

    fn assignments_table(&mut self, ui: &mut egui::Ui, selected: Option<u8>) {
        let p = self.pal;
        let dirty_n = self.rows.iter().filter(|r| r.id != 1 && r.dirty()).count();
        let failed = self.failed;
        let mut new_selected = None;
        Frame::new().fill(p.raised).stroke(Stroke::new(1.0, p.i18())).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            let w = ui.available_width() - S4 * 2.0;
            let cols = [w * 0.27, w * 0.27, w * 0.46];
            ui.add_space(S3);
            ui.horizontal(|ui| {
                ui.add_space(S4);
                for (i, h) in ["Button", "Currently on mouse", "Change to"].iter().enumerate() {
                    ui.allocate_ui_with_layout(vec2(cols[i], 16.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.set_min_width(cols[i]);
                        ui.label(caps(&p, h));
                    });
                }
            });
            ui.add_space(S2);
            hair(ui, &p);
            let row_h = 42.0;
            for (i, row) in self.rows.iter_mut().enumerate() {
                let left = ui.max_rect().left();
                let right = ui.max_rect().right();
                let top = ui.cursor().top();
                let bg = ui.painter().add(Shape::Noop);
                let (name, sub) = pretty_button(row.id);
                let is_sel = selected == Some(row.id);
                let resp = ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                    ui.add_space(S4);
                    // col 1 — which button
                    vcell(ui, cols[0], row_h, if sub.is_empty() { 20.0 } else { 34.0 }, |ui| {
                        ui.label(RichText::new(name).font(sans(T_BODY)).color(p.i100()));
                        if !sub.is_empty() {
                            ui.label(tracked(&sub.to_uppercase(), mono(T_MICRO), p.i65(), 0.12));
                        }
                    });
                    // col 2 — what the mouse has now
                    vcell(ui, cols[1], row_h, 16.0, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            let is_default = row.current.is_default();
                            let c = if is_default { p.i65() } else { p.i100() };
                            ui.label(RichText::new(row.current.to_string()).font(if is_default { mono(T_DATA) } else { mono_bold(T_DATA) }).color(c));
                            if is_default {
                                ui.label(tracked("DEFAULT", mono(T_MICRO), p.i65(), 0.12));
                            }
                        });
                    });
                    // col 3 — change to
                    ui.allocate_ui_with_layout(vec2(cols[2], row_h), egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.set_min_width(cols[2]);
                        ui.spacing_mut().item_spacing.y = 4.0;
                        ui.add_space((row_h - 30.0) / 2.0);
                        if row.id == 1 {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                chip(ui, &p, Some("○"), "Locked", ChipStyle::Outline, p.i65());
                                ui.label(RichText::new("keeps you able to click").font(sans(T_SMALL)).color(p.i65()));
                            });
                        } else {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 10.0;
                                let combo = egui::ComboBox::from_id_salt(("btn", row.id))
                                    .width(170.0)
                                    .selected_text(RichText::new(CHOICES[row.choice].0).font(sans(T_BODY - 1.0)).color(p.i100()))
                                    .show_ui(ui, |ui| {
                                        // compact list: all 15 choices fit without a long scroll
                                        ui.spacing_mut().interact_size.y = 24.0;
                                        ui.spacing_mut().item_spacing.y = 2.0;
                                        for (i, (label, _)) in CHOICES.iter().enumerate() {
                                            ui.selectable_value(&mut row.choice, i, *label);
                                        }
                                    });
                                demo::note(&format!("combo{}", row.id), &combo.response);
                                if failed == Some(row.id) {
                                    chip(ui, &p, Some("×"), "Failed", ChipStyle::Plain, p.bad);
                                } else if row.dirty() {
                                    chip(ui, &p, Some("◐"), "Unsaved", ChipStyle::Plain, p.warn);
                                }
                            });
                            if CHOICES[row.choice].1.is_none() {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 10.0;
                                    ui.add(egui::TextEdit::singleline(&mut row.key_text).hint_text("e.g. ctrl+c, f13, space")
                                        .desired_width(170.0).font(mono(T_DATA)).text_color(p.i100()));
                                    if !row.key_text.trim().is_empty() {
                                        if let Err(e) = onboard::parse_action(row.id, &row.wanted_action()) {
                                            ui.label(RichText::new(format!("× {e}")).font(sans(T_SMALL)).color(p.bad));
                                        }
                                    }
                                });
                                ui.add_space(8.0);
                            }
                        }
                    });
                });
                let bottom = resp.response.rect.bottom().max(top + row_h);
                let rect = Rect::from_min_max(pos2(left, top), pos2(right, bottom));
                if ui.rect_contains_pointer(rect) && ui.input(|i| i.pointer.primary_pressed()) { new_selected = Some(row.id); }
                let fill = if is_sel { p.i10() } else if i % 2 == 1 { p.i06() } else { Color32::TRANSPARENT };
                ui.painter().set(bg, Shape::Rect(RectShape::filled(rect, R0, fill)));
                if is_sel {
                    ui.painter().add(Shape::Rect(RectShape::filled(Rect::from_min_size(rect.min, vec2(3.0, rect.height())), R0, p.stamp_bg)));
                }
                ui.painter().hline(rect.x_range(), bottom, Stroke::new(1.0, p.i18()));
                ui.add_space((bottom - resp.response.rect.bottom()).max(0.0));
            }
            ui.add_space(S3);
            ui.horizontal(|ui| {
                ui.add_space(S4);
                ui.vertical(|ui| {
                    ui.set_width(w);
                    self.action_bar(ui, dirty_n);
                });
            });
            ui.add_space(S3);
        });
        if new_selected.is_some() { self.selected = new_selected; }
    }

    // ------------------------------------------------------------------ Device

    fn ui_device(&mut self, ui: &mut egui::Ui) {
        let p = self.pal;
        let has_device = self.info.is_some();
        title(
            ui, &p, "Device.",
            if has_device { "Live readings and the two settings the mouse keeps for itself: DPI and polling rate." }
            else { "No device info. Wake the mouse and press Refresh." },
            (ui.available_width() * 0.40).clamp(300.0, 440.0) + S4,
        );
        let total = ui.available_width();
        let gap = S4;
        if total < 900.0 {
            // narrow window: one column, status first
            section(ui, &p, "01", "Status");
            self.status_panel(ui);
            ui.add_space(S2);
            self.device_settings(ui);
            return;
        }
        let left_w = (total * 0.40).clamp(300.0, 440.0);
        let right_w = total - left_w - gap;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            ui.allocate_ui_with_layout(vec2(left_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(left_w);
                section(ui, &p, "01", "Status");
                self.status_panel(ui);
            });
            ui.allocate_ui_with_layout(vec2(right_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(right_w);
                self.device_settings(ui);
            });
        });
    }

    fn device_settings(&mut self, ui: &mut egui::Ui) {
        let p = self.pal;
        let has_device = self.info.is_some();
        let mut set_dpi = None;
        let mut set_poll = None;
        if has_device {
            section(ui, &p, "02", "DPI");
            set_dpi = self.dpi_panel(ui);
            ui.add_space(S2);
            section(ui, &p, "03", "Polling rate");
            set_poll = self.poll_panel(ui);
            ui.add_space(S2);
        }
        section(ui, &p, if has_device { "04" } else { "02" }, "Low-battery notification");
        self.battery_panel(ui);

        if let Some(d) = set_dpi {
            match open_backend().and_then(|b| b.set_dpi(d, d).map_err(sleep_hint)) {
                Ok(()) => { self.ok(format!("DPI set to {d}")); self.refresh_device(); }
                Err(e) => self.err(e),
            }
        }
        if let Some(p) = set_poll {
            match open_backend().and_then(|b| b.set_poll_rate(p).map_err(sleep_hint)) {
                Ok(()) => {
                    self.refresh_device();
                    match self.info.as_ref().and_then(|i| i.poll_rate) {
                        Some(now) if now == p => self.ok(format!("Polling rate set to {p} Hz (confirmed by the mouse).")),
                        Some(now) => self.err(format!("The mouse accepted the command but reports {now} Hz, so it doesn't support {p} Hz in its current mode.")),
                        None => self.ok(format!("Polling rate set to {p} Hz.")),
                    }
                }
                Err(e) => self.err(e),
            }
        }
    }

    fn status_panel(&mut self, ui: &mut egui::Ui) {
        let p = self.pal;
        let mut refresh = false;
        panel(ui, &p, 16, |ui| {
            let Some(info) = &self.info else {
                ui.add_space(S2);
                ui.label(RichText::new("No device info.").font(serif(T_TITLE)).color(p.i100()));
                ui.label(RichText::new("Wake the mouse and press Refresh.").font(sans(T_BODY)).color(p.i65()));
                ui.add_space(S4);
                refresh = btn(ui, &p, "↻  Refresh", Kind::Primary, true).clicked();
                return;
            };
            // hero: battery
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                match info.battery_pct {
                    Some(b) => {
                        ui.label(RichText::new(b.to_string()).font(serif(T_NUMERAL + 12.0)).color(p.i100()));
                        ui.label(RichText::new("%").font(serif(T_TITLE)).color(p.i65()));
                    }
                    None => { ui.label(RichText::new("n/a").font(serif(T_NUMERAL + 12.0)).color(p.i65())); }
                }
                ui.add_space(S3);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    ui.add_space(10.0);
                    ui.label(caps(&p, "Battery"));
                    if info.charging == Some(true) {
                        chip(ui, &p, Some("▲"), "Charging", ChipStyle::Stamp, p.stamp_fg);
                    } else if info.battery_pct.is_some() {
                        chip(ui, &p, Some("●"), "On battery", ChipStyle::Plain, p.i65());
                    }
                    if self.batt_cfg.enabled && info.battery_pct.is_some_and(|b| b <= self.batt_cfg.threshold) {
                        chip(ui, &p, Some("▲"), &format!("Low · alert at {}%", self.batt_cfg.threshold), ChipStyle::Outline, p.warn);
                    }
                });
            });
            ui.add_space(S2);
            let bw = ui.available_width();
            meter(ui, &p, bw, 14.0, info.battery_pct.unwrap_or(0) as f32 / 100.0, 20);
            ui.add_space(S4);
            hair(ui, &p);
            ui.add_space(S2);
            let lw = 112.0;
            let opt = |v: &Option<String>| v.clone().unwrap_or_else(|| "n/a".into());
            kv(ui, &p, "Name", lw, |ui| { ui.add(egui::Label::new(RichText::new(&info.name).font(sans(T_BODY - 1.0)).color(p.i100())).wrap()); });
            kv(ui, &p, "Firmware", lw, |ui| { ui.label(RichText::new(opt(&info.firmware)).font(mono(T_DATA + 1.0)).color(p.i100())); });
            kv(ui, &p, "Serial", lw, |ui| { ui.label(RichText::new(opt(&info.serial)).font(mono(T_DATA + 1.0)).color(p.i100())); });
            kv(ui, &p, "Connection", lw, |ui| { ui.label(RichText::new(&self.backend_kind).font(mono(T_DATA + 1.0)).color(p.i100())); });
            ui.add_space(S2);
            hair(ui, &p);
            ui.add_space(S3);
            ui.label(caps(&p, "DPI stages"));
            ui.add_space(S1);
            match &info.dpi_stages {
                Some(s) => {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        for (i, (x, _)) in s.stages.iter().enumerate() {
                            if i + 1 == s.active as usize {
                                chip(ui, &p, None, &x.to_string(), ChipStyle::Stamp, p.stamp_fg);
                            } else {
                                chip(ui, &p, None, &x.to_string(), ChipStyle::Outline, p.i65());
                            }
                        }
                    });
                    ui.add_space(S1);
                    ui.label(RichText::new("Filled = active stage.").font(sans(T_MICRO + 1.0)).color(p.i65()));
                }
                None => { ui.label(RichText::new("n/a").font(mono(T_DATA + 1.0)).color(p.i100())); }
            }
            ui.add_space(S4);
            refresh = btn(ui, &p, "↻  Refresh", Kind::Secondary, true).clicked();
        });
        if refresh { self.refresh_device(); }
    }

    /// Returns the DPI to apply, if Apply was clicked.
    fn dpi_panel(&mut self, ui: &mut egui::Ui) -> Option<u16> {
        let p = self.pal;
        let current = self.info.as_ref().and_then(|i| i.dpi).map(|(x, _)| x);
        let mut set = None;
        panel(ui, &p, 16, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = S3;
                ui.set_min_height(60.0);
                ui.label(caps(&p, "DPI"));
                Frame::new().fill(p.sunken).stroke(Stroke::new(1.0, p.i45())).inner_margin(Margin::symmetric(10, 2)).show(ui, |ui| {
                    ui.scope(|ui| {
                        ui.style_mut().override_font_id = Some(serif(T_NUMERAL - 4.0));
                        let w = &mut ui.style_mut().visuals.widgets;
                        w.inactive.fg_stroke = Stroke::new(1.0, p.i100());
                        w.inactive.bg_fill = Color32::TRANSPARENT;
                        w.inactive.weak_bg_fill = Color32::TRANSPARENT;
                        w.inactive.bg_stroke = Stroke::NONE;
                        ui.add(egui::DragValue::new(&mut self.dpi_edit).range(100..=30000).speed(50));
                    });
                });
                let differs = current != Some(self.dpi_edit);
                if btn(ui, &p, "Apply", if differs { Kind::Primary } else { Kind::Secondary }, true).clicked() { set = Some(self.dpi_edit); }
                let cur = current.map_or(String::new(), |x| format!("current {x}  ·  "));
                ui.label(RichText::new(format!("{cur}drag or type, 100–30000")).font(mono(T_DATA)).color(p.i65()));
            });
        });
        set
    }

    /// Returns the polling rate to apply, if Apply was clicked.
    fn poll_panel(&mut self, ui: &mut egui::Ui) -> Option<u32> {
        let p = self.pal;
        let current = self.info.as_ref().and_then(|i| i.poll_rate);
        let mut set = None;
        panel(ui, &p, 16, |ui| {
            let labels: Vec<String> = POLL_RATES.iter().map(|h| format!("{h} Hz")).collect();
            let active = POLL_RATES.iter().position(|h| *h == self.poll_edit);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(S3, S2);
                if let Some(i) = segmented(ui, &p, "poll", &labels, active, 60.0) { self.poll_edit = POLL_RATES[i]; }
                let differs = current != Some(self.poll_edit);
                if btn(ui, &p, "Apply", if differs { Kind::Primary } else { Kind::Secondary }, true).clicked() { set = Some(self.poll_edit); }
                if let Some(c) = current { ui.label(RichText::new(format!("current {c} Hz")).font(mono(T_DATA)).color(p.i65())); }
            });
            if self.poll_edit > 1000 {
                ui.add_space(S1);
                ui.add(egui::Label::new(RichText::new("Above 1000 Hz uses more CPU and battery (Razer quotes roughly a quarter of the 1000 Hz battery life at 8000 Hz).")
                    .font(sans(T_SMALL)).color(p.i65())).wrap());
            }
        });
        set
    }

    fn battery_panel(&mut self, ui: &mut egui::Ui) {
        let p = self.pal;
        let mut changed = false;
        panel(ui, &p, 16, |ui| {
            changed |= switch(ui, &p, &mut self.batt_cfg.enabled, "Send a desktop notification when the battery is low").changed();
            ui.add_space(S3);
            ui.add_enabled_ui(self.batt_cfg.enabled, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = S3;
                    ui.allocate_ui_with_layout(vec2(150.0, 28.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.label(caps(&p, "Notify at or below"));
                    });
                    ui.scope(|ui| {
                        ui.style_mut().visuals.widgets.inactive.bg_fill = p.i28();
                        ui.style_mut().visuals.selection.bg_fill = p.i100();
                        let r = ui.add(egui::Slider::new(&mut self.batt_cfg.threshold, 5..=50).suffix("%"));
                        changed |= r.drag_stopped() || (r.changed() && !r.dragged());
                    });
                });
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = S3;
                    ui.allocate_ui_with_layout(vec2(150.0, 28.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.label(caps(&p, "Check every"));
                    });
                    let r = ui.add(egui::DragValue::new(&mut self.batt_cfg.interval_min).range(1..=60).suffix(" min"));
                    changed |= r.drag_stopped() || r.lost_focus() || (r.changed() && !r.dragged() && !r.has_focus());
                });
                ui.add_space(S1);
                ui.add(egui::Label::new(RichText::new(format!(
                    "One alert when it reaches {}%, then a reminder every {} points lower. Resets once it's charging. Changes apply as you make them.",
                    self.batt_cfg.threshold, battery::REMIND_STEP
                )).font(sans(T_SMALL)).color(p.i65())).wrap());
            });
            ui.add_space(S3);
            hair(ui, &p);
            ui.add_space(S3);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = S3;
                ui.label(caps(&p, "Notifier"));
                if self.batt_service == "active" {
                    chip(ui, &p, Some("●"), "Running", ChipStyle::Plain, p.ok);
                } else {
                    chip(ui, &p, Some("○"), &self.batt_service, ChipStyle::Plain, p.i65());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if btn(ui, &p, "Send test notification", Kind::Secondary, true).clicked() {
                        let pct = self.info.as_ref().and_then(|i| i.battery_pct).unwrap_or(0);
                        let name = self.info.as_ref().map(|i| i.name.clone()).unwrap_or_else(|| "Razer mouse".into());
                        match battery::notify(&format!("{name}: test notification"),
                            &format!("Battery is at {pct}%. Low-battery alerts will look like this."), false) {
                            Ok(()) => self.ok("Test notification sent."),
                            Err(e) => self.err(format!("couldn't send notification: {e}")),
                        }
                    }
                });
            });
        });
        if changed { self.apply_battery_settings(); }
    }

    // ------------------------------------------------------------------ Software remap

    fn ui_software(&mut self, ui: &mut egui::Ui) {
        let p = self.pal;
        title(
            ui, &p, "Software remap.",
            "Optional. Runs on this computer only, while the service is running. Use it for things the mouse \
             can't store itself, such as multi-key shortcuts on several buttons or any evdev key name.",
            ui.available_width() * 0.36,
        );
        let total = ui.available_width();
        let narrow = total < 900.0;
        let right_w = if narrow { total } else { (total * 0.30).clamp(260.0, 340.0) };
        let gap = S4;
        let left_w = if narrow { total } else { total - right_w - gap };
        let layout = if narrow { egui::Layout::top_down(egui::Align::Min) } else { egui::Layout::left_to_right(egui::Align::Min) };
        ui.with_layout(layout, |ui| {
            ui.spacing_mut().item_spacing.x = gap;
            ui.allocate_ui_with_layout(vec2(left_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(left_w);
                section(ui, &p, "01", "Config");
                self.config_panel(ui);
            });
            ui.allocate_ui_with_layout(vec2(right_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(right_w);
                section(ui, &p, "02", "Service");
                self.service_panel(ui);
            });
        });
        // macros: full-width section, editor in the Config column and the parsed list in the Service column
        // (stacked when narrow)
        ui.add_space(S2);
        section(ui, &p, "03", "Macros");
        ui.with_layout(layout, |ui| {
            ui.spacing_mut().item_spacing.x = gap;
            let parsed = ui.allocate_ui_with_layout(vec2(left_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(left_w);
                self.macros_panel(ui)
            }).inner;
            ui.allocate_ui_with_layout(vec2(right_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(right_w);
                self.macros_list(ui, &parsed);
            });
        });
    }

    fn config_panel(&mut self, ui: &mut egui::Ui) {
        let p = self.pal;
        panel(ui, &p, 16, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = S2;
                ui.label(caps(&p, "Config"));
                // a real $XDG_CONFIG_HOME can be arbitrarily long: wrap instead of widening the column
                ui.add(egui::Label::new(RichText::new(self.conf_path.display().to_string()).font(mono(T_DATA)).color(p.i100())).wrap());
            });
            ui.add_space(S2);
            // Comment lines dimmed, the line the parser rejected highlighted (as of the previous frame's parse).
            let err_line: Option<usize> = self.conf_error.as_deref().and_then(|e| e.strip_prefix("line ")?.split(':').next()?.parse().ok());
            let mut layouter = move |ui: &egui::Ui, text: &dyn egui::TextBuffer, wrap: f32| {
                let mut job = egui::text::LayoutJob::default();
                job.wrap.max_width = wrap;
                for (ln, line) in text.as_str().split_inclusive('\n').enumerate() {
                    let (c, f) = if line.trim_start().starts_with('#') { (p.i65(), mono(T_DATA + 1.0)) } else { (p.i100(), mono_med(T_DATA + 1.0)) };
                    let bg = if err_line == Some(ln + 1) { p.bad.gamma_multiply(0.22) } else { Color32::TRANSPARENT };
                    job.append(line, 0.0, egui::TextFormat { font_id: f, color: c, line_height: Some(20.0), background: bg, ..Default::default() });
                }
                ui.painter().layout_job(job)
            };
            let w = ui.available_width();
            ui.add(egui::TextEdit::multiline(&mut self.conf_text).code_editor().desired_rows(9).desired_width(w)
                .layouter(&mut layouter).margin(Margin::same(12)));
            ui.add_space(S2);
            let parsed = remap::parse_config(&self.conf_text);
            match &parsed {
                Ok(m) => { chip(ui, &p, Some("●"), &format!("{} rule(s), valid", m.len()), ChipStyle::Plain, p.ok); self.conf_error = None; }
                Err(e) => {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = S3;
                        chip(ui, &p, Some("×"), "Invalid", ChipStyle::Outline, p.bad);
                        ui.add(egui::Label::new(RichText::new(e).font(mono(T_DATA)).color(p.bad)).wrap());
                    });
                    self.conf_error = Some(e.clone());
                }
            }
            ui.add_space(S3);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = S3;
                if btn(ui, &p, "💾  Save config", Kind::Primary, self.conf_error.is_none()).clicked() {
                    let r = self.conf_path.parent().map(std::fs::create_dir_all).transpose()
                        .and_then(|_| std::fs::write(&self.conf_path, &self.conf_text));
                    match r {
                        Ok(_) => self.ok("Config saved. Restart the service to apply."),
                        Err(e) => self.err(e.to_string()),
                    }
                }
                ui.label(RichText::new(if self.conf_error.is_none() { "Restart the service to apply." } else { "Fix the error to enable saving." })
                    .font(sans(T_SMALL)).color(p.i65()));
            });
        });
    }

    fn service_panel(&mut self, ui: &mut egui::Ui) {
        let p = self.pal;
        panel(ui, &p, 16, |ui| {
            let active = self.service_state == "active";
            ui.label(caps(&p, "Service"));
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(RichText::new(if active { "●" } else { "○" }).font(mono(16.0)).color(if active { p.ok } else { p.i65() }));
                ui.label(RichText::new(&self.service_state).font(serif(T_TITLE + 2.0)).color(if active { p.i100() } else { p.i65() }));
            });
            ui.label(RichText::new(SERVICE).font(mono(T_DATA)).color(p.i65()));
            ui.add_space(S4);
            hair(ui, &p);
            ui.add_space(S3);
            let bw = ui.available_width();
            let half = (bw - S2) / 2.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = S2;
                if btn_w(ui, &p, "Start / restart", Kind::Primary, true, half).clicked() { self.systemctl(&["restart", SERVICE]); }
                if btn_w(ui, &p, "Stop", Kind::Secondary, true, half).clicked() { self.systemctl(&["stop", SERVICE]); }
            });
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = S2;
                if btn_w(ui, &p, "Enable at login", Kind::Secondary, true, half).clicked() { self.systemctl(&["enable", "--now", SERVICE]); }
                if btn_w(ui, &p, "Disable", Kind::Secondary, true, half).clicked() { self.systemctl(&["disable", "--now", SERVICE]); }
            });
            if btn_w(ui, &p, "↻  Refresh state", Kind::Secondary, true, bw).clicked() { self.refresh_service(); }
        });
    }

    /// macros.conf editor, laid out like `config_panel`. Returns this frame's parse for the list beside it.
    fn macros_panel(&mut self, ui: &mut egui::Ui) -> Result<Macros, String> {
        let p = self.pal;
        panel(ui, &p, 16, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = S2;
                ui.label(caps(&p, "Macros"));
                ui.add(egui::Label::new(RichText::new(self.macros_path.display().to_string()).font(mono(T_DATA)).color(p.i100())).wrap());
            });
            ui.add_space(S2);
            code_editor(ui, &p, "macros", &mut self.macros_text, self.macros_error.as_deref());
            ui.add_space(S2);
            let parsed = macros::parse_macros(&self.macros_text);
            match &parsed {
                Ok(m) => { chip(ui, &p, Some("●"), &format!("{} macro(s), valid", m.len()), ChipStyle::Plain, p.ok); self.macros_error = None; }
                Err(e) => {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = S3;
                        chip(ui, &p, Some("×"), "Invalid", ChipStyle::Outline, p.bad);
                        ui.add(egui::Label::new(RichText::new(e).font(mono(T_DATA)).color(p.bad)).wrap());
                    });
                    self.macros_error = Some(e.clone());
                }
            }
            ui.add_space(S3);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = S3;
                if btn(ui, &p, "💾  Save macros", Kind::Primary, self.macros_error.is_none()).clicked() { self.save_macros(); }
                ui.label(RichText::new(if self.macros_error.is_none() { "Restart the service to apply." } else { "Fix the error to enable saving." })
                    .font(sans(T_SMALL)).color(p.i65()));
            });
            ui.add_space(S3);
            hair(ui, &p);
            ui.add_space(S3);
            macro_syntax(ui, &p);
            parsed
        })
    }

    /// Read-only summary of the parsed macros: name, event count, total delay (user delays, before the
    /// playback padding).
    fn macros_list(&self, ui: &mut egui::Ui, parsed: &Result<Macros, String>) {
        let p = self.pal;
        panel(ui, &p, 16, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let w = ui.available_width();
            let num_w = 64.0;
            let cols = [(w - 2.0 * num_w).max(0.0), num_w, num_w];
            list_row(ui, cols, [caps(&p, "Name"), caps(&p, "Events"), caps(&p, "Delay")]);
            ui.add_space(S1);
            hair(ui, &p);
            ui.add_space(S1);
            let note = |ui: &mut egui::Ui, s: &str| {
                ui.add_space(S1);
                ui.add(egui::Label::new(RichText::new(s).font(sans(T_SMALL)).color(p.i65())).wrap());
            };
            match parsed {
                Err(_) => note(ui, "Fix the error to list the macros."),
                Ok(m) if m.is_empty() => note(ui, "No macros yet: add a [name] block with at least one event."),
                Ok(m) => {
                    for mac in m.values() {
                        let (events, delay_ms) = macro_summary(mac);
                        let cell = |s: String| RichText::new(s).font(mono(T_DATA)).color(p.i100());
                        list_row(ui, cols, [cell(mac.name.clone()), cell(events.to_string()), cell(format!("{delay_ms} ms"))]);
                    }
                }
            }
        });
    }
}

// ---------------------------------------------------------------------- layout helpers

const TABS: [Tab; 3] = [Tab::Buttons, Tab::Device, Tab::Software];
/// Same set Synapse's protocol layer supports (rzDevice25 RATE_125Hz..RATE_8000Hz).
const POLL_RATES: [u32; 7] = [125, 250, 500, 1000, 2000, 4000, 8000];

fn key_syntax(ui: &mut egui::Ui, p: &Pal) {
    ui.add(egui::Label::new(RichText::new(
        "Keys: letters, digits, f1–f24, enter, esc, tab, space, arrows, home/end/pageup/pagedown, insert/delete; \
         modifiers ctrl, shift, alt, super (one key + any modifiers).",
    ).font(sans(T_SMALL)).color(p.i65())).wrap());
}

/// macros.conf when there is none yet: comments only (zero macros, so it saves as is), with one working example.
const MACROS_TEMPLATE: &str = "\
# One [name] block per macro, one event per line. Bind it in Config:  forward = macro:copy
# [copy]
# down ctrl
# delay 20
# tap c
# up ctrl
";

fn macro_syntax(ui: &mut egui::Ui, p: &Pal) {
    ui.add(egui::Label::new(RichText::new(
        "[name] · down/up/tap KEY · click BUTTON · delay MS · wheel N · hwheel N · move DX DY (keys as in Config). \
         Bind in Config: forward = macro:NAME[,once|xN|held|toggle]",
    ).font(sans(T_SMALL)).color(p.i65())).wrap());
}

/// Multiline code editor (as in `config_panel`): comment lines dimmed, the line named by a `line N: …` parse
/// error highlighted (as of the previous frame's parse).
fn code_editor(ui: &mut egui::Ui, p: &Pal, id: &str, text: &mut String, error: Option<&str>) {
    let p = *p;
    let err_line: Option<usize> = error.and_then(|e| e.strip_prefix("line ")?.split(':').next()?.parse().ok());
    let mut layouter = move |ui: &egui::Ui, text: &dyn egui::TextBuffer, wrap: f32| {
        let mut job = egui::text::LayoutJob::default();
        job.wrap.max_width = wrap;
        for (ln, line) in text.as_str().split_inclusive('\n').enumerate() {
            let (c, f) = if line.trim_start().starts_with('#') { (p.i65(), mono(T_DATA + 1.0)) } else { (p.i100(), mono_med(T_DATA + 1.0)) };
            let bg = if err_line == Some(ln + 1) { p.bad.gamma_multiply(0.22) } else { Color32::TRANSPARENT };
            job.append(line, 0.0, egui::TextFormat { font_id: f, color: c, line_height: Some(20.0), background: bg, ..Default::default() });
        }
        ui.painter().layout_job(job)
    };
    let w = ui.available_width();
    ui.add(egui::TextEdit::multiline(text).id_salt(id).code_editor().desired_rows(9).desired_width(w)
        .layouter(&mut layouter).margin(Margin::same(12)));
}

/// One row of the macros list: a left-aligned name column, then right-aligned number columns. Long names are
/// truncated (full text on hover).
fn list_row(ui: &mut egui::Ui, cols: [f32; 3], cells: [RichText; 3]) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for (i, (w, cell)) in cols.into_iter().zip(cells).enumerate() {
            let layout = if i == 0 { egui::Layout::left_to_right(egui::Align::Center) } else { egui::Layout::right_to_left(egui::Align::Center) };
            ui.allocate_ui_with_layout(vec2(w, 22.0), layout, |ui| {
                ui.set_min_size(vec2(w, 22.0));
                ui.add(egui::Label::new(cell).truncate());
            });
        }
    });
}

/// (event count, sum of the macro's own delays in ms). `tap` / `click` count as two events, as in the file
/// format's limit; Synapse's playback padding is not included.
fn macro_summary(m: &macros::Macro) -> (usize, u64) {
    let delay = m.events.iter().map(|e| match e { macros::MacroEvent::Delay(ms) => u64::from(*ms), _ => 0 }).sum();
    (m.events.len(), delay)
}

/// Write `text` to a temp file next to `path`, then rename it over `path` (creating the parent dir), so a crash
/// never leaves a half-written file. The temp file is removed if anything fails.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let r = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// Title row: serif display title left, one- or two-line intro in a fixed column to its right.
fn title(ui: &mut egui::Ui, p: &Pal, t: &str, intro: &str, intro_x: f32) {
    let total = ui.available_width();
    if total < 900.0 {
        ui.label(RichText::new(t).font(serif(T_DISPLAY)).color(p.i100()));
        ui.add(egui::Label::new(RichText::new(intro).font(sans(T_BODY - 1.0)).color(p.i65())).wrap());
        ui.add_space(S3);
        return;
    }
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.allocate_ui_with_layout(vec2(intro_x, 46.0), egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_min_size(vec2(intro_x, 46.0));
            ui.label(RichText::new(t).font(serif(T_DISPLAY)).color(p.i100()));
        });
        ui.allocate_ui_with_layout(vec2(total - intro_x, 46.0), egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_width(total - intro_x);
            ui.add_space(14.0);
            ui.add(egui::Label::new(RichText::new(intro).font(sans(T_BODY - 1.0)).color(p.i65())).wrap());
        });
    });
    ui.add_space(S3);
}

/// Fixed-size table cell whose content is vertically centred (egui's `Align::Center` only centres within the
/// content height, so pad explicitly).
fn vcell(ui: &mut egui::Ui, w: f32, row_h: f32, content_h: f32, add: impl FnOnce(&mut egui::Ui)) {
    ui.allocate_ui_with_layout(vec2(w, row_h), egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.set_min_size(vec2(w, row_h));
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.add_space(((row_h - content_h) / 2.0).max(0.0));
        add(ui);
    });
}

/// (name, qualifier): the qualifier is shown as a mono caption under the name.
fn pretty_button(id: u8) -> (&'static str, &'static str) {
    match id {
        1 => ("Left", ""),
        2 => ("Right", ""),
        3 => ("Middle", "wheel click"),
        4 => ("Back", "side, rear"),
        5 => ("Forward", "side, front"),
        0x60 => ("DPI", "underside"),
        9 => ("Wheel up", ""),
        10 => ("Wheel down", ""),
        _ => ("Other", ""),
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        demo::step(self, &ctx);
        self.start_watch(&ctx);
        let mode = self.demo.theme.unwrap_or(if ctx.theme() == egui::Theme::Light { Mode::Light } else { Mode::Dark });
        if self.styled != Some(mode) {
            self.pal = Pal::new(mode);
            apply_style(&ctx, &self.pal);
            self.styled = Some(mode);
        }
        let p = self.pal;

        // page + ghost kanji behind everything (background layer, so it sits under every opaque panel)
        let screen = ctx.content_rect();
        let bgp = ctx.layer_painter(egui::LayerId::background());
        bgp.rect_filled(screen, 0.0, p.bg);
        let glyph = match (self.tab, self.info.is_some()) {
            (Tab::Buttons, _) if self.rows.is_empty() => "空",
            (Tab::Buttons, _) => "動",
            (Tab::Device, false) => "空",
            (Tab::Device, true) => "力",
            (Tab::Software, _) => "鏡",
        };
        bgp.with_clip_rect(screen).text(pos2(screen.right() + 20.0, screen.top() - 40.0), Align2::RIGHT_TOP, glyph, kanji(420.0), p.i05());

        // bottom: status bar (outermost), then the button tester above it
        let err = self.status_is_error;
        egui::Panel::bottom("status")
            .frame(Frame::new()
                .fill(if err { p.stamp_bg } else { p.bg })
                .stroke(Stroke::new(1.0, if err { p.stamp_bg } else { p.i18() }))
                .inner_margin(Margin::symmetric(24, 6)))
            .show(ui, |ui| self.status_bar(ui));
        egui::Panel::bottom("tester").show_separator_line(false)
            .frame(Frame::new().fill(p.sunken).inner_margin(Margin::symmetric(24, 10)))
            .show(ui, |ui| self.tester(ui));
        egui::Panel::top("tabs").show_separator_line(false)
            .frame(Frame::new().inner_margin(Margin { left: 24, right: 24, top: 16, bottom: 12 }))
            .show(ui, |ui| self.header(ui));
        egui::Panel::top("strip").show_separator_line(false)
            .frame(Frame::new().inner_margin(Margin { left: 24, right: 24, top: 0, bottom: 16 }))
            .show(ui, |ui| self.device_strip(ui));
        egui::CentralPanel::default()
            .frame(Frame::new().inner_margin(Margin::symmetric(24, 0)))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.tab {
                    Tab::Buttons => self.ui_buttons(ui),
                    Tab::Device => self.ui_device(ui),
                    Tab::Software => self.ui_software(ui),
                });
            });
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        demo::inject(self, ctx, raw);
    }
}

fn main() -> eframe::Result {
    let (w, h) = demo::window_size().unwrap_or((1280.0, 900.0));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("razerctl")
            .with_app_id("razerctl")
            .with_inner_size([w, h])
            .with_min_inner_size([780.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native("razerctl", options, Box::new(|cc| {
        install_fonts(&cc.egui_ctx);
        Ok(Box::new(App::new()))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::epaint::Shape as EShape;

    fn test_app() -> App {
        App {
            tab: Tab::Buttons,
            status: String::new(),
            status_is_error: false,
            info: None,
            backend_kind: String::new(),
            dpi_edit: 800,
            poll_edit: 1000,
            profile: 1,
            rows: Vec::new(),
            conf_path: PathBuf::from("/nonexistent/razerctl/remap.conf"),
            conf_text: "# back = ctrl+c\n".into(),
            conf_error: None,
            macros_path: PathBuf::from("/nonexistent/razerctl/macros.conf"),
            macros_text: MACROS_TEMPLATE.into(),
            macros_error: None,
            service_state: "inactive".into(),
            presses: Arc::new(Mutex::new(VecDeque::new())),
            watching: false,
            batt_cfg: battery::Config { enabled: true, threshold: 20, interval_min: 5 },
            batt_service: "inactive".into(),
            selected: None,
            failed: None,
            pal: Pal::new(Mode::Dark),
            styled: None,
            demo: demo::Demo::default(),
        }
    }

    fn with_device(app: &mut App) {
        app.info = Some(demo::info(82, false));
        app.backend_kind = "hidraw".into();
        app.rows = demo::rows();
    }

    /// Lay out one frame of `tab` (header, strip, tester, status bar and the tab body) and return every string drawn.
    fn render(app: &mut App, tab: Tab, mode: Mode, size: (f32, f32)) -> String {
        render_texts(app, tab, mode, size).into_iter().map(|(s, ..)| s + "\n").collect()
    }

    /// Like `render`, one entry per string drawn: text, position, and override colour (buttons paint their label
    /// with one, so it tells an enabled button from a disabled one).
    fn render_texts(app: &mut App, tab: Tab, mode: Mode, size: (f32, f32)) -> Vec<(String, egui::Pos2, Option<Color32>)> {
        let ctx = egui::Context::default();
        install_fonts(&ctx);
        app.pal = Pal::new(mode);
        apply_style(&ctx, &app.pal);
        app.tab = tab;
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(size.0, size.1)));
        let mut texts = Vec::new();
        for _ in 0..2 {
            let mut out = ctx.run_ui(input.clone(), |ui| {
                app.header(ui);
                app.device_strip(ui);
                app.tester(ui);
                app.status_bar(ui);
                match tab {
                    Tab::Buttons => app.ui_buttons(ui),
                    Tab::Device => app.ui_device(ui),
                    Tab::Software => app.ui_software(ui),
                }
            });
            texts.clear();
            for cs in &out.shapes {
                if let EShape::Text(t) = &cs.shape { texts.push((t.galley.text().to_string(), t.pos, t.override_text_color)); }
            }
            // No renderer consumes the font-atlas upload here; epaint 0.36 panics on drop if it is left unapplied.
            out.textures_delta.clear();
        }
        texts
    }

    const SIZES: [(f32, f32); 2] = [(1280.0, 900.0), (780.0, 560.0)];
    /// Window height that fits the whole Software tab, macros included, in both widths (render has no scroll area).
    const TALL: f32 = 2400.0;

    /// copy: 5 events (tap = down + up), 30 ms; scroll: 3 events, 15 ms.
    const MACROS: &str = "# two macros\n[copy]\ndown ctrl\ndelay 30\ntap c\nup ctrl\n\n[scroll]\nwheel 1\ndelay 15\nwheel 1\n";
    const BAD_MACROS: &str = "[copy]\ndown ctrl\ntap c\n";

    #[test]
    fn every_tab_lays_out_in_both_modes_and_both_window_sizes() {
        for no_device in [false, true] {
            for mode in [Mode::Dark, Mode::Light] {
                for size in SIZES {
                    for (tab, _) in demo::TABS {
                        let mut app = test_app();
                        if !no_device { with_device(&mut app); }
                        let text = render(&mut app, tab, mode, size);
                        assert!(!text.is_empty(), "{tab:?} {mode:?} {size:?} drew nothing");
                    }
                    // the macros section is below the fold at these heights: lay it out tall enough to be drawn,
                    // with the template (empty list), real macros and a parse error
                    for macros_text in [MACROS_TEMPLATE, MACROS, BAD_MACROS] {
                        let mut app = test_app();
                        if !no_device { with_device(&mut app); }
                        app.macros_text = macros_text.into();
                        let text = render(&mut app, Tab::Software, mode, (size.0, TALL));
                        assert!(text.contains("03 // MACROS") && text.contains("SAVE MACROS"), "{mode:?} {size:?}: {text}");
                    }
                }
            }
        }
    }

    #[test]
    fn low_battery_settings_are_visible_with_no_device() {
        let mut app = test_app();
        // the narrow case is tall: egui drops text that is entirely below the screen, and this test has no scroll area
        for size in [(1280.0, 900.0), (780.0, 1800.0)] {
            let text = render(&mut app, Tab::Device, Mode::Dark, size);
            assert!(text.contains("LOW-BATTERY NOTIFICATION"), "{size:?}: section missing: {text}");
            assert!(text.contains("Send a desktop notification when the battery is low"), "{size:?}");
            assert!(text.contains("NOTIFY AT OR BELOW") && text.contains("CHECK EVERY"), "{size:?}");
            assert!(text.contains("No device info."), "{size:?}: empty state missing");
            assert!(!text.contains("POLLING RATE"), "{size:?}: DPI / polling need a device");
        }
    }

    #[test]
    fn device_tab_offers_all_seven_polling_rates_and_keeps_the_high_rate_warning() {
        let mut app = test_app();
        with_device(&mut app);
        app.poll_edit = 8000;
        let text = render(&mut app, Tab::Device, Mode::Dark, (1280.0, 900.0));
        for hz in POLL_RATES { assert!(text.contains(&format!("{hz} HZ")), "{hz} Hz segment missing"); }
        assert!(text.contains("Above 1000 Hz uses more CPU and battery"));
    }

    #[test]
    fn buttons_tab_says_what_is_wrong_instead_of_an_empty_table() {
        let mut app = test_app();
        let text = render(&mut app, Tab::Buttons, Mode::Light, (780.0, 560.0));
        assert!(text.contains("Wake the mouse and press Refresh."));
        assert!(text.contains("KEY SYNTAX"), "key syntax footnote was always visible");
        assert!(text.contains("RESET ALL TO DEFAULT"), "Reset stays enabled with no rows, as before");
    }

    #[test]
    fn mouse_diagram_collapses_below_900px() {
        let mut app = test_app();
        with_device(&mut app);
        let wide = render(&mut app, Tab::Buttons, Mode::Dark, (1280.0, 900.0));
        let narrow = render(&mut app, Tab::Buttons, Mode::Dark, (780.0, 560.0));
        assert!(wide.contains("TOP VIEW") && wide.contains("UNDERSIDE"));
        assert!(!narrow.contains("TOP VIEW"));
        assert!(narrow.contains("ONBOARD ASSIGNMENTS"));
    }

    #[test]
    fn header_drops_the_device_name_when_narrow_but_keeps_the_battery() {
        let mut app = test_app();
        with_device(&mut app);
        let name = app.info.as_ref().unwrap().name.clone();
        assert!(render(&mut app, Tab::Device, Mode::Dark, (1280.0, 900.0)).matches(name.as_str()).count() >= 2);
        let narrow = render(&mut app, Tab::Device, Mode::Dark, (780.0, 560.0));
        assert_eq!(narrow.matches(name.as_str()).count(), 1, "only Status > NAME, not the header");
        assert!(narrow.contains("82%"));
    }

    #[test]
    fn choices_keep_their_fifteen_labels_and_order() {
        let labels: Vec<&str> = CHOICES.iter().map(|(l, _)| *l).collect();
        assert_eq!(labels, [
            "Factory default", "Disabled", "Left click", "Right click", "Middle click", "Back", "Forward",
            "Scroll up", "Scroll down", "Double click", "DPI up", "DPI down", "DPI cycle up", "DPI cycle down",
            "Keyboard key…",
        ]);
        assert!(CHOICES.last().unwrap().1.is_none(), "last entry is the custom-key choice");
    }

    #[test]
    fn row_is_dirty_only_when_the_dropdown_differs_from_the_mouse() {
        let mut row = demo::rows().remove(3); // Back, factory default
        assert!(!row.dirty());
        row.choice = CHOICES.iter().position(|(l, _)| *l == "Disabled").unwrap();
        assert!(row.dirty());
        row.choice = CHOICES.len() - 1;
        row.key_text = "f99".into(); // does not parse: still counts as an unsaved edit
        assert!(row.dirty());
    }

    #[test]
    fn selection_defaults_to_the_first_unlocked_row_and_ignores_stale_ids() {
        let mut app = test_app();
        assert_eq!(app.selected_id(), None);
        with_device(&mut app);
        assert_eq!(app.selected_id(), Some(2), "Left (1) is locked");
        app.selected = Some(4);
        assert_eq!(app.selected_id(), Some(4));
        app.selected = Some(99);
        assert_eq!(app.selected_id(), Some(2));
    }

    #[test]
    fn button_names_carry_their_qualifier_separately() {
        assert_eq!(pretty_button(3), ("Middle", "wheel click"));
        assert_eq!(pretty_button(0x60), ("DPI", "underside"));
        assert_eq!(pretty_button(1), ("Left", ""));
        assert_eq!(pretty_button(0x42), ("Other", ""));
    }

    /// Override colour of the drawn "SAVE MACROS" label: stamp_fg when the button is enabled, ink-45 when not.
    fn save_macros_colour(texts: &[(String, egui::Pos2, Option<Color32>)]) -> Option<Color32> {
        texts.iter().find(|(s, ..)| s.ends_with("SAVE MACROS")).and_then(|(_, _, c)| *c)
    }

    #[test]
    fn macros_section_lists_valid_macros_and_refuses_to_save_invalid_ones() {
        for size in SIZES {
            let size = (size.0, TALL);
            let mut app = test_app();
            app.macros_text = MACROS.into();
            let texts = render_texts(&mut app, Tab::Software, Mode::Dark, size);
            let lines: Vec<&str> = texts.iter().map(|(s, ..)| s.as_str()).collect();
            assert!(lines.contains(&"2 MACRO(S), VALID"), "{size:?}: {lines:?}");
            for cell in ["copy", "5", "30 ms", "scroll", "3", "15 ms"] {
                assert!(lines.contains(&cell), "{size:?}: list cell {cell:?} missing: {lines:?}");
            }
            assert!(lines.contains(&"/nonexistent/razerctl/macros.conf"), "{size:?}: path missing");
            assert_eq!(app.macros_error, None);
            assert_eq!(save_macros_colour(&texts), Some(app.pal.stamp_fg), "{size:?}: Save should be enabled");

            app.macros_text = BAD_MACROS.into();
            let texts = render_texts(&mut app, Tab::Software, Mode::Dark, size);
            let lines: Vec<&str> = texts.iter().map(|(s, ..)| s.as_str()).collect();
            let err = "line 2: macro 'copy' leaves KEY_LEFTCTRL pressed";
            assert_eq!(app.macros_error.as_deref(), Some(err));
            assert!(lines.contains(&"INVALID") && lines.contains(&err), "{size:?}: {lines:?}");
            assert!(lines.contains(&"Fix the error to enable saving."), "{size:?}");
            assert!(lines.contains(&"Fix the error to list the macros."), "{size:?}");
            assert!(!lines.contains(&"copy"), "{size:?}: no list while the text doesn't parse");
            assert_eq!(save_macros_colour(&texts), Some(app.pal.i45()), "{size:?}: Save should be disabled");
            assert!(app.status.is_empty(), "nothing was saved or reported");
        }
    }

    #[test]
    fn macros_list_sits_beside_the_editor_when_wide_and_under_it_when_narrow() {
        let at = |texts: &[(String, egui::Pos2, Option<Color32>)], s: &str| {
            texts.iter().find(|(t, ..)| t == s).map(|(_, pos, _)| *pos).unwrap_or_else(|| panic!("{s} not drawn"))
        };
        let mut app = test_app();
        let wide = render_texts(&mut app, Tab::Software, Mode::Light, (1280.0, TALL));
        let (editor, list) = (at(&wide, "MACROS"), at(&wide, "NAME"));
        assert!(list.x > editor.x + 600.0 && (list.y - editor.y).abs() < 4.0, "wide: editor {editor:?}, list {list:?}");
        let narrow = render_texts(&mut app, Tab::Software, Mode::Light, (780.0, TALL));
        let (editor, list) = (at(&narrow, "MACROS"), at(&narrow, "NAME"));
        assert!((list.x - editor.x).abs() < 4.0 && list.y > editor.y + 200.0, "narrow: editor {editor:?}, list {list:?}");
        assert!(narrow.iter().any(|(t, ..)| t == "No macros yet: add a [name] block with at least one event."));
    }

    #[test]
    fn macros_template_is_valid_and_its_commented_example_is_a_working_macro() {
        assert!(macros::parse_macros(MACROS_TEMPLATE).unwrap().is_empty(), "comments only: saves as is");
        let example: String = MACROS_TEMPLATE.lines().skip_while(|l| !l.starts_with("# [")).map(|l| format!("{}\n", &l[2..])).collect();
        let m = macros::parse_macros(&example).unwrap();
        assert_eq!(m.keys().collect::<Vec<_>>(), ["copy"]);
        assert_eq!(macro_summary(&m["copy"]), (5, 20));
        assert!(MACROS_TEMPLATE.contains("forward = macro:copy"), "the template says how to bind it");
    }

    /// Unique dir under the system temp dir, removed on drop (also when a test fails).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            TempDir(std::env::temp_dir().join(format!("razerctl-gui-{tag}-{}-{nanos}", std::process::id())))
        }
        fn names(&self, sub: &str) -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(self.0.join(sub)).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            v.sort();
            v
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn save_macros_writes_the_raw_text_via_a_temp_file_and_leaves_none_behind() {
        let dir = TempDir::new("macros-save");
        let mut app = test_app();
        app.macros_path = dir.0.join("razerctl/macros.conf");
        app.macros_text = MACROS.into();
        app.save_macros(); // creates razerctl/
        assert!(!app.status_is_error, "{}", app.status);
        assert_eq!(app.status, "Macros saved. Restart the service to apply.");
        assert_eq!(std::fs::read_to_string(&app.macros_path).unwrap(), MACROS, "raw text, comments kept");
        app.macros_text = MACROS_TEMPLATE.into();
        app.save_macros(); // overwrite
        assert_eq!(std::fs::read_to_string(&app.macros_path).unwrap(), MACROS_TEMPLATE);
        assert_eq!(dir.names("razerctl"), ["macros.conf"]);

        // text that doesn't parse is refused before touching the file
        app.ok("");
        app.macros_text = BAD_MACROS.into();
        app.save_macros();
        assert!(app.status_is_error && app.status.contains("line 2:"), "{}", app.status);
        assert_eq!(std::fs::read_to_string(&app.macros_path).unwrap(), MACROS_TEMPLATE);

        // a failed rename (target is a directory) cleans up its temp file
        std::fs::create_dir(dir.0.join("razerctl/sub")).unwrap();
        assert!(write_atomic(&dir.0.join("razerctl/sub"), "x").is_err());
        assert_eq!(dir.names("razerctl"), ["macros.conf", "sub"]);
    }
}
