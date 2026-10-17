//! The update check in the window (DAN-1669, designed in DAN-1667): the `03 // UPDATES` panel on the App tab, the banner
//! under the header, and the update dialog. What happens, and when, is `razerctl::update`; this file is the egui side and
//! the two background threads (one check, or one download). Nothing here replaces a file until the user pressed *Update*.

use super::theme::*;
use super::widgets::*;
use eframe::egui::{self, vec2, Align, Color32, Frame, Layout, Margin, RichText, Stroke, Ui};
use razerctl::update::{
    self, Applied, CheckResult, InstallType, Outcome, Paths, Release, Source, Stage, State,
    UpdateError, Version,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What the dialog is showing. `Offer` is the only one that asks anything; the rest report on an update the user chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dialog {
    Offer,
    Working(Stage),
    /// The new file is in place (`old` is the previous one) and a restart is waiting.
    Done {
        path: PathBuf,
        old: Option<PathBuf>,
    },
    /// Verified, but the running file could not be replaced: saved at `path` instead.
    Saved {
        path: PathBuf,
        why: String,
    },
    Failed(UpdateError),
}

enum Msg {
    Checked { manual: bool, result: CheckResult },
    Progress(Stage),
    Applied(Result<Applied, UpdateError>),
}

/// What the window must do on the dialog's behalf.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Quit and start the new file. The dialog has already told the user which unsaved edits that throws away.
    Restart { path: PathBuf },
}

pub struct Updater {
    pub install: InstallType,
    pub current: Version,
    src: Source,
    paths: Paths,
    /// Background checks allowed. Off in tests and under the visual-review hooks, so they never reach the network.
    pub auto: bool,
    state: State,
    release: Option<Release>,
    pub dialog: Option<Dialog>,
    rx: Option<Receiver<Msg>>,
    checking: bool,
    last_eval: Option<Instant>,
    cancel: Arc<AtomicBool>,
    /// Set by the Check now button: the answer is told in the status bar (an automatic check says nothing).
    pub say: Option<String>,
}

impl Updater {
    pub fn new() -> Updater {
        Updater::with(
            Paths::default_paths(),
            Source::from_env(),
            InstallType::detect(),
            update::current_version(),
        )
    }

    pub fn with(paths: Paths, src: Source, install: InstallType, current: Version) -> Updater {
        let state = paths.load();
        let release = update::pending_release(&paths, &current);
        Updater {
            install,
            current,
            src,
            paths,
            auto: false,
            state,
            release,
            dialog: None,
            rx: None,
            checking: false,
            last_eval: None,
            cancel: Arc::new(AtomicBool::new(false)),
            say: None,
        }
    }

    /// Demo and test hook: put the updater in a given state without any request.
    pub fn stage(&mut self, release: Option<Release>, state: State, dialog: Option<Dialog>) {
        self.release = release;
        self.state = state;
        self.dialog = dialog;
    }

    /// Once per frame: take what a background thread sent, and start the automatic check when it is due.
    pub fn tick(&mut self, ctx: &egui::Context, enabled: bool) {
        while let Some(m) = self.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            match m {
                Msg::Checked { manual, result } => {
                    self.checking = false;
                    self.state = self.paths.load();
                    self.release = update::pending_release(&self.paths, &self.current);
                    if manual {
                        self.say = match &result {
                            CheckResult::Available(r) => {
                                Some(format!("razerctl {} is available.", r.version))
                            }
                            CheckResult::UpToDate(_) => Some("razerctl is up to date.".into()),
                            // offline and rate limited are shown on the App tab and nowhere else
                            CheckResult::Failed(_) => None,
                        };
                    }
                }
                Msg::Progress(s) => {
                    if matches!(self.dialog, Some(Dialog::Working(_))) {
                        self.dialog = Some(Dialog::Working(s));
                    }
                }
                Msg::Applied(r) => {
                    self.dialog = Some(match r {
                        Ok(Applied::Replaced { path, old }) => Dialog::Done { path, old },
                        Ok(Applied::Saved { path, why }) => Dialog::Saved { path, why },
                        Err(UpdateError::Cancelled) => Dialog::Offer,
                        Err(e) => Dialog::Failed(e),
                    });
                }
            }
        }
        if !self.auto || !enabled || self.checking {
            return;
        }
        if self
            .last_eval
            .is_some_and(|t| t.elapsed() < Duration::from_secs(600))
        {
            return;
        }
        self.last_eval = Some(Instant::now());
        ctx.request_repaint_after(Duration::from_secs(601));
        if self.paths.load().due(update::now_secs()) {
            self.start_check(ctx, false);
        }
    }

    /// One request on its own thread; `manual` skips the 24 hour throttle by not consulting it.
    pub fn start_check(&mut self, ctx: &egui::Context, manual: bool) {
        if self.checking {
            return;
        }
        self.checking = true;
        let (tx, rx) = channel();
        self.rx = Some(rx);
        let (src, paths, current, ctx) = (
            self.src.clone(),
            self.paths.clone(),
            self.current.clone(),
            ctx.clone(),
        );
        std::thread::spawn(move || {
            let result = update::check_and_record(&src, &paths, &current, update::now_secs());
            let _ = tx.send(Msg::Checked { manual, result });
            ctx.request_repaint();
        });
    }

    /// Download, verify and swap on its own thread. Only ever called from the *Update* button (or *Try again*).
    fn start_apply(&mut self, ctx: &egui::Context) {
        let (InstallType::AppImage { path }, Some(rel)) =
            (self.install.clone(), self.release.clone())
        else {
            return;
        };
        self.cancel.store(false, Ordering::Relaxed);
        self.dialog = Some(Dialog::Working(Stage::Downloading { done: 0, total: 0 }));
        let (tx, rx) = channel();
        self.rx = Some(rx);
        let (src, cancel, ctx) = (self.src.clone(), self.cancel.clone(), ctx.clone());
        std::thread::spawn(move || {
            let mut progress = |s: Stage| {
                let _ = tx.send(Msg::Progress(s));
                ctx.request_repaint();
            };
            let r = update::apply_appimage(
                &src,
                &rel,
                &path,
                &update::fallback_dir(),
                &cancel,
                &mut progress,
            );
            let _ = tx.send(Msg::Applied(r));
            ctx.request_repaint();
        });
    }

    fn close(&mut self) {
        self.dialog = None;
    }

    /// *Not now*, Esc on the offer, or *Close* on a package install: the banner stays away for a day.
    fn not_now(&mut self) {
        update::dismiss(&self.paths, update::now_secs());
        self.state = self.paths.load();
        self.dialog = None;
    }

    /// The banner is for a release the user has not turned down in the last day, while no dialog is open.
    pub fn banner_release(&self, enabled: bool) -> Option<&Release> {
        let r = self.release.as_ref()?;
        (enabled && self.dialog.is_none() && self.state.banner_wanted(update::now_secs()))
            .then_some(r)
    }
}

