//! System tray (DAN-1453), built to the DAN-1452 mockups: `tray-icons.png` (five states, priority order, tooltips),
//! `tray-menu-{dark,light}.png` (rows and enable rules), `app-firstclose-*.png` (the one-time notice) and
//! `app-settings-*.png` (the close setting this module stores).
//!
//! **How "hide to the tray" works.** winit cannot hide a window on Wayland (`set_visible` is a no-op) and cannot
//! un-minimise one, so the window is really closed. `main` then keeps the *process* alive, holding the tray icon and
//! the battery poller, and on *Show razerctl* starts the GUI again with `exec`. On X11 this behaves the same, so
//! there is one code path to test. What is lost across a hide is the window's unsaved UI state, which closing the
//! window always lost.
//!
//! The tray is a StatusNotifierItem over D-Bus (`ksni`, pure Rust; `tray-icon` was rejected because it pulls in GTK3).
//! The remap service and the low-battery notifier are separate systemd user units and never depend on this process.
//!
//! The first half of the file is decisions with no I/O in it (close action, icon state, tooltip, menu rows, pixmaps).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use eframe::egui;
use razerctl::battery;

use super::service::{self, RealSys};
use super::SERVICE;

// ------------------------------------------------------------------------------------------------ settings

/// `~/.config/razerctl/gui.conf`: what the window itself does, as opposed to `battery.conf` / `remap.conf`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Closing the window hides to the tray (when there is one) instead of quitting. Default on.
    pub close_to_tray: bool,
    /// The one-time "razerctl is still running" notice has been shown.
    pub notified: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            close_to_tray: true,
            notified: false,
        }
    }
}

pub fn config_path() -> PathBuf {
    battery::config_path().with_file_name("gui.conf")
}

/// `key = value` lines; unknown keys and bad values keep the defaults.
pub fn parse_settings(text: &str) -> Settings {
    let mut s = Settings::default();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = matches!(v.trim(), "true" | "yes" | "1" | "on");
        match k.trim() {
            "close_to_tray" => s.close_to_tray = v,
            "tray_notice_shown" => s.notified = v,
            _ => {}
        }
    }
    s
}

pub fn format_settings(s: &Settings) -> String {
    format!(
        "# razerctl-gui settings\nclose_to_tray = {}\ntray_notice_shown = {}\n",
        s.close_to_tray, s.notified
    )
}

pub fn load_settings(path: &Path) -> Settings {
    std::fs::read_to_string(path)
        .map(|t| parse_settings(&t))
        .unwrap_or_default()
}

pub fn save_settings(path: &Path, s: &Settings) -> std::io::Result<()> {
    super::write_atomic(path, &format_settings(s))
}

// ------------------------------------------------------------------------------------------------ close decision

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseAction {
    /// Close the window and keep the process in the tray.
    Hide,
    /// Close the window and exit.
    Quit,
}

/// What closing the window does. With no StatusNotifier host there is nowhere to hide to, and a hidden window the user
/// cannot get back would be worse than quitting (DAN-1452 decision 4), so the stored choice is kept but not obeyed.
pub fn close_action(close_to_tray: bool, host_present: bool) -> CloseAction {
    if close_to_tray && host_present {
        CloseAction::Hide
    } else {
        CloseAction::Quit
    }
}