// ------------------------------------------------------------------ words

fn install_name(i: &InstallType) -> &'static str {
    match i {
        InstallType::AppImage { .. } => "AppImage",
        InstallType::Tarball => "Tarball / install.sh",
        InstallType::Package => "System package",
    }
}

/// Where this copy lives, as the user would type it: `$HOME` shown as `~`.
fn location(install: &InstallType) -> String {
    let path = match install {
        InstallType::AppImage { path } => path.clone(),
        _ => std::env::current_exe().unwrap_or_default(),
    };
    let home = std::env::var("HOME").unwrap_or_default();
    match path.strip_prefix(&home) {
        Ok(rest) if !home.is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

fn switch_text(i: &InstallType) -> &'static str {
    match i {
        InstallType::Package => "Tell me when a new version is released",
        _ => "Check for updates",
    }
}

fn explain(i: &InstallType, enabled: bool) -> &'static str {
    match (i, enabled) {
        (InstallType::AppImage { .. }, true) => "razerctl asks GitHub once a day whether a newer release exists. You are shown the changes first, and nothing is downloaded until you press Update.",
        (InstallType::Tarball, true) => "razerctl asks GitHub once a day whether a newer release exists and tells you. It cannot replace itself here: you update by hand from the release page.",
        (InstallType::Package, false) => "Updates come from your system package manager (yay or paru). Turn this on only if you want to be told when a release is out: razerctl never replaces files under /usr.",
        (InstallType::Package, true) => "razerctl asks GitHub once a day and tells you when a release is out. Updates still come from your package manager (yay or paru); razerctl never replaces files under /usr.",
        (_, false) => "razerctl will not contact GitHub. You can still get new versions from the release page.",
    }
}

struct Tm {
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    min: u32,
}

fn local(secs: u64) -> Option<Tm> {
    let t = secs as libc::time_t;
    // SAFETY: localtime_r writes only into the tm it is given, and is called with a valid pointer to each.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return None;
        }
        tm
    };
    Some(Tm {
        year: tm.tm_year + 1900,
        month: tm.tm_mon as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        min: tm.tm_min as u32,
    })
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// "Today, 09:41", "Yesterday, 09:41", or "14 Oct, 09:41" in the local time zone.
pub fn when(then: u64, now: u64) -> String {
    let (Some(t), Some(n)) = (local(then), local(now)) else {
        return "At an unknown time".into();
    };
    let day = |tm: &Tm| (tm.year, tm.month, tm.day);
    let yesterday = local(now.saturating_sub(24 * 3600)).is_some_and(|y| day(&y) == day(&t));
    let name = if day(&t) == day(&n) {
        "Today".to_string()
    } else if yesterday {
        "Yesterday".to_string()
    } else {
        format!("{} {}", t.day, MONTHS[t.month as usize % 12])
    };
    format!("{name}, {:02}:{:02}", t.hour, t.min)
}

/// `2026-10-17T15:23:45Z` as "17 Oct 2026".
fn release_date(published: &str) -> Option<String> {
    let mut it = published.get(..10)?.split('-');
    let (y, m, d): (&str, usize, u32) = (
        it.next()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    );
    Some(format!("{d} {} {y}", MONTHS.get(m.checked_sub(1)?)?))
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

#[derive(Debug, PartialEq, Eq)]
enum Block {
    Gap,
    Para(String),
    Bullet(String),
}

/// The notes as the box draws them: a `- ` or `* ` line starts a bullet, a blank line is a gap, and any other line joins the
/// block above it (the CHANGELOG is hard-wrapped, so a bullet's second line must not become a paragraph of its own).
fn blocks(notes: &str) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    for line in notes.lines() {
        let t = line.trim();
        if t.is_empty() {
            if !matches!(out.last(), Some(Block::Gap) | None) {
                out.push(Block::Gap);
            }
        } else if let Some(b) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            out.push(Block::Bullet(plain(b)));
        } else {
            match out.last_mut() {
                Some(Block::Bullet(s) | Block::Para(s)) => {
                    s.push(' ');
                    s.push_str(&plain(t));
                }
                _ => out.push(Block::Para(plain(t))),
            }
        }
    }
    out
}

/// A line of release notes as plain text: heading marks and emphasis dropped.
fn plain(line: &str) -> String {
    line.trim_start_matches('#')
        .trim()
        .replace("**", "")
        .replace('`', "")
}

// ------------------------------------------------------------------ small pieces

fn body(ui: &mut Ui, p: &Pal, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).font(sans(T_BODY - 1.0)).color(p.i65())).wrap());
}

fn small(ui: &mut Ui, p: &Pal, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).font(sans(T_SMALL)).color(p.i65())).wrap());
}

/// A read-only mono block on the sunken surface (paths, commands, hashes). Same surface as the macro editor.
fn code_block(ui: &mut Ui, p: &Pal, lines: &[(&str, &str)]) {
    Frame::new()
        .fill(p.sunken)
        .stroke(Stroke::new(1.0, p.i28()))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 4.0;
            for (k, v) in lines {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = S3;
                    if !k.is_empty() {
                        ui.allocate_ui_with_layout(
                            vec2(64.0, 16.0),
                            Layout::left_to_right(Align::Center),
                            |ui| {
                                ui.set_min_width(64.0);
                                ui.label(caps(p, k));
                            },
                        );
                    }
                    ui.add(
                        egui::Label::new(RichText::new(*v).font(mono(T_DATA)).color(p.i100()))
                            .wrap(),
                    );
                });
            }
        });
}

fn callout(ui: &mut Ui, p: &Pal, glyph_color: Color32, glyph: &str, head: &str, text: &str) {
    Frame::new()
        .fill(p.i06())
        .stroke(Stroke::new(1.0, p.i45()))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = S2;
                ui.label(
                    RichText::new(glyph)
                        .font(mono(T_LABEL + 1.0))
                        .color(glyph_color),
                );
                ui.label(
                    RichText::new(head)
                        .font(serif(T_TITLE - 4.0))
                        .color(p.i100()),
                );
            });
            ui.add_space(2.0);
            body(ui, p, text);
        });
}

#[derive(Clone, Copy, PartialEq)]
enum S {
    Done,
    Now,
    Wait,
    Fail,
    Skip,
}

/// One line of the 4-step list (Download, Check, Replace, Restart). Glyphs follow the chip set: ● done ◐ now ○ waiting × failed.
fn step(ui: &mut Ui, p: &Pal, st: S, label: &str, detail: &str) {
    let (g, gc, tc) = match st {
        S::Done => ("●", p.ok, p.i100()),
        S::Now => ("◐", p.i100(), p.i100()),
        S::Wait => ("○", p.i65(), p.i65()),
        S::Fail => ("×", p.bad, p.i100()),
        S::Skip => ("–", p.i65(), p.i65()),
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = S3;
        ui.allocate_ui_with_layout(
            vec2(16.0, 22.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.set_min_width(16.0);
                ui.label(RichText::new(g).font(mono(T_BODY)).color(gc));
            },
        );
        ui.label(RichText::new(label).font(sans(T_BODY - 1.0)).color(tc));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(detail).font(mono(T_DATA)).color(p.i65()));
        });
    });
}

// ------------------------------------------------------------------ App tab: the Updates panel

/// 03 // UPDATES. Returns the switch's new position when it was just toggled. Reuses `panel`, `switch`, `kv`, `hair`,
/// `chip` and `btn`.
pub fn panel_ui(ui: &mut Ui, p: &Pal, u: &mut Updater, enabled: bool) -> Option<bool> {
    let mut toggled = None;
    let mut check_now = false;
    let mut view = false;
    let now = update::now_secs();
    panel(ui, p, 16, |ui| {
        let total = ui.available_width();
        if total >= 700.0 {
            // wide: what it does on the left, what it found on the right
            let gap = S5;
            let lw = ((total - gap) * 0.46).floor();
            let row = ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                ui.allocate_ui_with_layout(vec2(lw, 0.0), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(lw);
                    toggled = left(ui, p, u, enabled);
                });
                ui.allocate_ui_with_layout(
                    vec2(total - lw - gap, 0.0),
                    Layout::top_down(Align::Min),
                    |ui| {
                        ui.set_width(total - lw - gap);
                        (check_now, view) = right(ui, p, u, enabled, now);
                    },
                );
            });
            let r = row.response.rect;
            ui.painter().vline(
                r.left() + lw + gap / 2.0,
                r.y_range(),
                Stroke::new(1.0, p.i18()),
            );
        } else {
            toggled = left(ui, p, u, enabled);
            ui.add_space(S3);
            hair(ui, p);
            ui.add_space(S3);
            (check_now, view) = right(ui, p, u, enabled, now);
        }
    });
    if check_now {
        u.say = None;
        u.start_check(&ui.ctx().clone(), true);
    }
    if view && u.release.is_some() {
        u.dialog = Some(Dialog::Offer);
    }
    toggled
}

fn left(ui: &mut Ui, p: &Pal, u: &Updater, enabled: bool) -> Option<bool> {
    let mut on = enabled;
    let r = switch(ui, p, &mut on, switch_text(&u.install));
    ui.add_space(S2);
    body(ui, p, explain(&u.install, enabled));
    r.changed().then_some(on)
}

/// Returns (Check now pressed, View update pressed).
fn right(ui: &mut Ui, p: &Pal, u: &Updater, enabled: bool, now: u64) -> (bool, bool) {
    kv(ui, p, "Installed as", 112.0, |ui| {
        ui.label(
            RichText::new(install_name(&u.install))
                .font(sans(T_BODY - 1.0))
                .color(p.i100()),
        );
    });
    kv(ui, p, "Version", 112.0, |ui| {
        ui.label(
            RichText::new(u.current.to_string())
                .font(mono(T_DATA + 1.0))
                .color(p.i100()),
        );
    });
    kv(ui, p, "Location", 112.0, |ui| {
        ui.add(
            egui::Label::new(
                RichText::new(location(&u.install))
                    .font(mono(T_DATA))
                    .color(p.i65()),
            )
            .truncate(),
        );
    });
    ui.add_space(S2);
    hair(ui, p);
    ui.add_space(S2);
    let waiting = enabled && u.release.is_some();
    let (mut check_now, mut view) = (false, false);
    let stack = ui.available_width() < 440.0; // narrow cell: the button drops under the chip instead of colliding with it
    let mut action = |ui: &mut Ui| {
        if waiting {
            view = btn(ui, p, "View update", Kind::Primary, true).clicked();
        } else {
            check_now = btn(ui, p, "Check now", Kind::Secondary, enabled && !u.checking).clicked();
        }
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = S3;
        ui.allocate_ui_with_layout(
            vec2(112.0, 28.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.set_min_width(112.0);
                ui.label(caps(p, "Last check"));
            },
        );
        last_check_chip(ui, p, u, enabled);
        if !stack {
            ui.with_layout(Layout::right_to_left(Align::Center), &mut action);
        }
    });
    if stack {
        ui.add_space(S1);
        ui.horizontal(|ui| {
            ui.add_space(112.0 + S3);
            action(ui);
        });
    }
    if enabled {
        if let Some(line) = last_check_line(u, now) {
            ui.add_space(S1);
            small(ui, p, &line);
        }
    }
    (check_now, view)
}