// ------------------------------------------------------------------------------------------------ what the tray shows

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemapLine {
    Running,
    /// Unit active but no mouse: remap is waiting for it.
    Waiting,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading {
    /// Marketing name without the "Razer " prefix, as on the tray sheet ("Viper Mini SE").
    pub name: String,
    pub pct: Option<u8>,
    pub charging: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub device: Option<Reading>,
    pub remap: RemapLine,
    /// `battery.conf` threshold: the level the tray calls "low".
    pub threshold: u8,
}

impl Default for Snapshot {
    fn default() -> Self {
        Snapshot {
            device: None,
            remap: RemapLine::Stopped,
            threshold: battery::Config::default().threshold,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph {
    NoDevice,
    Low,
    Charging,
    Remap,
    Normal,
}

impl Glyph {
    #[cfg(test)]
    pub const ALL: [Glyph; 5] = [
        Glyph::NoDevice,
        Glyph::Low,
        Glyph::Charging,
        Glyph::Remap,
        Glyph::Normal,
    ];

    /// Symbolic icon name, as in `tray-icons.png`.
    pub fn icon_name(self) -> &'static str {
        match self {
            Glyph::NoDevice => "razerctl-nodevice-symbolic",
            Glyph::Low => "razerctl-low-symbolic",
            Glyph::Charging => "razerctl-charging-symbolic",
            Glyph::Remap => "razerctl-remap-symbolic",
            Glyph::Normal => "razerctl-symbolic",
        }
    }
}

impl Snapshot {
    pub fn is_low(&self) -> bool {
        self.device
            .as_ref()
            .is_some_and(|d| !d.charging && d.pct.is_some_and(|p| p <= self.threshold))
    }
}

/// The icon shows the first condition that is true: no mouse, low, charging, remap running, normal. Never "passive":
/// KDE folds passive items into the overflow menu, which would hide the icon exactly when no mouse is found.
pub fn glyph(s: &Snapshot) -> Glyph {
    let Some(d) = &s.device else {
        return Glyph::NoDevice;
    };
    if s.is_low() {
        Glyph::Low
    } else if d.charging {
        Glyph::Charging
    } else if s.remap == RemapLine::Running {
        Glyph::Remap
    } else {
        Glyph::Normal
    }
}

/// `razerctl · Viper Mini SE · 14% (low)`, as in the tooltip column of `tray-icons.png`.
pub fn tooltip(s: &Snapshot) -> String {
    let Some(d) = &s.device else {
        return "razerctl · no mouse found".into();
    };
    let mut t = format!("razerctl · {}", d.name);
    if let Some(p) = d.pct {
        t.push_str(&format!(" · {p}%"));
        if s.is_low() {
            t.push_str(" (low)");
        } else if d.charging {
            t.push_str(" (charging)");
        }
    }
    if glyph(s) == Glyph::Remap {
        t.push_str(" · remap running");
    }
    t
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Show,
    StartRemap,
    StopRemap,
    StopMacros,
    Quit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    Action {
        label: &'static str,
        act: Act,
        enabled: bool,
    },
    Info(String),
    Sep,
}

/// The menu of `tray-menu-*.png`: Show, battery line, remap line and its opposite action, Stop all macros, Quit.
/// Stop all macros is enabled while remap is running with a mouse (macros only play then; v1 has no playback status).
pub fn menu_rows(s: &Snapshot) -> Vec<Row> {
    let device = match &s.device {
        None => "○ No mouse found".to_string(),
        Some(d) => {
            let mut t = d.name.clone();
            if let Some(p) = d.pct {
                t.push_str(&format!(" · {p}%"));
            }
            if s.is_low() {
                t = format!("▲ {t} · low");
            } else if d.charging {
                t.push_str(" · charging");
            }
            t
        }
    };
    let (remap, toggle) = match s.remap {
        RemapLine::Running => (
            "● Remap running",
            Row::Action {
                label: "Stop remap",
                act: Act::StopRemap,
                enabled: true,
            },
        ),
        RemapLine::Waiting => (
            "○ Remap waiting for the mouse",
            Row::Action {
                label: "Stop remap",
                act: Act::StopRemap,
                enabled: true,
            },
        ),
        RemapLine::Stopped => (
            "○ Remap stopped",
            Row::Action {
                label: "Start remap",
                act: Act::StartRemap,
                enabled: true,
            },
        ),
        RemapLine::Failed => (
            "× Remap failed",
            Row::Action {
                label: "Start remap",
                act: Act::StartRemap,
                enabled: true,
            },
        ),
    };
    vec![
        Row::Action {
            label: "Show razerctl",
            act: Act::Show,
            enabled: true,
        },
        Row::Sep,
        Row::Info(device),
        Row::Sep,
        Row::Info(remap.into()),
        toggle,
        Row::Action {
            label: "Stop all macros",
            act: Act::StopMacros,
            enabled: s.remap == RemapLine::Running,
        },
        Row::Sep,
        Row::Action {
            label: "Quit razerctl",
            act: Act::Quit,
            enabled: true,
        },
    ]
}

/// One reading of the world, as the poller sees it. `unit` is `systemctl is-active` for the remap unit.
pub fn snapshot_from(
    read: Result<(String, u8, bool), String>,
    unit: &str,
    threshold: u8,
) -> Snapshot {
    let device = read.ok().map(|(name, pct, charging)| Reading {
        name: name.strip_prefix("Razer ").unwrap_or(&name).to_string(),
        pct: Some(pct),
        charging,
    });
    let remap = match unit {
        "active" if device.is_some() => RemapLine::Running,
        "active" => RemapLine::Waiting,
        "failed" => RemapLine::Failed,
        _ => RemapLine::Stopped,
    };
    Snapshot {
        device,
        remap,
        threshold,
    }
}

// ------------------------------------------------------------------------------------------------ pixmaps

/// ARGB32 (network byte order) pixmap of `g` at `size` px, for hosts that cannot resolve the symbolic icon names (the
/// SVGs in `packaging/` are the primary icons: the shell recolours those). Same 22-unit geometry as the SVGs and
/// the mockup: light glyph with a faint dark halo so it reads on both panels.
pub fn pixmap(g: Glyph, size: u32) -> ksni::Icon {
    const SS: u32 = 4;
    let n = size as usize;
    let mut cov = vec![0.0f32; n * n];
    for py in 0..n {
        for px in 0..n {
            let mut hit = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) * 22.0 / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) * 22.0 / size as f32;
                    hit += glyph_covers(g, x, y) as u32;
                }
            }
            cov[py * n + px] = hit as f32 / (SS * SS) as f32;
        }
    }
    let mut data = Vec::with_capacity(n * n * 4);
    for py in 0..n {
        for px in 0..n {
            let a = cov[py * n + px];
            let mut halo = 0.0f32;
            for (dx, dy) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)] {
                let (qx, qy) = (px as i32 + dx, py as i32 + dy);
                if qx >= 0 && qy >= 0 && (qx as usize) < n && (qy as usize) < n {
                    halo = halo.max(cov[qy as usize * n + qx as usize]);
                }
            }
            // premultiplied-free ARGB: light glyph over a 55% dark halo
            let (r, gc, b, alpha) = if a > 0.0 {
                let alpha = a + (1.0 - a) * halo * 0.55;
                let mix = a / alpha;
                (
                    0xe8 as f32 * mix,
                    0xe8 as f32 * mix,
                    0xe8 as f32 * mix,
                    alpha,
                )
            } else {
                (0.0, 0.0, 0.0, halo * 0.55)
            };
            data.extend_from_slice(&[
                (alpha * 255.0).round() as u8,
                r.round() as u8,
                gc.round() as u8,
                b.round() as u8,
            ]);
        }
    }
    ksni::Icon {
        width: size as i32,
        height: size as i32,
        data,
    }
}