fn last_check_chip(ui: &mut Ui, p: &Pal, u: &Updater, enabled: bool) {
    if !enabled {
        chip(ui, p, Some("○"), "Not checking", ChipStyle::Plain, p.i65());
    } else if u.checking {
        chip(ui, p, Some("◐"), "Checking", ChipStyle::Plain, p.i100());
    } else if let Some(r) = &u.release {
        chip(
            ui,
            p,
            Some("▲"),
            &format!("{} available", r.version),
            ChipStyle::Outline,
            p.warn,
        );
    } else {
        match u.state.outcome {
            Some(Outcome::UpToDate | Outcome::Available) => {
                chip(ui, p, Some("●"), "Up to date", ChipStyle::Plain, p.ok);
            }
            Some(Outcome::Offline) => {
                chip(
                    ui,
                    p,
                    Some("○"),
                    "Couldn't connect",
                    ChipStyle::Plain,
                    p.i65(),
                );
            }
            Some(Outcome::RateLimited) => {
                chip(ui, p, Some("○"), "Rate limited", ChipStyle::Plain, p.i65());
            }
            Some(Outcome::Failed) => {
                chip(
                    ui,
                    p,
                    Some("○"),
                    "Couldn't check",
                    ChipStyle::Plain,
                    p.i65(),
                );
            }
            None => {
                chip(
                    ui,
                    p,
                    Some("○"),
                    "Not checked yet",
                    ChipStyle::Plain,
                    p.i65(),
                );
            }
        }
    }
}

fn last_check_line(u: &Updater, now: u64) -> Option<String> {
    if u.checking {
        return None;
    }
    let Some(at) = u.state.last_check else {
        return Some("razerctl checks when this window opens, then once a day.".into());
    };
    let t = when(at, now);
    Some(match (&u.release, u.state.outcome) {
        (Some(_), _) if u.state.dismissed.is_some() => {
            format!("{t}. You chose Not now; razerctl will ask again tomorrow.")
        }
        (Some(_), _) => format!("{t}. A newer release is out."),
        (None, Some(Outcome::Offline)) => {
            format!("{t}. No answer from api.github.com. razerctl tries again tomorrow and shows nothing else.")
        }
        (None, Some(Outcome::RateLimited)) => {
            format!("{t}. GitHub asked razerctl to wait. It tries again tomorrow and shows nothing else.")
        }
        (None, Some(Outcome::Failed)) => {
            format!("{t}. GitHub had no release to show. razerctl tries again tomorrow and shows nothing else.")
        }
        _ => format!("{t}. Next check in about 24 hours."),
    })
}

// ------------------------------------------------------------------ the banner

/// Between the header and the device strip, while a release is waiting and was not turned down today. *See what's new*
/// opens the dialog; *Not now* keeps the banner away for a day.
pub fn banner_ui(ui: &mut Ui, p: &Pal, u: &mut Updater, enabled: bool) {
    let Some(rel) = u.banner_release(enabled) else {
        return;
    };
    let text = format!(
        "razerctl {} is available. You have {}.",
        rel.version, u.current
    );
    let (mut see, mut later) = (false, false);
    Frame::new()
        .fill(p.i06())
        .stroke(Stroke::new(1.0, p.i28()))
        .inner_margin(Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = S3;
                chip(ui, p, Some("▲"), "Update", ChipStyle::Plain, p.warn);
                ui.label(RichText::new(text).font(sans(T_BODY - 1.0)).color(p.i100()));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    later = btn(ui, p, "Not now", Kind::Secondary, true).clicked();
                    see = btn(ui, p, "See what's new", Kind::Secondary, true).clicked();
                });
            });
        });
    ui.add_space(S2);
    if see {
        u.dialog = Some(Dialog::Offer);
    }
    if later {
        u.not_now();
    }
}

// ------------------------------------------------------------------ the dialog

const W: f32 = 520.0;

/// Below 700 px of window height the terminal states drop the 4-step recap: the sentence under it says the same thing.
fn compact(ctx: &egui::Context) -> bool {
    ctx.content_rect().height() < 700.0
}

/// `unsaved` is a phrase naming the edits a restart would throw away, if there are any.
pub fn dialog_ui(
    ctx: &egui::Context,
    p: &Pal,
    u: &mut Updater,
    unsaved: Option<&str>,
) -> Option<Action> {
    let dialog = u.dialog.clone()?;
    let rel = u.release.clone()?;
    let screen = ctx.content_rect();
    let scrim = if p.mode == Mode::Dark {
        Color32::from_black_alpha(165)
    } else {
        Color32::from_rgba_unmultiplied(33, 29, 23, 96)
    };
    let compact = compact(ctx);
    // the changelog box takes what is left, so the dialog fits a 560 px high window
    let log_h = (screen.height() - 480.0).clamp(96.0, 200.0);
    let mut out = Out::default();
    let frame = Frame::new()
        .fill(p.raised)
        .stroke(Stroke::new(1.0, p.i45()))
        .inner_margin(Margin::same(24));
    let content = |ui: &mut Ui| {
        ui.set_width(W);
        dialog_body(ui, p, u, &rel, &dialog, unsaved, log_h, compact, &mut out);
    };
    let modal = egui::Modal::new(egui::Id::new("update-dialog"))
        .backdrop_color(scrim)
        .frame(frame)
        .show(ctx, content);
    // Esc is Not now / Close / Cancel / Later; a click on the scrim is not, so a stray click never dismisses it
    let esc = modal.is_top_modal
        && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
    if esc {
        out.esc = true;
    }
    finish(ctx, u, &rel, &dialog, out)
}

#[derive(Default)]
struct Out {
    update: bool,
    not_now: bool,
    open_page: bool,
    cancel: bool,
    close: bool,
    copy: Option<String>,
    restart: bool,
    later: bool,
    esc: bool,
}

fn finish(
    ctx: &egui::Context,
    u: &mut Updater,
    rel: &Release,
    dialog: &Dialog,
    o: Out,
) -> Option<Action> {
    let appimage = matches!(u.install, InstallType::AppImage { .. });
    if let Some(t) = o.copy {
        ctx.copy_text(t);
    }
    if o.open_page {
        ctx.open_url(egui::OpenUrl::new_tab(&rel.page));
    }
    match dialog {
        Dialog::Offer => {
            if o.update && appimage {
                u.start_apply(ctx);
            } else if o.not_now || o.close || o.esc || (o.open_page && !appimage) {
                u.not_now();
            }
        }
        Dialog::Working(_) => {
            if o.cancel || o.esc {
                u.cancel.store(true, Ordering::Relaxed);
            }
        }
        Dialog::Done { path, .. } => {
            if o.restart {
                return Some(Action::Restart { path: path.clone() });
            }
            if o.later || o.esc {
                u.close();
            }
        }
        Dialog::Saved { .. } => {
            if o.close || o.esc {
                u.close();
            }
        }
        Dialog::Failed(_) => {
            if o.update && appimage {
                u.start_apply(ctx);
            } else if o.close || o.esc {
                u.close();
            }
        }
    }
    None
}

fn head(ui: &mut Ui, p: &Pal, label: &str, install: &InstallType, title_text: &str) {
    ui.horizontal(|ui| {
        ui.label(caps(p, label));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            chip(
                ui,
                p,
                None,
                install_name(install),
                ChipStyle::Outline,
                p.i65(),
            );
        });
    });
    ui.add_space(S1);
    ui.label(
        RichText::new(title_text)
            .font(serif(T_TITLE + 4.0))
            .color(p.i100()),
    );
    ui.add_space(S2);
}

fn version_row(ui: &mut Ui, p: &Pal, from: &Version, to: &Version, right: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = S3;
        ui.label(
            RichText::new(from.to_string())
                .font(mono(T_BODY))
                .color(p.i65()),
        );
        ui.label(RichText::new("→").font(mono(T_BODY)).color(p.i65()));
        ui.label(
            RichText::new(to.to_string())
                .font(mono_med(T_BODY))
                .color(p.i100()),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(right).font(mono(T_DATA)).color(p.i65()));
        });
    });
}

fn changelog_box(ui: &mut Ui, p: &Pal, rel: &Release, h: f32) {
    ui.label(caps(p, &format!("What's new in {}", rel.version)));
    ui.add_space(S1);
    let notes = rel.notes_text();
    Frame::new()
        .fill(p.sunken)
        .stroke(Stroke::new(1.0, p.i28()))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.style_mut().spacing.scroll = egui::style::ScrollStyle::solid();
            ui.style_mut().spacing.scroll.bar_width = 8.0;
            ui.style_mut().spacing.scroll.foreground_color = false;
            ui.style_mut().visuals.widgets.inactive.bg_fill = p.i65(); // scrollbar handle, as the macro list (3:1+ against the track)
            egui::ScrollArea::vertical()
                .max_height(h)
                .auto_shrink([false, false])
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    if notes.is_empty() {
                        ui.add(egui::Label::new(
                            RichText::new("No release notes were published for this version.")
                                .font(sans(T_SMALL))
                                .color(p.i65()),
                        ));
                    }
                    for block in blocks(&notes) {
                        match block {
                            Block::Gap => {
                                ui.add_space(2.0);
                            }
                            Block::Bullet(t) => {
                                ui.horizontal_top(|ui| {
                                    ui.spacing_mut().item_spacing.x = S2;
                                    ui.label(RichText::new("–").font(mono(T_DATA)).color(p.i65()));
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(t).font(sans(T_SMALL)).color(p.i100()),
                                        )
                                        .wrap(),
                                    );
                                });
                            }
                            Block::Para(t) => {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(t).font(sans(T_SMALL)).color(p.i100()),
                                    )
                                    .wrap(),
                                );
                            }
                        }
                    }
                });
        });
    ui.add_space(S1);
    ui.label(
        RichText::new("Plain text from the CHANGELOG.md section for this release.")
            .font(mono(T_MICRO))
            .color(p.i65()),
    );
}

/// Fixed-height stand-in for the changelog while the dialog is working, so the dialog does not change size under the cursor.
fn steps_box(ui: &mut Ui, p: &Pal, h: f32, add: impl FnOnce(&mut Ui)) {
    if h == 0.0 {
        add(ui); // compact: no recap box, so the callout is not boxed twice
        return;
    }
    Frame::new()
        .fill(p.raised)
        .stroke(Stroke::new(1.0, p.i28()))
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(h);
            add(ui);
        });
}

fn footer(ui: &mut Ui, p: &Pal, buttons: impl FnOnce(&mut Ui), note: &str) {
    ui.add_space(S4);
    hair(ui, p);
    ui.add_space(S3);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = S2;
        buttons(ui);
    });
    if !note.is_empty() {
        ui.add_space(S2);
        small(ui, p, note);
    }
}

fn short_hash(h: &str) -> String {
    if h.len() > 20 {
        format!("{}…{}", &h[..12], &h[h.len() - 4..])
    } else {
        h.to_string()
    }
}