fn rrect_sdf(x: f32, y: f32, (l, t, r, b): (f32, f32, f32, f32), rad: f32) -> f32 {
    let (cx, cy) = ((l + r) / 2.0, (t + b) / 2.0);
    let (hx, hy) = ((r - l) / 2.0 - rad, (b - t) / 2.0 - rad);
    let (qx, qy) = ((x - cx).abs() - hx, (y - cy).abs() - hy);
    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - rad
}

fn seg_dist(x: f32, y: f32, (ax, ay): (f32, f32), (bx, by): (f32, f32)) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let t = (((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    ((x - ax - t * dx).powi(2) + (y - ay - t * dy).powi(2)).sqrt()
}

fn in_tri(x: f32, y: f32, a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    let s = |p: (f32, f32), q: (f32, f32)| (x - q.0) * (p.1 - q.1) - (p.0 - q.0) * (y - q.1);
    let (d1, d2, d3) = (s(a, b), s(b, c), s(c, a));
    !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
}

/// The 22-unit glyph of `draw_icon` in the mockup's `tray.rs`, as a point test.
fn glyph_covers(g: Glyph, x: f32, y: f32) -> bool {
    let body = (6.0, 2.0, 16.0, 20.0);
    let ring = rrect_sdf(x, y, body, 5.0).abs() <= 0.8;
    let mut on = if g == Glyph::NoDevice {
        // dashed outline, no wheel, no badge
        let ang = (y - 11.0).atan2(x - 11.0) + std::f32::consts::PI;
        ring && ((ang / (std::f32::consts::TAU / 18.0)) as u32).is_multiple_of(2)
    } else {
        ring || (rrect_sdf(x, y, body, 5.0) < 0.0
            && ((y - 9.5).abs() <= 0.8 && (6.8..=15.2).contains(&x)
                || seg_dist(x, y, (11.0, 2.8), (11.0, 9.5)) <= 0.8
                || ((10.0..=12.0).contains(&x) && (4.2..=7.4).contains(&y))))
    };
    let d = ((x - 16.2).powi(2) + (y - 16.2).powi(2)).sqrt();
    match g {
        Glyph::Low | Glyph::Charging => {
            if d <= 5.8 {
                on = d <= 4.4;
            }
            if g == Glyph::Low {
                if seg_dist(x, y, (16.2, 13.6), (16.2, 16.3)) <= 0.8
                    || ((x - 16.2).powi(2) + (y - 18.4).powi(2)).sqrt() <= 0.9
                {
                    on = false;
                }
            } else if in_tri(x, y, (17.0, 12.8), (14.4, 16.6), (16.3, 16.6))
                || in_tri(x, y, (16.1, 15.8), (18.0, 15.8), (15.4, 19.6))
            {
                on = false;
            }
        }
        Glyph::Remap if d <= 4.6 => on = d <= 2.9,
        _ => {}
    }
    on
}

// ------------------------------------------------------------------------------------------------ the process-level tray

/// Messages from the tray menu to whoever owns the window (or, once it is closed, to `linger`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    Show,
    Quit,
}

struct Shared {
    snap: Mutex<Snapshot>,
    cmds: Sender<Cmd>,
    /// Set once there is a window to wake when a command arrives.
    repaint: Mutex<Option<egui::Context>>,
    /// Wakes the poller early (after a menu action) instead of waiting out its interval.
    poke: Mutex<Sender<()>>,
    /// What closing the window decided; read by `main` once the event loop has returned.
    outcome: Mutex<Option<CloseAction>>,
}

impl Shared {
    fn send(&self, c: Cmd) {
        let _ = self.cmds.send(c);
        if let Some(ctx) = self
            .repaint
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            ctx.request_repaint();
        }
    }
    fn snapshot(&self) -> Snapshot {
        self.snap
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

struct Item {
    shared: Arc<Shared>,
    /// Cleared while the StatusNotifierWatcher is gone (shell restart, extension disabled).
    online: Arc<AtomicBool>,
}

fn run_act(shared: &Arc<Shared>, act: Act) {
    match act {
        Act::Show => shared.send(Cmd::Show),
        Act::Quit => shared.send(Cmd::Quit),
        Act::StartRemap | Act::StopRemap | Act::StopMacros => {
            // systemctl takes a moment and the menu callback must not block; read the result back right after
            let poke = shared
                .poke
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            std::thread::spawn(move || {
                let sys = RealSys;
                let r = match act {
                    Act::StartRemap => service::systemctl(&sys, &["start", SERVICE]).map(|_| ()),
                    Act::StopRemap => service::systemctl(&sys, &["stop", SERVICE]).map(|_| ()),
                    _ => service::stop_all_macros(&sys),
                };
                if let Err(e) = r {
                    eprintln!("tray: {e}");
                }
                let _ = poke.send(());
            });
        }
    }
}

impl ksni::Tray for Item {
    fn id(&self) -> String {
        "razerctl".into()
    }
    fn title(&self) -> String {
        "razerctl".into()
    }
    fn category(&self) -> ksni::Category {
        ksni::Category::Hardware
    }
    fn status(&self) -> ksni::Status {
        if glyph(&self.shared.snapshot()) == Glyph::Low {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }
    fn icon_name(&self) -> String {
        glyph(&self.shared.snapshot()).icon_name().into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let g = glyph(&self.shared.snapshot());
        vec![pixmap(g, 22), pixmap(g, 32)]
    }
    fn attention_icon_name(&self) -> String {
        Glyph::Low.icon_name().into()
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: tooltip(&self.shared.snapshot()),
            ..Default::default()
        }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.shared.send(Cmd::Show);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{MenuItem, StandardItem};
        menu_rows(&self.shared.snapshot())
            .into_iter()
            .map(|r| match r {
                Row::Sep => MenuItem::Separator,
                Row::Info(t) => StandardItem {
                    label: t,
                    enabled: false,
                    ..Default::default()
                }
                .into(),
                Row::Action {
                    label,
                    act,
                    enabled,
                } => StandardItem {
                    label: label.into(),
                    enabled,
                    activate: Box::new(move |this: &mut Self| run_act(&this.shared, act)),
                    ..Default::default()
                }
                .into(),
            })
            .collect()
    }
    fn watcher_online(&self) {
        self.online.store(true, Ordering::Relaxed);
    }
    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        self.online.store(false, Ordering::Relaxed);
        true
    }
}

type TrayHandle = ksni::blocking::Handle<Item>;

/// The tray plus its poller. Cheap to create; `probe` is what answers "is there a StatusNotifier host right now".
#[derive(Clone)]
pub struct Host {
    shared: Arc<Shared>,
    /// Menu commands. Read by the window while there is one, then by `linger`.
    cmds: Arc<Mutex<Receiver<Cmd>>>,
    handle: Arc<Mutex<Option<TrayHandle>>>,
    online: Arc<AtomicBool>,
    /// Test/demo override of `probe`.
    forced: Option<bool>,
}

impl Host {
    /// Start the poller and try to put the icon up (in the background). A missing host is not an error: `probe` tries
    /// again at close time.
    pub fn start() -> Host {
        let (tx, rx) = mpsc::channel();
        let (poke_tx, poke_rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            snap: Mutex::new(Snapshot::default()),
            cmds: tx,
            repaint: Mutex::new(None),
            poke: Mutex::new(poke_tx),
            outcome: Mutex::new(None),
        });
        let host = Host {
            shared,
            cmds: Arc::new(Mutex::new(rx)),
            handle: Arc::new(Mutex::new(None)),
            online: Arc::new(AtomicBool::new(true)),
            forced: None,
        };
        // off the main thread: registering with a slow or hung StatusNotifierWatcher must not hold up the window
        let bg = host.clone();
        std::thread::Builder::new()
            .name("tray register".into())
            .spawn(move || {
                bg.try_spawn();
            })
            .ok();
        let (shared, handle) = (host.shared.clone(), host.handle.clone());
        std::thread::Builder::new()
            .name("tray poller".into())
            .spawn(move || poll_forever(&shared, &handle, &poke_rx))
            .ok();
        host
    }

    /// A host that answers `probe` with `present` and never touches D-Bus (demo shots, tests).
    pub fn fake(present: bool) -> Host {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            snap: Mutex::new(Snapshot::default()),
            cmds: tx,
            repaint: Mutex::new(None),
            poke: Mutex::new(mpsc::channel().0),
            outcome: Mutex::new(None),
        });
        Host {
            shared,
            cmds: Arc::new(Mutex::new(rx)),
            handle: Arc::new(Mutex::new(None)),
            online: Arc::new(AtomicBool::new(true)),
            forced: Some(present),
        }
    }

    /// Commands that arrived since the last call.
    pub fn drain(&self) -> Vec<Cmd> {
        self.cmds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .try_iter()
            .collect()
    }

    fn try_spawn(&self) -> bool {
        use ksni::blocking::TrayMethods;
        let mut h = self.handle.lock().unwrap_or_else(PoisonError::into_inner);
        if h.as_ref().is_some_and(|h| !h.is_closed()) {
            return true;
        }
        self.online.store(true, Ordering::Relaxed);
        match (Item {
            shared: self.shared.clone(),
            online: self.online.clone(),
        })
        .spawn()
        {
            Ok(handle) => {
                *h = Some(handle);
                true
            }
            Err(e) => {
                *h = None;
                eprintln!("tray: no StatusNotifier host ({e})");
                false
            }
        }
    }

    /// Is the icon on screen right now? Tries to bring it up if it is not (a shell that started after us, an
    /// extension enabled since): the answer `close_action` needs is the one at the moment of closing.
    pub fn probe(&self) -> bool {
        match self.forced {
            Some(p) => p,
            None => self.try_spawn() && self.present(),
        }
    }

    /// No retry: is the icon there and its watcher online?
    pub fn present(&self) -> bool {
        match self.forced {
            Some(p) => p,
            None => {
                self.online.load(Ordering::Relaxed)
                    && self
                        .handle
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .as_ref()
                        .is_some_and(|h| !h.is_closed())
            }
        }
    }

    pub fn attach_window(&self, ctx: &egui::Context) {
        *self
            .shared
            .repaint
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(ctx.clone());
    }

    pub fn set_outcome(&self, a: CloseAction) {
        *self
            .shared
            .outcome
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(a);
    }

    pub fn outcome(&self) -> Option<CloseAction> {
        *self
            .shared
            .outcome
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Re-read the world soon (after the window started or stopped the service).
    pub fn poke(&self) {
        let _ = self
            .shared
            .poke
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(());
    }
}

/// Battery and remap state every 30 s (and on a poke), whether or not a window exists: this is the "keep polling
/// while hidden" requirement, and it is a plain thread so it cannot depend on a frame being drawn.
fn poll_forever(
    shared: &Arc<Shared>,
    handle: &Arc<Mutex<Option<TrayHandle>>>,
    poke: &Receiver<()>,
) {
    const EVERY: Duration = Duration::from_secs(30);
    let sys = RealSys;
    loop {
        let threshold = battery::load().threshold;
        let read = battery::read_battery().map_err(|e| e.to_string());
        let unit = service::systemctl(&sys, &["is-active", SERVICE])
            .map(|o| o.stdout.trim().to_string())
            .unwrap_or_default();
        let snap = snapshot_from(read, &unit, threshold);
        let changed = {
            let mut cur = shared.snap.lock().unwrap_or_else(PoisonError::into_inner);
            let changed = *cur != snap;
            *cur = snap;
            changed
        };
        if changed {
            if let Some(h) = handle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
            {
                h.update(|_| {}); // re-emit icon, tooltip and menu from the new snapshot
            }
        }
        match poke.recv_timeout(EVERY) {
            Ok(()) => std::thread::sleep(Duration::from_millis(400)), // let the unit finish changing state first
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// After the window is gone and the decision was `Hide`: keep the process (tray icon, poller) alive until the user
/// asks for the window back (re-exec) or quits. If the icon disappears (shell stopped hosting it) the window comes
/// back rather than leaving a process nobody can see.
pub fn linger(host: &Host) -> Cmd {
    let mut gone = 0;
    loop {
        let got = host
            .cmds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recv_timeout(Duration::from_secs(2));
        match got {
            Ok(c) => return c,
            Err(RecvTimeoutError::Disconnected) => return Cmd::Quit,
            Err(RecvTimeoutError::Timeout) => {
                gone = if host.present() { 0 } else { gone + 1 };
                if gone >= 5 {
                    eprintln!(
                        "tray: the icon is gone; reopening the window instead of running unseen"
                    );
                    return Cmd::Show;
                }
            }
        }
    }
}

/// Replace this process by a fresh GUI. Returns only if that failed.
pub fn reopen_window() -> std::io::Error {
    use std::os::unix::process::CommandExt;
    // the tray menu / SNI Activate call that asked for this is answered on a thread of this process: let the reply go
    // out before exec closes the D-Bus connection under it
    std::thread::sleep(Duration::from_millis(250));
    match std::env::current_exe() {
        Ok(exe) => std::process::Command::new(exe)
            .args(std::env::args_os().skip(1))
            .exec(),
        Err(e) => e,
    }
}

/// The one-time notice (`app-firstclose-*.png`): through the same `notify-send` path as the battery alert. The flag is
/// only set when the notice was actually sent, so a failure tries again on the next close.
pub fn first_close_notice(path: &Path, s: &mut Settings) {
    if s.notified {
        return;
    }
    let sent = battery::notify(
        "razerctl is still running",
        "The window is hidden, not quit. It stays in the system tray so low-battery alerts keep arriving. To quit, use the tray menu: Quit razerctl.",
        false,
    );
    match sent {
        Ok(()) => {
            s.notified = true;
            if let Err(e) = save_settings(path, s) {
                eprintln!("tray: could not remember the first-close notice: {e}");
            }
        }
        Err(e) => eprintln!("tray: first-close notice not sent: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(pct: u8, charging: bool) -> Option<Reading> {
        Some(Reading {
            name: "Viper Mini SE".into(),
            pct: Some(pct),
            charging,
        })
    }
    fn snap(device: Option<Reading>, remap: RemapLine) -> Snapshot {
        Snapshot {
            device,
            remap,
            threshold: 20,
        }
    }

    #[test]
    fn close_decision_covers_setting_times_host() {
        use CloseAction::*;
        // (close_to_tray, host present) -> action
        assert_eq!(close_action(true, true), Hide);
        assert_eq!(
            close_action(true, false),
            Quit,
            "no StatusNotifier host: nowhere to hide, so a real quit"
        );
        assert_eq!(close_action(false, true), Quit);
        assert_eq!(close_action(false, false), Quit);
    }

    #[test]
    fn default_setting_is_minimise_to_tray_and_survives_a_round_trip() {
        assert!(Settings::default().close_to_tray);
        assert!(!Settings::default().notified);
        let s = Settings {
            close_to_tray: false,
            notified: true,
        };
        assert_eq!(parse_settings(&format_settings(&s)), s);
        assert_eq!(
            parse_settings("garbage\nclose_to_tray = maybe"),
            Settings {
                close_to_tray: false,
                notified: false
            }
        );
        assert_eq!(parse_settings(""), Settings::default());
    }

    #[test]
    fn icon_priority_is_no_mouse_then_low_then_charging_then_remap_then_normal() {
        let g = |d, r| glyph(&snap(d, r));
        assert_eq!(g(None, RemapLine::Running), Glyph::NoDevice);
        assert_eq!(g(dev(14, false), RemapLine::Running), Glyph::Low);
        assert_eq!(
            g(dev(14, true), RemapLine::Running),
            Glyph::Charging,
            "low while charging is just charging"
        );
        assert_eq!(g(dev(82, true), RemapLine::Running), Glyph::Charging);
        assert_eq!(g(dev(82, false), RemapLine::Running), Glyph::Remap);
        assert_eq!(g(dev(82, false), RemapLine::Stopped), Glyph::Normal);
        assert_eq!(
            g(dev(20, false), RemapLine::Stopped),
            Glyph::Low,
            "at the threshold counts as low"
        );
        assert_eq!(g(dev(21, false), RemapLine::Stopped), Glyph::Normal);
    }

    #[test]
    fn tooltips_match_the_sheet() {
        assert_eq!(
            tooltip(&snap(None, RemapLine::Stopped)),
            "razerctl · no mouse found"
        );
        assert_eq!(
            tooltip(&snap(dev(14, false), RemapLine::Stopped)),
            "razerctl · Viper Mini SE · 14% (low)"
        );
        assert_eq!(
            tooltip(&snap(dev(82, true), RemapLine::Stopped)),
            "razerctl · Viper Mini SE · 82% (charging)"
        );
        assert_eq!(
            tooltip(&snap(dev(82, false), RemapLine::Running)),
            "razerctl · Viper Mini SE · 82% · remap running"
        );
        assert_eq!(
            tooltip(&snap(dev(82, false), RemapLine::Stopped)),
            "razerctl · Viper Mini SE · 82%"
        );
    }

    fn labels(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                Row::Sep => "---".into(),
                Row::Info(t) => format!("i {t}"),
                Row::Action {
                    label,
                    enabled: true,
                    ..
                } => (*label).into(),
                Row::Action {
                    label,
                    enabled: false,
                    ..
                } => format!("({label})"),
            })
            .collect()
    }

    #[test]
    fn menu_states_a_b_c_of_the_sheet() {
        // A: mouse found, remap running
        assert_eq!(
            labels(&menu_rows(&snap(dev(82, false), RemapLine::Running))),
            [
                "Show razerctl",
                "---",
                "i Viper Mini SE · 82%",
                "---",
                "i ● Remap running",
                "Stop remap",
                "Stop all macros",
                "---",
                "Quit razerctl"
            ]
        );
        // B: low battery, remap stopped: Stop all macros greyed, the action is the opposite of the line
        assert_eq!(
            labels(&menu_rows(&snap(dev(14, false), RemapLine::Stopped))),
            [
                "Show razerctl",
                "---",
                "i ▲ Viper Mini SE · 14% · low",
                "---",
                "i ○ Remap stopped",
                "Start remap",
                "(Stop all macros)",
                "---",
                "Quit razerctl"
            ]
        );
        // C: no mouse, remap waiting for it
        assert_eq!(
            labels(&menu_rows(&snap(None, RemapLine::Waiting))),
            [
                "Show razerctl",
                "---",
                "i ○ No mouse found",
                "---",
                "i ○ Remap waiting for the mouse",
                "Stop remap",
                "(Stop all macros)",
                "---",
                "Quit razerctl"
            ]
        );
    }

    #[test]
    fn menu_never_offers_more_than_five_actions_and_charging_is_named() {
        let rows = menu_rows(&snap(dev(82, true), RemapLine::Failed));
        assert_eq!(
            rows.iter()
                .filter(|r| matches!(r, Row::Action { .. }))
                .count(),
            4
        );
        let l = labels(&rows);
        assert!(
            l.contains(&"i Viper Mini SE · 82% · charging".to_string()),
            "{l:?}"
        );
        assert!(
            l.contains(&"i × Remap failed".to_string()) && l.contains(&"Start remap".to_string()),
            "{l:?}"
        );
    }

    #[test]
    fn a_reading_becomes_a_snapshot() {
        let s = snapshot_from(Ok(("Razer Viper Mini SE".into(), 82, false)), "active", 20);
        assert_eq!(
            (s.device.as_ref().map(|d| d.name.as_str()), s.remap),
            (Some("Viper Mini SE"), RemapLine::Running)
        );
        assert_eq!(
            snapshot_from(Err("no Razer device".into()), "active", 20).remap,
            RemapLine::Waiting
        );
        assert_eq!(
            snapshot_from(Err("x".into()), "failed", 20).remap,
            RemapLine::Failed
        );
        assert_eq!(
            snapshot_from(Ok(("M".into(), 5, false)), "inactive", 20).remap,
            RemapLine::Stopped
        );
    }

    #[test]
    fn show_and_quit_only_message_the_window_and_start_no_systemctl() {
        // The tray's Quit stops the GUI and nothing else (AC-T3): Show / Quit are plain messages, and only the three
        // remap actions spawn systemctl, which are on SERVICE alone. The battery notifier unit is never named here.
        let host = Host::fake(true);
        run_act(&host.shared, Act::Quit);
        run_act(&host.shared, Act::Show);
        assert_eq!(host.drain(), [Cmd::Quit, Cmd::Show]);
        let src = include_str!("tray.rs");
        let code = src.split("#[cfg(test)]").next().unwrap();
        assert!(
            !code.contains("battery::SERVICE") && !code.contains("razerctl-battery"),
            "the tray must not touch the notifier unit"
        );
    }

    #[test]
    fn status_is_never_passive_and_only_low_asks_for_attention() {
        let item = |s: Snapshot| {
            let shared = Arc::new(Shared {
                snap: Mutex::new(s),
                cmds: mpsc::channel().0,
                repaint: Mutex::new(None),
                poke: Mutex::new(mpsc::channel().0),
                outcome: Mutex::new(None),
            });
            Item {
                shared,
                online: Arc::new(AtomicBool::new(true)),
            }
        };
        use ksni::Tray;
        for (s, want) in [
            (snap(None, RemapLine::Stopped), "Active"),
            (snap(dev(82, false), RemapLine::Running), "Active"),
            (snap(dev(10, false), RemapLine::Stopped), "NeedsAttention"),
        ] {
            assert_eq!(item(s).status().to_string(), want);
        }
    }

    #[test]
    fn pixmaps_are_argb_at_both_sizes_and_the_five_glyphs_differ() {
        let mut seen = Vec::new();
        for g in Glyph::ALL {
            for size in [22u32, 32] {
                let p = pixmap(g, size);
                assert_eq!(
                    (p.width, p.height, p.data.len()),
                    (size as i32, size as i32, (size * size * 4) as usize)
                );
                assert!(
                    p.data.chunks(4).any(|px| px[0] > 200),
                    "{g:?} {size}: nothing drawn"
                );
            }
            let p = pixmap(g, 22);
            assert!(!seen.contains(&p.data), "{g:?} looks like another state");
            seen.push(p.data);
        }
        // the 16 px size of the sheet must keep Low and Charging apart too
        assert_ne!(
            pixmap(Glyph::Low, 16).data,
            pixmap(Glyph::Charging, 16).data
        );
    }
}