#[allow(clippy::too_many_arguments)]
fn dialog_body(
    ui: &mut Ui,
    p: &Pal,
    u: &Updater,
    rel: &Release,
    dialog: &Dialog,
    unsaved: Option<&str>,
    log_h: f32,
    compact: bool,
    o: &mut Out,
) {
    let inst = &u.install;
    let (cur, new) = (&u.current, &rel.version);
    let file_new = rel.appimage_name();
    let file_old = match inst {
        InstallType::AppImage { path } => path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        _ => String::new(),
    };
    let size = rel.appimage().map(|a| a.size).unwrap_or(0);
    let recap_h = if compact { 0.0 } else { log_h + 22.0 };
    let cstep = |ui: &mut Ui, st: S, label: &str, detail: &str| {
        if !compact {
            step(ui, p, st, label, detail);
        }
    };
    match dialog {
        Dialog::Offer => {
            head(
                ui,
                p,
                "Update available",
                inst,
                &format!("razerctl {new} is available"),
            );
            let mut right = release_date(&rel.published)
                .map(|d| format!("Released {d}"))
                .unwrap_or_default();
            if matches!(inst, InstallType::AppImage { .. }) && size > 0 {
                right = format!("{right} · {}", megabytes(size));
            }
            version_row(ui, p, cur, new, &right);
            ui.add_space(S3);
            changelog_box(ui, p, rel, log_h);
            ui.add_space(S3);
            match inst {
                InstallType::AppImage { .. } => small(ui, p, &format!(
                    "Update downloads {file_new}, checks it against the release's SHA256SUMS, then replaces this file. The old one is kept as {file_old}.old. Nothing changes if the check fails."
                )),
                InstallType::Tarball => small(ui, p, "razerctl was installed from a tarball or with install.sh, so you update it by hand. The release page has the new files and the install steps."),
                InstallType::Package => {
                    small(ui, p, &format!("razerctl was installed by your package manager, so it is updated there, not here. If the package still shows {cur}, the release has not reached it yet: try again in a day."));
                    ui.add_space(S2);
                    code_block(ui, p, &[("", "yay -Syu"), ("", "paru -Syu")]);
                }
            }
            footer(
                ui,
                p,
                |ui| match inst {
                    InstallType::AppImage { .. } => {
                        o.update = btn(ui, p, "Update", Kind::Primary, true).clicked();
                        o.not_now = btn(ui, p, "Not now", Kind::Secondary, true).clicked();
                    }
                    InstallType::Tarball => {
                        o.open_page =
                            btn(ui, p, "Open release page", Kind::Primary, true).clicked();
                        o.not_now = btn(ui, p, "Not now", Kind::Secondary, true).clicked();
                    }
                    InstallType::Package => {
                        o.close = btn(ui, p, "Close", Kind::Primary, true).clicked()
                    }
                },
                "razerctl asks once a day. Turn this off on the App tab.",
            );
        }
        Dialog::Working(stage) => {
            let (title, done, total) = match *stage {
                Stage::Downloading { done, total } => (
                    format!("Downloading razerctl {new}"),
                    done,
                    if total > 0 { total } else { size },
                ),
                Stage::Verifying | Stage::Replacing => {
                    (format!("Checking razerctl {new}"), size, size)
                }
            };
            head(ui, p, "Updating", inst, &title);
            version_row(ui, p, cur, new, &megabytes(size));
            ui.add_space(S3);
            ui.label(caps(p, "Progress"));
            ui.add_space(S1);
            let frac = if total > 0 {
                (done as f32 / total as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let downloading = matches!(stage, Stage::Downloading { .. });
            steps_box(ui, p, log_h + 22.0, |ui| {
                let detail = format!(
                    "{} of {}",
                    megabytes(done).trim_end_matches(" MB"),
                    megabytes(total)
                );
                step(
                    ui,
                    p,
                    if downloading { S::Now } else { S::Done },
                    "Download",
                    &detail,
                );
                ui.add_space(S1);
                let w = ui.available_width();
                meter(ui, p, w, 14.0, frac, 48);
                ui.add_space(S3);
                step(
                    ui,
                    p,
                    if downloading { S::Wait } else { S::Now },
                    "Check against SHA256SUMS",
                    if downloading { "" } else { "sha256" },
                );
                step(ui, p, S::Wait, "Replace the AppImage", "keeps .old");
                step(ui, p, S::Wait, "Restart razerctl", "");
                ui.add_space(S3);
                small(
                    ui,
                    p,
                    if downloading {
                        "The window stays usable while this runs. Cancel deletes the partial download."
                    } else {
                        "The window stays usable while razerctl checks the file. Nothing has been replaced yet."
                    },
                );
            });
            footer(
                ui,
                p,
                |ui| o.cancel = btn(ui, p, "Cancel", Kind::Secondary, true).clicked(),
                "",
            );
        }
        Dialog::Done { old, .. } => {
            head(
                ui,
                p,
                "Updated",
                inst,
                &format!("razerctl {new} is installed"),
            );
            version_row(ui, p, cur, new, "");
            ui.add_space(S3);
            let kept = old
                .as_ref()
                .map(|_| format!("{file_old}.old kept"))
                .unwrap_or_default();
            steps_box(ui, p, recap_h, |ui| {
                cstep(ui, S::Done, "Download", &megabytes(size));
                cstep(ui, S::Done, "Check against SHA256SUMS", "matches");
                cstep(ui, S::Done, "Replace the AppImage", &kept);
                cstep(ui, S::Now, "Restart razerctl", "waiting for you");
                ui.add_space(S3);
                match unsaved {
                    Some(what) => callout(
                        ui,
                        p,
                        p.warn,
                        "▲",
                        "You have unsaved changes",
                        &format!("{what} not saved yet, and restarting throws them away. Press Later, save them, then restart."),
                    ),
                    None => body(ui, p, &format!("This window is still running {cur}. Restart razerctl to use {new}; your settings and the mouse's saved buttons carry over.")),
                }
            });
            if unsaved.is_some() {
                footer(
                    ui,
                    p,
                    |ui| {
                        o.later = btn(ui, p, "Later", Kind::Primary, true).clicked();
                        o.restart =
                            btn(ui, p, "Restart and discard", Kind::Secondary, true).clicked();
                    },
                    "Later keeps this window open.",
                );
            } else {
                footer(
                    ui,
                    p,
                    |ui| {
                        o.restart = btn(ui, p, "Restart now", Kind::Primary, true).clicked();
                        o.later = btn(ui, p, "Later", Kind::Secondary, true).clicked();
                    },
                    "Remap and alerts keep running.",
                );
            }
        }
        Dialog::Saved { path, why } => {
            head(
                ui,
                p,
                "Update downloaded",
                inst,
                "razerctl couldn't replace itself",
            );
            version_row(ui, p, cur, new, "downloaded, not installed");
            ui.add_space(S3);
            let shown = path.display().to_string();
            steps_box(ui, p, recap_h, |ui| {
                cstep(ui, S::Done, "Download", &megabytes(size));
                cstep(ui, S::Done, "Check against SHA256SUMS", "matches");
                cstep(ui, S::Fail, "Replace the AppImage", "can't write");
                cstep(ui, S::Skip, "Restart razerctl", "not needed");
                ui.add_space(S3);
                body(ui, p, &format!("razerctl couldn't replace the running file ({why}), so it is untouched. The new version passed the check and is saved here, ready to run:"));
                ui.add_space(S2);
                code_block(ui, p, &[("", &shown)]);
            });
            footer(
                ui,
                p,
                |ui| {
                    if btn(ui, p, "Copy path", Kind::Primary, true).clicked() {
                        o.copy = Some(shown.clone());
                    }
                    o.close = btn(ui, p, "Close", Kind::Secondary, true).clicked();
                },
                "",
            );
        }
        Dialog::Failed(e) => {
            let mismatch = matches!(e, UpdateError::Mismatch { .. });
            head(
                ui,
                p,
                "Update failed",
                inst,
                if mismatch {
                    "The download didn't match"
                } else {
                    "The update didn't finish"
                },
            );
            version_row(ui, p, cur, new, "not installed");
            ui.add_space(S3);
            steps_box(ui, p, recap_h, |ui| {
                match e {
                    UpdateError::Mismatch { .. } => {
                        cstep(ui, S::Done, "Download", &megabytes(size));
                        cstep(ui, S::Fail, "Check against SHA256SUMS", "doesn't match");
                    }
                    UpdateError::Net(_) | UpdateError::MissingAsset(_) => {
                        cstep(ui, S::Fail, "Download", "failed");
                        cstep(ui, S::Skip, "Check against SHA256SUMS", "not done");
                    }
                    UpdateError::Io(_) | UpdateError::Cancelled => {
                        cstep(ui, S::Done, "Download", &megabytes(size));
                        cstep(ui, S::Fail, "Check against SHA256SUMS", "failed");
                    }
                }
                cstep(ui, S::Skip, "Replace the AppImage", "not done");
                cstep(ui, S::Skip, "Restart razerctl", "not needed");
                ui.add_space(S3);
                match e {
                    UpdateError::Mismatch { .. } => callout(
                        ui,
                        p,
                        p.bad,
                        "×",
                        "Nothing was changed",
                        &format!("The file razerctl downloaded does not match the checksum published with the release, so it was deleted. You are still on {cur} and {file_old} is exactly as it was."),
                    ),
                    other => callout(
                        ui,
                        p,
                        p.bad,
                        "×",
                        "Nothing was changed",
                        &format!("{other}. You are still on {cur} and {file_old} is exactly as it was."),
                    ),
                }
                if let UpdateError::Mismatch { expected, got } = e {
                    ui.add_space(S2);
                    code_block(
                        ui,
                        p,
                        &[
                            ("Expected", &short_hash(expected)),
                            ("Got", &short_hash(got)),
                        ],
                    );
                }
            });
            footer(
                ui,
                p,
                |ui| {
                    o.update = btn(ui, p, "Try again", Kind::Primary, true).clicked();
                    o.open_page = btn(ui, p, "Open release page", Kind::Secondary, true).clicked();
                    o.close = btn(ui, p, "Close", Kind::Secondary, true).clicked();
                },
                "",
            );
        }
    }
}

/// The new file to start after *Restart now*, and what to do with the edits: handed from the window to `main`, which
/// starts it once the window is gone (the process is replaced, so nothing may still be running).
static RESTART: Mutex<Option<PathBuf>> = Mutex::new(None);

pub fn request_restart(path: PathBuf) {
    *RESTART.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
}

/// Replaces this process with `path`, the file the update just wrote, with the same arguments. Only returns on failure.
/// The AppImage runtime sets `APPDIR` and puts it first in a few search paths; the old mount is going away, so those
/// entries are dropped and the new file's own runtime sets its own.
pub fn restart(path: &std::path::Path) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(path);
    cmd.args(std::env::args_os().skip(1));
    let appdir = std::env::var("APPDIR").unwrap_or_default();
    for var in ["APPDIR", "APPIMAGE", "ARGV0", "OWD"] {
        cmd.env_remove(var);
    }
    for var in [
        "PATH",
        "LD_LIBRARY_PATH",
        "XDG_DATA_DIRS",
        "GSETTINGS_SCHEMA_DIR",
    ] {
        if let Ok(v) = std::env::var(var) {
            cmd.env(var, without_dir(&v, &appdir));
        }
    }
    cmd.env_remove(super::carry::ENV);
    cmd.exec()
}

/// A `:`-separated search path without the entries under `dir` (all of it when `dir` is empty).
fn without_dir(list: &str, dir: &str) -> String {
    if dir.is_empty() {
        return list.to_string();
    }
    let under = |e: &str| std::path::Path::new(e).starts_with(dir);
    list.split(':')
        .filter(|e| !under(e))
        .collect::<Vec<_>>()
        .join(":")
}

pub fn take_restart() -> Option<PathBuf> {
    RESTART.lock().unwrap_or_else(|e| e.into_inner()).take()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_sizes_read_the_way_the_dialog_prints_them() {
        assert_eq!(
            release_date("2026-10-17T15:23:45Z").as_deref(),
            Some("17 Oct 2026")
        );
        assert_eq!(release_date("2026-01-02").as_deref(), Some("2 Jan 2026"));
        assert_eq!(release_date("bad"), None);
        assert_eq!(release_date("2026-13-01"), None);
        assert_eq!(megabytes(14_200_000), "14.2 MB");
        assert_eq!(plain("## **Fixed** `remap`"), "Fixed remap");
        assert_eq!(
            short_hash(&format!("{}{}", "3f9a6c0e5b7d", "0".repeat(48) + "b21d")),
            "3f9a6c0e5b7d…b21d"
        );
    }

    fn release() -> Release {
        Release::parse(
            r#"{"tag_name":"v0.3.0","draft":false,"html_url":"https://x/r","body":"n",
                "assets":[{"name":"razerctl-0.3.0-x86_64.AppImage","size":10,"browser_download_url":"http://127.0.0.1:9/a"},
                {"name":"SHA256SUMS","size":10,"browser_download_url":"http://127.0.0.1:9/s"}]}"#,
        )
        .unwrap()
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("razerctl-updater-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// An updater whose "GitHub" is a port nothing listens on, with its files in `dir`.
    fn updater(dir: &std::path::Path, install: InstallType) -> Updater {
        let mut u = Updater::with(
            Paths::in_dir(dir),
            Source::loopback("http://127.0.0.1:9"),
            install,
            Version::parse("0.2.0").unwrap(),
        );
        u.stage(Some(release()), State::default(), Some(Dialog::Offer));
        u
    }

    fn appimage(dir: &std::path::Path) -> InstallType {
        InstallType::AppImage {
            path: dir.join("razerctl-0.2.0-x86_64.AppImage"),
        }
    }

    #[test]
    fn not_now_and_esc_dismiss_for_a_day_and_close_the_dialog() {
        let ctx = egui::Context::default();
        for how in ["not_now", "esc"] {
            let dir = tmp(how);
            let mut u = updater(&dir, appimage(&dir));
            let out = Out {
                not_now: how == "not_now",
                esc: how == "esc",
                ..Out::default()
            };
            assert_eq!(finish(&ctx, &mut u, &release(), &Dialog::Offer, out), None);
            assert_eq!(u.dialog, None, "{how}");
            assert!(u.state.dismissed.is_some(), "{how}: remembered");
            assert!(
                !u.state.banner_wanted(update::now_secs()),
                "{how}: the banner stays away today"
            );
            assert!(u.banner_release(true).is_none());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn only_an_appimage_install_can_start_an_update() {
        let ctx = egui::Context::default();
        for install in [InstallType::Tarball, InstallType::Package] {
            let dir = tmp("noapply");
            let mut u = updater(&dir, install.clone());
            finish(
                &ctx,
                &mut u,
                &release(),
                &Dialog::Offer,
                Out {
                    update: true,
                    ..Out::default()
                },
            );
            assert!(
                u.dialog != Some(Dialog::Working(Stage::Downloading { done: 0, total: 0 })),
                "{install:?}"
            );
            assert!(u.rx.is_none(), "{install:?}: no thread was started");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn nothing_downloads_until_update_is_pressed() {
        let ctx = egui::Context::default();
        let dir = tmp("press");
        let mut u = updater(&dir, appimage(&dir));
        u.auto = true;
        u.tick(&ctx, true); // the automatic check runs, but it is a check, never a download
        assert!(u.rx.is_some() && u.dialog == Some(Dialog::Offer));
        finish(
            &ctx,
            &mut u,
            &release(),
            &Dialog::Offer,
            Out {
                update: true,
                ..Out::default()
            },
        );
        assert!(
            matches!(u.dialog, Some(Dialog::Working(_))),
            "now it is working"
        );
        // the fixture port is closed: the download fails, and the failure is shown, with nothing written
        let t = Instant::now();
        while !matches!(u.dialog, Some(Dialog::Failed(_))) && t.elapsed() < Duration::from_secs(20)
        {
            u.tick(&ctx, true);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            matches!(u.dialog, Some(Dialog::Failed(UpdateError::Net(_)))),
            "{:?}",
            u.dialog
        );
        assert_eq!(
            std::fs::read_dir(&dir)
                .unwrap()
                .filter(|e| e
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".part"))
                .count(),
            0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn esc_while_working_cancels_the_download_and_done_offers_restart_or_later() {
        let ctx = egui::Context::default();
        let dir = tmp("esc");
        let mut u = updater(&dir, appimage(&dir));
        let working = Dialog::Working(Stage::Verifying);
        finish(
            &ctx,
            &mut u,
            &release(),
            &working,
            Out {
                esc: true,
                ..Out::default()
            },
        );
        assert!(u.cancel.load(Ordering::Relaxed));
        let done = Dialog::Done {
            path: dir.join("new"),
            old: None,
        };
        u.dialog = Some(done.clone());
        assert_eq!(
            finish(
                &ctx,
                &mut u,
                &release(),
                &done,
                Out {
                    restart: true,
                    ..Out::default()
                }
            ),
            Some(Action::Restart {
                path: dir.join("new")
            })
        );
        assert_eq!(
            finish(
                &ctx,
                &mut u,
                &release(),
                &done,
                Out {
                    later: true,
                    ..Out::default()
                }
            ),
            None
        );
        assert_eq!(
            u.dialog, None,
            "Later keeps the window and closes only the dialog"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The automatic check: not when the switch is off, not when `auto` is off (tests, hooks), not twice in 24 hours; offline
    /// is silent (no status line to say, the outcome recorded for the App tab).
    #[test]
    fn the_background_check_respects_the_switch_the_throttle_and_stays_silent_offline() {
        let ctx = egui::Context::default();
        let dir = tmp("auto");
        let mut u = updater(&dir, appimage(&dir));
        u.stage(None, State::default(), None);
        u.tick(&ctx, true);
        assert!(u.rx.is_none(), "auto is off: no thread");
        u.auto = true;
        u.tick(&ctx, false);
        assert!(u.rx.is_none(), "the switch is off: no thread");
        u.tick(&ctx, true);
        assert!(u.rx.is_some() && u.checking, "due and on: one check");
        let t = Instant::now();
        while u.checking && t.elapsed() < Duration::from_secs(20) {
            u.tick(&ctx, true);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!u.checking);
        assert_eq!(u.state.outcome, Some(Outcome::Offline));
        assert_eq!(u.say, None, "offline says nothing");
        assert_eq!(u.dialog, None);
        assert!(u.banner_release(true).is_none());
        // used up for the day, even though it failed
        u.last_eval = None;
        u.tick(&ctx, true);
        assert!(
            !u.checking,
            "a second automatic check inside 24 hours is not made"
        );
        // Check now ignores the throttle, and tells an up-to-date answer only because the user asked
        u.start_check(&ctx, true);
        assert!(u.checking);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_restart_drops_the_old_mount_from_the_search_paths() {
        let mount = "/tmp/.mount_razerckAbc";
        let list = format!("{mount}/usr/bin:/usr/bin:{mount}/usr/lib:/home/u/bin");
        assert_eq!(without_dir(&list, mount), "/usr/bin:/home/u/bin");
        assert_eq!(without_dir(&list, ""), list, "no APPDIR: nothing to drop");
        assert_eq!(
            without_dir("/tmp/.mount_razerckAbcd/x:/a", mount),
            "/tmp/.mount_razerckAbcd/x:/a",
            "whole components only"
        );
    }

    #[test]
    fn hard_wrapped_notes_join_into_one_block_per_bullet_and_paragraph() {
        let notes = "**What it is.** A tool,\nwrapped.\n\n\n- **Buttons:** remaps\n  saved into the mouse.\n- Device: battery\n\nClosing line.";
        assert_eq!(
            blocks(notes),
            [
                Block::Para("What it is. A tool, wrapped.".into()),
                Block::Gap,
                Block::Bullet("Buttons: remaps saved into the mouse.".into()),
                Block::Bullet("Device: battery".into()),
                Block::Gap,
                Block::Para("Closing line.".into()),
            ]
        );
        assert_eq!(blocks(""), []);
    }

    #[test]
    fn when_says_today_yesterday_or_the_date() {
        // noon UTC on a mid-month day, far from any date line, so the local day is the same everywhere that matters
        let now = 1_760_702_400; // 2026-10-17 ~12:00 UTC
        assert!(when(now - 60, now).starts_with("Today, "));
        assert!(when(now - 24 * 3600, now).starts_with("Yesterday, "));
        let old = when(now - 5 * 24 * 3600, now);
        assert!(old.starts_with("12 Oct, "), "{old}");
    }
}
