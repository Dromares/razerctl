//! The macro UI inside Software remap (DAN-1426 design, DAN-1437 build): the `RULES | MACROS` sub-nav, the
//! `ADD A RULE` strip, and the structured macro editor that reads and writes `macros.conf`.
//!
//! The first half is state and logic with no egui in it (draft macros, validation with row numbers, the rule
//! scan over the `remap.conf` text, the rename / remove rewrites), so the tests drive it without a frame. The
//! second half draws it, in the same tokens and widgets as the rest of the app.
//!
//! Storage is unchanged: macros live in `macros.conf` (`razerctl::macros`), bindings stay lines in `remap.conf`
//! (`NAME = macro:<name>[,once|xN|held|toggle]`, `razerctl::remap`). Nothing here talks to the mouse.

use std::collections::BTreeSet;
use std::path::Path;

use eframe::egui::{
    self, epaint::RectShape, pos2, vec2, Align, Color32, ComboBox, DragValue, Frame, Layout, Margin, Rect, RichText,
    Sense, Shape, Stroke, TextEdit, Ui,
};
use evdev::KeyCode;
use razerctl::macros::{self, Macro, MacroEvent, Macros};
use razerctl::remap::{self, Action, Playback};

use super::theme::*;
use super::widgets::*;
use super::{write_atomic, App};

// ---------------------------------------------------------------------------------------------- model

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sub {
    Rules,
    Macros,
}

/// Whether macros can play right now, from the two things the GUI can see: `systemctl is-active` and whether the
/// mouse answered, plus what `service.rs` read back after the last start. (The service does not report back, so there is no "playing" state; see the design's flag 6.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Remap {
    Running,
    Stopped,
    NoDevice,
    /// The unit was started (or is configured to) but is crash-looping or exited; `svc_failure` says how.
    Failed,
}

/// One event row. 1:1 with `MacroEvent`, except a key row keeps the text as typed until it is checked, so a
/// half-typed key name is an error on that row rather than something the editor refuses to hold.
#[derive(Clone, PartialEq, Debug)]
pub enum Ev {
    Key(String, bool),
    Button(KeyCode, bool),
    Wheel(i32),
    HWheel(i32),
    Move(i32, i32),
    Delay(u32),
}

fn is_button(k: KeyCode) -> bool {
    format!("{k:?}").starts_with("BTN_")
}

/// The mouse buttons a macro can press (what `BTN_*` rows offer).
pub const BUTTON_CODES: [KeyCode; 5] =
    [KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE, KeyCode::BTN_SIDE, KeyCode::BTN_EXTRA];

impl Ev {
    pub fn from_event(e: &MacroEvent) -> Ev {
        match e {
            MacroEvent::Key(k, d) => Ev::Key(format!("{k:?}"), *d),
            MacroEvent::Button(k, d) => Ev::Button(*k, *d),
            MacroEvent::Wheel(n) => Ev::Wheel(*n),
            MacroEvent::HWheel(n) => Ev::HWheel(*n),
            MacroEvent::Move(x, y) => Ev::Move(*x, *y),
            MacroEvent::Delay(n) => Ev::Delay(*n),
        }
    }

    pub fn to_event(&self) -> Result<MacroEvent, String> {
        Ok(match self {
            Ev::Key(t, d) => {
                if t.trim().is_empty() {
                    return Err("Type a key name, such as KEY_A or ctrl.".into());
                }
                let k = remap::parse_key(t)?;
                if is_button(k) {
                    return Err(format!("'{}' is a mouse button. Use a BTN row or + Click.", t.trim()));
                }
                MacroEvent::Key(k, *d)
            }
            Ev::Button(k, d) => MacroEvent::Button(*k, *d),
            Ev::Wheel(n) => MacroEvent::Wheel(*n),
            Ev::HWheel(n) => MacroEvent::HWheel(*n),
            Ev::Move(x, y) => MacroEvent::Move(*x, *y),
            Ev::Delay(n) => MacroEvent::Delay(*n),
        })
    }

    /// (column text for EVENT, true when this is a Delay row)
    fn kind(&self) -> &'static str {
        match self {
            Ev::Key(_, true) => "KEY ↓",
            Ev::Key(_, false) => "KEY ↑",
            Ev::Button(_, true) => "BTN ↓",
            Ev::Button(_, false) => "BTN ↑",
            Ev::Wheel(_) => "WHEEL",
            Ev::HWheel(_) => "HWHEEL",
            Ev::Move(..) => "MOVE",
            Ev::Delay(_) => "DELAY",
        }
    }
}

/// A macro as the editor holds it. `orig` is the name it has in the file today (None = new), which is what the
/// rules in `remap.conf` still say until the next save rewrites them.
#[derive(Clone, PartialEq, Debug)]
pub struct Draft {
    pub name: String,
    pub guid: Option<String>,
    pub events: Vec<Ev>,
    pub orig: Option<String>,
}

impl Draft {
    pub fn from_macro(m: &Macro) -> Draft {
        Draft {
            name: m.name.clone(),
            guid: m.guid.clone(),
            events: m.events.iter().map(Ev::from_event).collect(),
            orig: Some(m.name.clone()),
        }
    }

    pub fn new(name: &str, events: Vec<Ev>) -> Draft {
        Draft { name: name.into(), guid: None, events, orig: None }
    }

    /// The macro the file will hold, or the first thing wrong with a row.
    pub fn to_macro(&self) -> Result<Macro, String> {
        let events = self.events.iter().map(Ev::to_event).collect::<Result<Vec<_>, _>>()?;
        Ok(Macro { name: self.name.clone(), guid: self.guid.clone(), events })
    }

    /// Run time of one playback, padding included (the engine's own `play_ms`, fed stand-ins for the events whose
    /// value does not matter for timing, so a half-typed key still gets a number).
    pub fn run_ms(&self) -> u64 {
        let stand_in: Vec<MacroEvent> = self
            .events
            .iter()
            .map(|e| if let Ev::Delay(n) = e { MacroEvent::Delay(*n) } else { MacroEvent::Wheel(1) })
            .collect();
        macros::play_ms(&stand_in)
    }
}

/// One thing wrong with a macro. `ev` is the row to mark, None for the macro as a whole.
#[derive(Clone, PartialEq, Debug)]
pub struct Issue {
    pub ev: Option<usize>,
    pub msg: String,
}

pub fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= macros::MAX_NAME && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Everything that blocks saving `d`, in plain language, with row numbers. `all` is the whole list, for the
/// duplicate-name check. A macro with no issues is accepted by `macros::save_macros`: that is checked last.
pub fn issues(d: &Draft, all: &[Draft]) -> Vec<Issue> {
    let mut out = Vec::new();
    let mut add = |ev: Option<usize>, msg: String| out.push(Issue { ev, msg });
    if !valid_name(&d.name) {
        add(None, format!("The name must be 1 to {} letters, digits, - or _.", macros::MAX_NAME));
    } else if all.iter().filter(|o| o.name == d.name).count() > 1 {
        add(None, format!("Another macro is already called '{}'.", d.name));
    }
    if d.events.is_empty() {
        add(None, "No events. A macro needs at least one event.".into());
        return out;
    }
    if d.events.len() > macros::MAX_EVENTS {
        add(Some(macros::MAX_EVENTS), format!("{} events is too many (the most is {}).", d.events.len(), macros::MAX_EVENTS));
    }
    let mut held: Vec<(KeyCode, usize)> = Vec::new();
    for (i, e) in d.events.iter().enumerate() {
        let n = i + 1;
        let ev = match e.to_event() {
            Ok(ev) => ev,
            Err(m) => {
                add(Some(i), format!("#{n:02}: {m}"));
                continue;
            }
        };
        match ev {
            MacroEvent::Key(k, down) | MacroEvent::Button(k, down) => {
                if down {
                    if held.iter().any(|(h, _)| *h == k) {
                        add(Some(i), format!("{k:?} is pressed again at #{n:02} while it is still down."));
                    } else {
                        held.push((k, i));
                    }
                } else if let Some(pos) = held.iter().position(|(h, _)| *h == k) {
                    held.remove(pos);
                } else {
                    add(Some(i), format!("{k:?} is released at #{n:02} but was never pressed."));
                }
            }
            MacroEvent::Wheel(v) | MacroEvent::HWheel(v) => {
                if v == 0 || v.unsigned_abs() > macros::MAX_WHEEL {
                    add(Some(i), format!("#{n:02}: scroll {v} is out of range (1 to {} ticks, up or down).", macros::MAX_WHEEL));
                }
            }
            MacroEvent::Move(dx, dy) => {
                if (dx == 0 && dy == 0) || dx.unsigned_abs() > macros::MAX_MOVE || dy.unsigned_abs() > macros::MAX_MOVE {
                    add(Some(i), format!("#{n:02}: move {dx},{dy} is out of range (not both 0, each at most {}).", macros::MAX_MOVE));
                }
            }
            MacroEvent::Delay(ms) => {
                if ms > macros::MAX_DELAY_MS {
                    add(Some(i), format!("#{n:02}: {ms} ms is too long (the most is {} ms).", macros::MAX_DELAY_MS));
                }
            }
        }
    }
    for (k, i) in held {
        add(Some(i), format!("{k:?} is pressed at #{:02} and never released, so it would stay held after the macro ends.", i + 1));
    }
    if out.is_empty() {
        // anything the engine's own check still refuses (guid, unnamed key codes) is shown instead of lost
        if let Err(e) = d.to_macro().and_then(|m| macros::validate(&m)) {
            out.push(Issue { ev: None, msg: e });
        }
    }
    out
}

// ------------------------------------------------------------------------------- rules in remap.conf

/// One `BUTTON = macro:NAME[,mode]` line of the rules text.
#[derive(Clone, PartialEq, Debug)]
pub struct RuleRef {
    /// 1-based line number in the text.
    pub line: usize,
    /// The left-hand side as written (`forward`, `BTN_EXTRA`).
    pub source: String,
    pub name: String,
    pub mode: Playback,
}

pub fn rule_refs(conf: &str) -> Vec<RuleRef> {
    conf.lines()
        .enumerate()
        .filter_map(|(i, l)| {
            let (from, to) = l.split('#').next()?.split_once('=')?;
            match remap::parse_action(to) {
                Ok(Action::Macro { name, mode }) => Some(RuleRef { line: i + 1, source: from.trim().into(), name, mode }),
                _ => None,
            }
        })
        .collect()
}

/// BACK / FORWARD for the mouse's side buttons (what the rest of the app calls them), else the key as written.
pub fn button_label(source: &str) -> String {
    match remap::parse_key(source) {
        Ok(k) if k == KeyCode::BTN_LEFT => "LEFT".into(),
        Ok(k) if k == KeyCode::BTN_RIGHT => "RIGHT".into(),
        Ok(k) if k == KeyCode::BTN_MIDDLE => "MIDDLE".into(),
        Ok(k) if k == KeyCode::BTN_SIDE => "BACK".into(),
        Ok(k) if k == KeyCode::BTN_EXTRA => "FORWARD".into(),
        _ => source.to_uppercase(),
    }
}

pub fn mode_label(m: Playback) -> String {
    match m {
        Playback::Once => "ONCE".into(),
        Playback::Times(n) => format!("{n} TIMES"),
        Playback::Held => "WHILE HELD".into(),
        Playback::Toggle => "TOGGLE".into(),
    }
}

/// `FORWARD · WHILE HELD` for each rule that plays `d` (by the name the file has today, or the one it will have).
pub fn used_by(d: &Draft, refs: &[RuleRef]) -> Vec<String> {
    refs.iter()
        .filter(|r| r.name == d.name || Some(&r.name) == d.orig.as_ref())
        .map(|r| format!("{} · {}", button_label(&r.source), mode_label(r.mode)))
        .collect()
}

/// What a save does to the rules besides writing macros.conf.
#[derive(Clone, PartialEq, Debug)]
pub enum RuleFix {
    Rename { from: String, to: String },
    Remove(String),
}

/// Byte range of the macro name in a rule line (`back = macro:NAME,held`), None for any other line.
fn macro_span(line: &str) -> Option<(usize, usize)> {
    let code = &line[..line.find('#').unwrap_or(line.len())];
    let eq = code.find('=')?;
    let rhs = &code[eq + 1..];
    let off = rhs.to_ascii_lowercase().find("macro:")?;
    if !rhs[..off].trim().is_empty() {
        return None;
    }
    let start = eq + 1 + off + "macro:".len();
    let rest = &code[start..];
    let seg = &rest[..rest.find(',').unwrap_or(rest.len())];
    let lead = seg.len() - seg.trim_start().len();
    Some((start + lead, start + lead + seg.trim().len()))
}

/// `text` with the fixes applied, every other byte kept (comments, spacing, the trailing newline). Each line is
/// matched once against the original names, so two macros that swap names do not cascade.
pub fn fix_text(text: &str, fixes: &[RuleFix]) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let span = macro_span(body).filter(|_| !rule_refs(body).is_empty());
        let Some((s, e)) = span else {
            out.push_str(line);
            continue;
        };
        let name = &body[s..e];
        let fix = fixes.iter().find(|f| match f {
            RuleFix::Rename { from, .. } => from == name,
            RuleFix::Remove(n) => n == name,
        });
        match fix {
            Some(RuleFix::Remove(_)) => {}
            Some(RuleFix::Rename { to, .. }) => {
                out.push_str(&line[..s]);
                out.push_str(to);
                out.push_str(&line[e..]);
            }
            None => out.push_str(line),
        }
    }
    out
}

/// Remove line `n` (1-based) of `text`.
pub fn remove_line(text: &str, n: usize) -> String {
    text.split_inclusive('\n').enumerate().filter(|(i, _)| i + 1 != n).map(|(_, l)| l).collect()
}

/// Point line `n` (1-based) at another macro, keeping the mode and everything else on the line.
pub fn retarget_line(text: &str, n: usize, to: &str) -> String {
    text.split_inclusive('\n')
        .enumerate()
        .map(|(i, l)| match macro_span(l).filter(|_| i + 1 == n) {
            Some((s, e)) => format!("{}{to}{}", &l[..s], &l[e..]),
            None => l.to_string(),
        })
        .collect()
}

// -------------------------------------------------------------------------------- the ADD A RULE strip

pub const BUTTONS: [(&str, &str); 5] =
    [("Left", "left"), ("Right", "right"), ("Middle", "middle"), ("Back", "back"), ("Forward", "forward")];
pub const DOES: [&str; 7] =
    ["Keyboard key…", "Disable", "Scroll up", "Scroll down", "Scroll left", "Scroll right", "Play a macro…"];
pub const DOES_MACRO: usize = 6;
pub const MODES: [&str; 5] = ["Once", "Times", "While held", "Toggle", "Sequence"];
/// Sequence has no engine behind it yet: drawn, not selectable.
pub const MODE_SEQUENCE: usize = 4;

#[derive(Clone, PartialEq, Debug)]
pub struct Bind {
    pub button: usize,
    pub does: usize,
    pub macro_name: String,
    pub key: String,
    pub mode: usize,
    pub times: u32,
}

impl Default for Bind {
    fn default() -> Self {
        Bind { button: 4, does: DOES_MACRO, macro_name: String::new(), key: String::new(), mode: 0, times: 5 }
    }
}

impl Bind {
    pub fn mode_token(&self) -> String {
        match self.mode {
            0 => "once".into(),
            1 => format!("x{}", self.times),
            2 => "held".into(),
            3 => "toggle".into(),
            _ => "sequence".into(),
        }
    }

    pub fn mode_help(&self) -> &'static str {
        match self.mode {
            0 => "Plays once per press, even if you keep holding.",
            1 => "Plays the set number of times per press.",
            2 => "Repeats while the button is held; stops on release.",
            3 => "Press once to start repeating, press again to stop.",
            _ => "Not available yet.",
        }
    }

    /// The rule line this strip would add, or why it can't add one.
    pub fn line(&self, macros: &[Draft]) -> Result<String, String> {
        let button = BUTTONS[self.button].1;
        let action = match self.does {
            0 => {
                let k = self.key.trim();
                if k.is_empty() {
                    return Err("Type the key to press, such as ctrl+c or f13.".into());
                }
                remap::parse_action(k)?;
                k.to_string()
            }
            1 => "disable".into(),
            2 => "scroll_up".into(),
            3 => "scroll_down".into(),
            4 => "scroll_left".into(),
            5 => "scroll_right".into(),
            _ => {
                if macros.is_empty() {
                    return Err("Make a macro first.".into());
                }
                if !valid_name(&self.macro_name) {
                    return Err("Pick a macro with a valid name.".into());
                }
                if self.mode == MODE_SEQUENCE {
                    return Err("Sequence is not available yet.".into());
                }
                if button == "left" && (self.mode == 2 || self.mode == 3) {
                    return Err("Left can't repeat while held or toggled: you would have no click left to press Stop.".into());
                }
                format!("macro:{},{}", self.macro_name, self.mode_token())
            }
        };
        Ok(format!("{button} = {action}"))
    }
}

// ------------------------------------------------------------------------------------- editor state

#[derive(Clone, PartialEq, Debug)]
pub struct MacroUi {
    pub sub: Sub,
    pub drafts: Vec<Draft>,
    /// The list as last loaded or saved: Revert goes back to it, Save is enabled while `drafts` differs.
    pub(super) saved: Vec<Draft>,
    /// Macros deleted with "Delete and remove rule": their rules are removed from remap.conf by the next save.
    pub remove_rules: BTreeSet<String>,
    pub sel: usize,
    pub sel_ev: usize,
    pub confirm_delete: bool,
    /// macros.conf exists but does not parse: shown, and never overwritten from here.
    pub load_error: Option<String>,
    pub bind: Bind,
    /// The selection the narrow list last scrolled into view (it scrolls again only when this changes).
    pub list_seen: Option<usize>,
}

impl MacroUi {
    pub fn empty() -> Self {
        MacroUi {
            sub: Sub::Rules,
            drafts: Vec::new(),
            saved: Vec::new(),
            remove_rules: BTreeSet::new(),
            sel: 0,
            sel_ev: 0,
            confirm_delete: false,
            load_error: None,
            bind: Bind::default(),
            list_seen: None,
        }
    }

    pub fn from_macros(m: &Macros) -> Self {
        let drafts: Vec<Draft> = m.values().map(Draft::from_macro).collect();
        MacroUi { saved: drafts.clone(), drafts, ..Self::empty() }
    }

    pub fn load(path: &Path) -> Self {
        match macros::load_macros(path) {
            Ok(m) => Self::from_macros(&m),
            Err(e) => MacroUi { load_error: Some(e), ..Self::empty() },
        }
    }

    pub fn dirty(&self) -> bool {
        self.drafts != self.saved || !self.remove_rules.is_empty()
    }

    pub fn revert(&mut self) {
        self.drafts = self.saved.clone();
        self.remove_rules.clear();
        self.confirm_delete = false;
        self.sel = self.sel.min(self.drafts.len().saturating_sub(1));
        self.sel_ev = 0;
    }

    /// Per macro, what is wrong with it (parallel to `drafts`).
    pub fn all_issues(&self) -> Vec<Vec<Issue>> {
        self.drafts.iter().map(|d| issues(d, &self.drafts)).collect()
    }

    pub fn invalid_count(&self) -> usize {
        self.all_issues().iter().filter(|v| !v.is_empty()).count()
    }

    pub fn can_save(&self) -> bool {
        self.dirty() && self.load_error.is_none() && self.invalid_count() == 0
    }

    /// Rule rewrites the next save makes: a rename follows its rules, a deleted macro takes its rules with it.
    pub fn fixes(&self) -> Vec<RuleFix> {
        let renames = self.drafts.iter().filter_map(|d| match &d.orig {
            Some(o) if *o != d.name => Some(RuleFix::Rename { from: o.clone(), to: d.name.clone() }),
            _ => None,
        });
        let removes = self
            .remove_rules
            .iter()
            .filter(|n| !self.drafts.iter().any(|d| &d.name == *n))
            .map(|n| RuleFix::Remove(n.clone()));
        renames.chain(removes).collect()
    }

    /// Rules whose macro does not exist (and that no pending fix will repair): (line, macro name), first first.
    pub fn dangling(&self, conf: &str) -> Vec<(usize, String)> {
        if self.load_error.is_some() {
            return Vec::new(); // the list is unknown, not empty: every rule would look dangling
        }
        let fixed: BTreeSet<String> = self
            .fixes()
            .into_iter()
            .map(|f| match f {
                RuleFix::Rename { from, .. } => from,
                RuleFix::Remove(n) => n,
            })
            .collect();
        rule_refs(conf)
            .into_iter()
            .filter(|r| !self.drafts.iter().any(|d| d.name == r.name) && !fixed.contains(&r.name))
            .map(|r| (r.line, r.name))
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.drafts.iter().map(|d| d.name.clone()).collect()
    }

    /// `base` if free, else `base-2`, `base-3`, ...
    pub fn free_name(&self, base: &str) -> String {
        let taken = |n: &str| self.drafts.iter().any(|d| d.name == n);
        if !taken(base) {
            return base.into();
        }
        (2..).map(|i| format!("{base}-{i}")).find(|n| !taken(n)).unwrap()
    }

    /// Append a macro and select it.
    pub fn push(&mut self, d: Draft) {
        self.drafts.push(d);
        self.sel = self.drafts.len() - 1;
        self.sel_ev = 0;
        self.confirm_delete = false;
    }

    pub fn new_macro(&mut self) {
        let name = (1..).map(|i| format!("untitled-{i}")).find(|n| !self.drafts.iter().any(|d| d.name == *n)).unwrap();
        self.push(Draft::new(&name, Vec::new()));
    }

    pub fn duplicate(&mut self) {
        let Some(d) = self.drafts.get(self.sel) else { return };
        let name = self.free_name(&format!("{}-copy", d.name));
        let copy = Draft { name, orig: None, ..d.clone() };
        self.push(copy);
    }

    /// Remove the selected macro. With `with_rules`, its rules go too, at the next save.
    pub fn delete(&mut self, with_rules: bool) {
        if self.sel >= self.drafts.len() {
            return;
        }
        let d = self.drafts.remove(self.sel);
        if with_rules {
            self.remove_rules.insert(d.name);
            self.remove_rules.extend(d.orig);
        }
        self.sel = self.sel.min(self.drafts.len().saturating_sub(1));
        self.sel_ev = 0;
        self.confirm_delete = false;
    }

    /// Insert `evs` after the selected row (at the end when none is selected) and select the last one added.
    pub fn insert_events(&mut self, evs: Vec<Ev>) {
        let sel_ev = self.sel_ev;
        let Some(d) = self.drafts.get_mut(self.sel) else { return };
        let at = if d.events.is_empty() { 0 } else { (sel_ev + 1).min(d.events.len()) };
        let n = evs.len();
        d.events.splice(at..at, evs);
        self.sel_ev = at + n - 1;
    }

    /// `drafts` as the file will hold them. Err names the first macro that is not ready.
    pub fn to_macros(&self) -> Result<Macros, String> {
        let mut out = Macros::new();
        for d in &self.drafts {
            let m = d.to_macro().map_err(|e| format!("macro '{}': {e}", d.name))?;
            out.insert(d.name.clone(), m);
        }
        if out.len() != self.drafts.len() {
            return Err("two macros have the same name".into());
        }
        Ok(out)
    }

    /// After a successful save: what is on disk is now what is on screen. Sorted as a reload would, the selection
    /// kept on the same macro.
    fn mark_saved(&mut self) {
        let name = self.drafts.get(self.sel).map(|d| d.name.clone());
        for d in &mut self.drafts {
            d.orig = Some(d.name.clone());
        }
        self.drafts.sort_by(|a, b| a.name.cmp(&b.name));
        self.sel = name.and_then(|n| self.drafts.iter().position(|d| d.name == n)).unwrap_or(0);
        self.saved = self.drafts.clone();
        self.remove_rules.clear();
        self.confirm_delete = false;
    }
}

/// The macros the starters in the empty state create.
pub fn starter(name: &str) -> Option<Draft> {
    use Ev::*;
    let tap = |k: &str| [Key(k.into(), true), Key(k.into(), false)];
    let events = match name {
        "rapid-click" => vec![Button(KeyCode::BTN_LEFT, true), Delay(12), Button(KeyCode::BTN_LEFT, false), Delay(38)],
        "copy-paste" => {
            let mut v = vec![Key("KEY_LEFTCTRL".into(), true)];
            v.extend(tap("KEY_C"));
            v.extend([Key("KEY_LEFTCTRL".into(), false), Delay(100), Key("KEY_LEFTCTRL".into(), true)]);
            v.extend(tap("KEY_V"));
            v.push(Key("KEY_LEFTCTRL".into(), false));
            v
        }
        "double-tap" => {
            let mut v = tap("KEY_SPACE").to_vec();
            v.push(Delay(80));
            v.extend(tap("KEY_SPACE"));
            v
        }
        _ => return None,
    };
    Some(Draft::new(name, events))
}

/// `~/.config/razerctl/macros.conf` for a path under $HOME.
pub fn tilde(p: &Path) -> String {
    match std::env::var_os("HOME").and_then(|h| p.strip_prefix(h).ok().map(|r| r.display().to_string())) {
        Some(rest) => format!("~/{rest}"),
        None => p.display().to_string(),
    }
}

// ------------------------------------------------------------------------------------------- saving

impl App {
    /// Write macros.conf from the editor, and with it the rule rewrites a rename or a delete needs, as one save:
    /// if the second file fails the first is put back. Nothing is sent to the mouse.
    pub fn save_macros_from_ui(&mut self) -> bool {
        if let Some(e) = &self.mac.load_error {
            let e = e.clone();
            self.err(format!("Macros not saved: macros.conf could not be read ({e})."));
            return false;
        }
        let map = match self.mac.to_macros() {
            Ok(m) => m,
            Err(e) => {
                self.err(format!("Macros not saved: {e}"));
                return false;
            }
        };
        let fixes = self.mac.fixes();
        let before = std::fs::read_to_string(&self.macros_path).ok();
        // remap.conf on disk, rewritten in place: only the renamed / removed rules change, not edits still
        // sitting in the rules editor
        let disk_conf = std::fs::read_to_string(&self.conf_path).ok().map(|t| (fix_text(&t, &fixes), t)).filter(|(n, o)| n != o);
        if let Err(e) = macros::save_macros(&self.macros_path, &map) {
            self.err(format!("Macros not saved: {e}"));
            return false;
        }
        if let Some((new, _)) = disk_conf {
            if let Err(e) = write_atomic(&self.conf_path, &new) {
                let back = match &before {
                    Some(t) => write_atomic(&self.macros_path, t),
                    None => std::fs::remove_file(&self.macros_path),
                };
                let note = if back.is_ok() { "macros.conf was put back" } else { "macros.conf could NOT be put back" };
                self.err(format!("Nothing saved: {}: {e} ({note}).", self.conf_path.display()));
                return false;
            }
        }
        self.conf_text = fix_text(&self.conf_text, &fixes);
        self.mac.mark_saved();
        self.ok(format!("Macros saved to {}. Restart the service to apply.", tilde(&self.macros_path)));
        true
    }

    pub fn remap_state(&self) -> Remap {
        if self.svc_failure.is_some() {
            Remap::Failed
        } else if self.service_state != "active" {
            Remap::Stopped
        } else if self.info.is_none() {
            Remap::NoDevice
        } else {
            Remap::Running
        }
    }
}

/// What was pressed in the banner this frame.
#[derive(Default, Clone, Copy)]
pub struct BannerClick {
    pub start: bool,
    pub stop_macros: bool,
}

// --------------------------------------------------------------------------------------------- drawing

/// Combo box over `items`; returns the index picked this frame.
fn combo(ui: &mut Ui, p: &Pal, id: impl std::hash::Hash + std::fmt::Debug, w: f32, items: &[String], cur: usize) -> Option<usize> {
    let mut picked = None;
    let text = items.get(cur).cloned().unwrap_or_default();
    ComboBox::from_id_salt(id)
        .width(w)
        .selected_text(RichText::new(text).font(sans(T_BODY - 1.0)).color(p.i100()))
        .show_ui(ui, |ui| {
            ui.spacing_mut().interact_size.y = 24.0;
            ui.spacing_mut().item_spacing.y = 2.0;
            for (i, l) in items.iter().enumerate() {
                if ui.selectable_label(i == cur, l).clicked() {
                    picked = Some(i);
                }
            }
        });
    picked
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// Labelled field: caps caption above, control below.
fn field(ui: &mut Ui, p: &Pal, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.label(caps(p, label));
        add(ui);
    });
}

/// The mode picker: one bordered box like `segmented`, but a segment can be disabled (Sequence). Returns the
/// segment clicked.
fn mode_seg(ui: &mut Ui, p: &Pal, active: usize, disabled: &[usize]) -> Option<usize> {
    let mut clicked = None;
    Frame::new().stroke(Stroke::new(1.0, p.i45())).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for (i, l) in MODES.iter().enumerate() {
                let (on, dis) = (i == active, disabled.contains(&i));
                let (fg, fill) = if on {
                    (p.stamp_fg, p.stamp_bg)
                } else if dis {
                    (p.i45(), Color32::TRANSPARENT)
                } else {
                    (p.i65(), Color32::TRANSPARENT)
                };
                let mut b = egui::Button::new(tracked(&l.to_uppercase(), mono_med(T_LABEL), fg, TRACK_LABEL))
                    .fill(fill)
                    .stroke(Stroke::NONE)
                    .corner_radius(R0)
                    .min_size(vec2(88.0, 32.0));
                if dis {
                    b = b.sense(Sense::hover());
                }
                if ui.add(b).clicked() {
                    clicked = Some(i);
                }
                if i + 1 < MODES.len() {
                    let r = ui.min_rect();
                    ui.painter().vline(r.right(), r.y_range(), Stroke::new(1.0, p.i18()));
                }
            }
        });
    });
    clicked
}

fn mono_field(ui: &mut Ui, p: &Pal, text: &mut String, w: f32) -> egui::Response {
    ui.add(TextEdit::singleline(text).desired_width(w).font(mono(T_DATA + 1.0)).text_color(p.i100()).margin(Margin::symmetric(8, 3)))
}

fn small_btn(ui: &mut Ui, p: &Pal, glyph: &str, enabled: bool) -> bool {
    let fg = if enabled { p.i100() } else { p.i45() };
    let mut b = egui::Button::new(RichText::new(glyph).font(mono(T_DATA)).color(fg))
        .corner_radius(R0)
        .min_size(vec2(26.0, 20.0))
        .fill(Color32::TRANSPARENT)
        .stroke(Stroke::new(1.0, if enabled { p.i45() } else { p.i18() }));
    if !enabled {
        b = b.sense(Sense::hover());
    }
    ui.add(b).clicked()
}

fn fmt_s(ms: u64) -> String {
    format!("{:.2} s", ms as f64 / 1000.0)
}

/// Fixed-size table cell. `set_min_size` is required or the cell shrinks to its content.
fn cell(ui: &mut Ui, w: f32, h: f32, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(vec2(w, h), Layout::left_to_right(Align::Center), |ui| {
        ui.set_min_size(vec2(w, h));
        ui.spacing_mut().button_padding = vec2(6.0, 2.0);
        add(ui);
    });
}

/// Rows of the macro list shown at once below 900 px: three and a half, so the next row peeks out.
const LIST_ROWS_NARROW: f32 = 3.5;

/// `add`, inside a vertical scroll area of fixed height `h` when `scroll` (long lists), else as it is.
fn scroll_if(ui: &mut Ui, scroll: bool, id: &str, h: f32, bar_visible: bool, add: impl FnOnce(&mut Ui)) {
    if scroll {
        let vis = if bar_visible { egui::scroll_area::ScrollBarVisibility::AlwaysVisible } else { egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded };
        egui::ScrollArea::vertical().id_salt(id).max_height(h).min_scrolled_height(h).auto_shrink([false, false]).scroll_bar_visibility(vis).show(ui, add);
    } else {
        add(ui);
    }
}

/// What a click in the editor asks for; applied once the frame's borrows are done.
pub(super) enum Act {
    None,
    Delete,
    DeleteMacro { with_rules: bool },
    CancelDelete,
    Duplicate,
    BindTo,
    Save { restart: bool },
    Revert,
    Add(Vec<Ev>),
    Up(usize),
    Down(usize),
    Remove(usize),
}

impl App {
    /// `Macros · n` label of the sub-nav.
    pub fn macros_label(&self) -> String {
        match &self.mac.load_error {
            Some(_) => "Macros · !".into(),
            None => format!("Macros · {}", self.mac.drafts.len()),
        }
    }

    /// One-line honesty banner beside the sub-nav: whether macros can play right now, and why not. Reports which
    /// button was pressed: "Start service" / "Try again", or "Stop all macros" (DAN-1452 decision 10: always shown
    /// while remap runs, because v1 has no playback status to tell whether anything is playing).
    pub fn macro_banner(&self, ui: &mut Ui) -> BannerClick {
        let p = self.pal;
        let mut start = false;
        let mut stop_macros = false;
        let h = 34.0;
        let w = ui.available_width();
        ui.allocate_ui_with_layout(vec2(w, h), Layout::left_to_right(Align::Center), |ui| {
            ui.set_min_size(vec2(w, h));
            ui.spacing_mut().item_spacing.x = S3;
            let framed = |ui: &mut Ui, add: &mut dyn FnMut(&mut Ui)| {
                Frame::new().fill(p.i06()).stroke(Stroke::new(1.0, p.i45())).inner_margin(Margin::symmetric(12, 1)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.set_min_height(h - 2.0);
                        ui.spacing_mut().item_spacing.x = S3;
                        add(ui)
                    });
                });
            };
            match self.remap_state() {
                Remap::Stopped => framed(ui, &mut |ui| {
                    chip(ui, &p, Some("▲"), "Remap not running", ChipStyle::Stamp, p.stamp_fg);
                    ui.label(RichText::new("Macros won't play until the service is started.").font(sans(T_SMALL)).color(p.i100()));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        start = btn(ui, &p, "Start service", Kind::Primary, true).clicked();
                    });
                }),
                Remap::Failed => framed(ui, &mut |ui| {
                    chip(ui, &p, Some("×"), "Remap failed", ChipStyle::Outline, p.bad);
                    let why = self.svc_failure.as_ref().map(|f| f.full()).unwrap_or_default();
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        start = btn(ui, &p, "Try again", Kind::Primary, true).clicked();
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.add(egui::Label::new(RichText::new(&why).font(mono(T_DATA)).color(p.bad)).truncate()).on_hover_text(&why);
                        });
                    });
                }),
                Remap::NoDevice => framed(ui, &mut |ui| {
                    chip(ui, &p, Some("○"), "No mouse", ChipStyle::Outline, p.i100());
                    ui.add(egui::Label::new(
                        RichText::new("Remap is waiting for the mouse. Running macros were stopped; bound buttons work again on reconnect.")
                            .font(sans(T_SMALL)).color(p.i100()),
                    ).truncate());
                }),
                Remap::Running => {
                    ui.label(RichText::new("●").font(mono(T_LABEL + 1.0)).color(p.ok));
                    ui.label(tracked("REMAP RUNNING", mono_med(T_LABEL), p.i100(), 0.12));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        stop_macros = btn(ui, &p, "Stop all macros", Kind::Secondary, true).clicked();
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.add(egui::Label::new(
                                RichText::new("Macros play on this computer while the service runs.")
                                    .font(sans(T_SMALL)).color(p.i65()),
                            ).truncate());
                        });
                    });
                }
            }
        });
        BannerClick { start, stop_macros }
    }

    pub fn ui_rules_view(&mut self, ui: &mut Ui) {
        let p = self.pal;
        let total = ui.available_width();
        let narrow = total < 900.0;
        let right_w = if narrow { total } else { (total * 0.30).clamp(260.0, 340.0) };
        let gap = S4;
        let left_w = if narrow { total } else { total - right_w - gap };
        let layout = if narrow { Layout::top_down(Align::Min) } else { Layout::left_to_right(Align::Min) };
        ui.with_layout(layout, |ui| {
            ui.spacing_mut().item_spacing.x = gap;
            ui.allocate_ui_with_layout(vec2(left_w, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(left_w);
                section(ui, &p, "01", "Rules");
                self.config_panel(ui);
                ui.add_space(S2);
                section(ui, &p, "02", "Add a rule");
                self.bind_strip(ui);
            });
            ui.allocate_ui_with_layout(vec2(right_w, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(right_w);
                section(ui, &p, "03", "Service");
                self.service_panel(ui);
            });
        });
    }

    fn bind_strip(&mut self, ui: &mut Ui) {
        let p = self.pal;
        let names = self.mac.names();
        if !names.contains(&self.mac.bind.macro_name) {
            self.mac.bind.macro_name = names.first().cloned().unwrap_or_default();
        }
        let line = self.mac.bind.line(&self.mac.drafts);
        let mut add_rule = false;
        let b = &mut self.mac.bind;
        panel(ui, &p, 16, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = S3;
                field(ui, &p, "Button", |ui| {
                    let items: Vec<String> = BUTTONS.iter().map(|(l, _)| l.to_string()).collect();
                    if let Some(i) = combo(ui, &p, "bind-button", 150.0, &items, b.button) { b.button = i; }
                });
                field(ui, &p, "Does", |ui| {
                    if let Some(i) = combo(ui, &p, "bind-does", 180.0, &strings(&DOES), b.does) { b.does = i; }
                });
                if b.does == DOES_MACRO {
                    field(ui, &p, "Macro", |ui| {
                        let cur = names.iter().position(|n| *n == b.macro_name).unwrap_or(0);
                        if let Some(i) = combo(ui, &p, "bind-macro", 200.0, &names, cur) { b.macro_name = names[i].clone(); }
                    });
                } else if b.does == 0 {
                    field(ui, &p, "Key", |ui| {
                        ui.add(TextEdit::singleline(&mut b.key).hint_text("e.g. ctrl+c, f13, space").desired_width(200.0)
                            .font(mono(T_DATA + 1.0)).text_color(p.i100()).margin(Margin::symmetric(8, 6)));
                    });
                }
                ui.with_layout(Layout::right_to_left(Align::Max), |ui| {
                    add_rule = btn(ui, &p, "Add rule  →", Kind::Primary, line.is_ok()).clicked();
                });
            });
            if b.does == DOES_MACRO {
                ui.add_space(S3);
                field(ui, &p, "How it plays", |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = S3;
                        if let Some(i) = mode_seg(ui, &p, b.mode, &[MODE_SEQUENCE]) { b.mode = i; }
                        if b.mode == 1 {
                            ui.add(DragValue::new(&mut b.times).range(1..=remap::MAX_TIMES).prefix("× "));
                        }
                        ui.label(RichText::new(b.mode_help()).font(sans(T_SMALL)).color(p.i65()));
                    });
                });
            }
            ui.add_space(S3);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = S3;
                ui.label(caps(&p, "Adds this line"));
                match &line {
                    Ok(l) => {
                        Frame::new().fill(p.sunken).stroke(Stroke::new(1.0, p.i18())).inner_margin(Margin::symmetric(10, 4)).show(ui, |ui| {
                            ui.label(RichText::new(l).font(mono_med(T_DATA + 1.0)).color(p.i100()));
                        });
                        ui.label(RichText::new("Nothing is saved until you press Save config.").font(sans(T_SMALL)).color(p.i65()));
                    }
                    Err(why) => {
                        ui.label(RichText::new("▲").font(mono(T_LABEL + 1.0)).color(p.warn));
                        ui.add(egui::Label::new(RichText::new(why).font(sans(T_SMALL)).color(p.i100())).wrap());
                    }
                }
            });
        });
        if add_rule {
            if let Ok(l) = line {
                if !self.conf_text.is_empty() && !self.conf_text.ends_with('\n') {
                    self.conf_text.push('\n');
                }
                self.conf_text.push_str(&l);
                self.conf_text.push('\n');
                self.ok(format!("Added \"{l}\" to the rules. Nothing is saved until you press Save config."));
            }
        }
    }

    pub fn ui_macros_view(&mut self, ui: &mut Ui) {
        let p = self.pal;
        let total = ui.available_width();
        let narrow = total < 900.0;
        let list_w = if narrow { total } else { 300.0 };
        let gap = S4;
        let right_w = if narrow { total } else { total - list_w - gap };
        let layout = if narrow { Layout::top_down(Align::Min) } else { Layout::left_to_right(Align::Min) };
        if let Some(e) = self.mac.load_error.clone() {
            section(ui, &p, "01", "Macros");
            panel(ui, &p, 20, |ui| {
                ui.label(RichText::new("macros.conf can't be read.").font(serif(T_TITLE)).color(p.i100()));
                ui.add_space(S2);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = S3;
                    chip(ui, &p, Some("×"), "Invalid", ChipStyle::Outline, p.bad);
                    ui.add(egui::Label::new(RichText::new(&e).font(mono(T_DATA)).color(p.bad)).wrap());
                });
                ui.add_space(S2);
                ui.add(egui::Label::new(RichText::new(
                    "The file is left as it is: nothing here will overwrite it. Fix it by hand, then reload.",
                ).font(sans(T_BODY - 1.0)).color(p.i65())).wrap());
                ui.add_space(S3);
                if btn(ui, &p, "↻  Reload", Kind::Primary, true).clicked() {
                    self.mac = MacroUi { sub: Sub::Macros, ..MacroUi::load(&self.macros_path) };
                }
            });
            return;
        }
        if self.mac.drafts.is_empty() {
            ui.with_layout(layout, |ui| {
                ui.spacing_mut().item_spacing.x = gap;
                ui.allocate_ui_with_layout(vec2(list_w, 0.0), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(list_w);
                    section(ui, &p, "01", "Macros");
                    self.empty_list(ui);
                });
                ui.allocate_ui_with_layout(vec2(right_w, 0.0), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(right_w);
                    section(ui, &p, "02", "How macros run");
                    how_macros_run(ui, &p);
                });
            });
            return;
        }
        self.mac.sel = self.mac.sel.min(self.mac.drafts.len() - 1);
        ui.with_layout(layout, |ui| {
            ui.spacing_mut().item_spacing.x = gap;
            ui.allocate_ui_with_layout(vec2(list_w, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(list_w);
                section(ui, &p, "01", "Macros");
                self.macro_list(ui, narrow);
            });
            ui.allocate_ui_with_layout(vec2(right_w, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(right_w);
                section(ui, &p, "02", "Edit");
                self.editor(ui);
            });
        });
    }

    fn empty_list(&mut self, ui: &mut Ui) {
        let p = self.pal;
        let mut pick: Option<&'static str> = None;
        let mut new = false;
        panel(ui, &p, 16, |ui| {
            ui.label(RichText::new("No macros yet.").font(serif(T_TITLE)).color(p.i100()));
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new("A macro is a named list of key presses, clicks, scrolls and waits. Make one, then bind it to a button.")
                .font(sans(T_BODY - 1.0)).color(p.i65())).wrap());
            ui.add_space(S4);
            new = btn_w(ui, &p, "+  New macro", Kind::Primary, true, ui.available_width()).clicked();
            ui.add_space(S4);
            hair(ui, &p);
            ui.add_space(S3);
            ui.label(caps(&p, "Or start from"));
            ui.add_space(S2);
            for (n, d) in [("rapid-click", "left click every 50 ms"), ("copy-paste", "ctrl+c, wait, ctrl+v"), ("double-tap", "one key, twice, 80 ms apart")] {
                let r = Frame::new().stroke(Stroke::new(1.0, p.i28())).inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(n).font(mono_bold(T_DATA)).color(p.i100()));
                    ui.label(RichText::new(d).font(sans(T_SMALL)).color(p.i65()));
                });
                let hit = ui.interact(r.response.rect, ui.id().with(("starter", n)), Sense::click());
                if hit.hovered() {
                    ui.painter().rect_stroke(r.response.rect, R0, Stroke::new(1.0, p.i100()), egui::StrokeKind::Inside);
                }
                if hit.clicked() {
                    pick = Some(n);
                }
            }
        });
        if new {
            self.mac.new_macro();
        }
        if let Some(d) = pick.and_then(starter) {
            let name = self.mac.free_name(&d.name);
            self.mac.push(Draft { name, ..d });
        }
    }

    fn macro_list(&mut self, ui: &mut Ui, narrow: bool) {
        let p = self.pal;
        let refs = rule_refs(&self.conf_text);
        let issues = self.mac.all_issues();
        let sel = self.mac.sel;
        let mut clicked = None;
        let mut new = false;
        // below 900 px the panels stack, so the list must not push the editor off the screen: compact rows, capped at
        // 3.5 rows (the half row says "more below") with a solid scrollbar, and the selected row scrolled into view
        let scroll_sel = self.mac.list_seen != Some(sel);
        self.mac.list_seen = Some(sel);
        Frame::new().fill(p.raised).stroke(Stroke::new(1.0, p.i18())).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            let row_h = if narrow { 54.0 } else { 76.0 };
            let (capped, cap_h) = if narrow { (self.mac.drafts.len() > 3, row_h * LIST_ROWS_NARROW) } else { (self.mac.drafts.len() > 8, row_h * 8.0) };
            if narrow {
                // egui's handle is `bg_fill` of the inactive widget, which this theme sets to the track colour (invisible)
                let st = ui.style_mut();
                st.spacing.scroll = egui::style::ScrollStyle::solid();
                st.spacing.scroll.bar_width = 8.0;
                st.spacing.scroll.floating_allocated_width = 0.0;
                st.visuals.widgets.inactive.bg_fill = p.i65_on(p.sunken);
                st.visuals.widgets.hovered.bg_fill = p.i100();
                st.visuals.widgets.active.bg_fill = p.i100();
            }
            scroll_if(ui, capped, "macro-list", cap_h, narrow, |ui| {
                for (i, d) in self.mac.drafts.iter().enumerate() {
                    let w = ui.available_width();
                    let (rect, resp) = ui.allocate_exact_size(vec2(w, row_h), Sense::click());
                    let is_sel = i == sel;
                    let fill = if is_sel { p.i10() } else if resp.hovered() { p.i06() } else { Color32::TRANSPARENT };
                    ui.painter().add(Shape::Rect(RectShape::filled(rect, R0, fill)));
                    if is_sel {
                        ui.painter().add(Shape::Rect(RectShape::filled(Rect::from_min_size(rect.min, vec2(3.0, rect.height())), R0, p.stamp_bg)));
                    }
                    ui.painter().hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, p.i18()));
                    if is_sel && scroll_sel { ui.scroll_to_rect(rect, Some(Align::Center)); }
                    let inner = Rect::from_min_max(pos2(rect.left() + S4, rect.top()), pos2(rect.right() - S3, rect.bottom()));
                    let mut c = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(Layout::top_down(Align::Min)));
                    c.spacing_mut().item_spacing.y = 2.0;
                    if narrow { c.spacing_mut().interact_size.y = 18.0; }
                    c.add_space(if narrow { 8.0 } else { 10.0 });
                    c.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        let name = if d.name.is_empty() { "(no name)" } else { d.name.as_str() };
                        ui.add(egui::Label::new(RichText::new(name).font(mono_bold(T_DATA + 1.0)).color(p.i100())).truncate());
                        if !issues[i].is_empty() {
                            chip(ui, &p, Some("×"), "Invalid", ChipStyle::Plain, p.bad);
                        }
                    });
                    let detail = if d.events.is_empty() {
                        "NO EVENTS".to_string()
                    } else {
                        format!("{} EVENT{} · {}", d.events.len(), if d.events.len() == 1 { "" } else { "S" }, fmt_s(d.run_ms()).to_uppercase())
                    };
                    let used = used_by(d, &refs);
                    let used_txt = if used.is_empty() { "UNUSED".to_string() } else { used.join("  /  ") };
                    let used_col = if used.is_empty() { p.i45() } else { p.i65() };
                    if narrow {
                        // one meta line: what it is, then where it is used (one label: a `horizontal` row would be as
                        // tall as a button and push the line out of its 54 px)
                        c.add(egui::Label::new(tracked(&format!("{detail}   → {used_txt}"), mono(T_MICRO), used_col, 0.10)).truncate());
                    } else {
                        c.label(tracked(&detail, mono(T_MICRO), p.i65(), 0.10));
                        c.add(egui::Label::new(tracked(&format!("→ {used_txt}"), mono(T_MICRO), used_col, 0.10)).truncate());
                    }
                    if resp.clicked() {
                        clicked = Some(i);
                    }
                }
            });
            ui.add_space(S3);
            ui.horizontal(|ui| {
                ui.add_space(S4);
                new = btn_w(ui, &p, "+  New macro", Kind::Primary, true, ui.available_width() - S4).clicked();
            });
            ui.add_space(S3);
        });
        if let Some(i) = clicked {
            self.mac.sel = i;
            self.mac.sel_ev = 0;
            self.mac.confirm_delete = false;
        }
        if new {
            self.mac.new_macro();
        }
    }

    fn editor(&mut self, ui: &mut Ui) {
        let p = self.pal;
        let sel = self.mac.sel;
        let all_issues = self.mac.all_issues();
        let issues = all_issues[sel].clone();
        let bad_total = all_issues.iter().filter(|v| !v.is_empty()).count();
        let refs = rule_refs(&self.conf_text);
        let used = used_by(&self.mac.drafts[sel], &refs);
        let dirty = self.mac.dirty();
        let can_save = self.mac.can_save();
        let confirm = self.mac.confirm_delete;
        let sel_ev = self.mac.sel_ev;
        let mut new_sel_ev = None;
        let mut act = Act::None;
        let d = &mut self.mac.drafts[sel];
        let run_ms = d.run_ms();
        panel(ui, &p, 16, |ui| {
            // --- header: name + duplicate / delete, or the inline delete confirmation
            if confirm {
                Frame::new().fill(p.i06()).stroke(Stroke::new(1.0, p.i45())).inner_margin(Margin::same(12)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = S2;
                        ui.label(RichText::new("▲").font(mono(T_LABEL + 1.0)).color(p.i100()));
                        ui.label(RichText::new(format!("Delete '{}'?", d.name)).font(serif(T_TITLE - 2.0)).color(p.i100()));
                    });
                    ui.add_space(2.0);
                    ui.add(egui::Label::new(RichText::new(format!(
                        "It is used by {} rule{}: {}. Deleting it leaves {} pointing at nothing, and Save config stays blocked until the rule is fixed. \
                         Nothing is final until you press Save macros; Revert brings it back.",
                        used.len(), if used.len() == 1 { "" } else { "s" }, used.join(", "),
                        if used.len() == 1 { "that rule" } else { "those rules" },
                    )).font(sans(T_BODY - 1.0)).color(p.i65())).wrap());
                    ui.add_space(S3);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = S2;
                        if btn(ui, &p, "Delete and remove rule", Kind::Primary, true).clicked() { act = Act::DeleteMacro { with_rules: true }; }
                        if btn(ui, &p, "Delete macro only", Kind::Secondary, true).clicked() { act = Act::DeleteMacro { with_rules: false }; }
                        if btn(ui, &p, "Cancel", Kind::Secondary, true).clicked() { act = Act::CancelDelete; }
                    });
                });
            } else {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = S3;
                    ui.label(caps(&p, "Name"));
                    mono_field(ui, &p, &mut d.name, 220.0);
                    ui.label(RichText::new("letters, digits, - and _").font(sans(T_SMALL)).color(p.i65()));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if btn(ui, &p, "Delete", Kind::Secondary, true).clicked() { act = Act::Delete; }
                        if btn(ui, &p, "Duplicate", Kind::Secondary, true).clicked() { act = Act::Duplicate; }
                    });
                });
                ui.add_space(S1);
                let roomy = ui.available_width() > 760.0;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = S2;
                    ui.label(caps(&p, "Used by"));
                    if used.is_empty() {
                        ui.label(RichText::new("no button yet").font(sans(T_SMALL)).color(p.i65()));
                    }
                    for u in &used {
                        chip(ui, &p, None, u, ChipStyle::Outline, p.i100());
                    }
                    // the caption goes first when the panel is narrow, or the Bind button would be pushed out of it
                    if !used.is_empty() && roomy {
                        ui.label(RichText::new("Renaming updates these rules in the config.").font(sans(T_SMALL)).color(p.i65()));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if btn(ui, &p, "Bind to a button  →", Kind::Secondary, true).clicked() { act = Act::BindTo; }
                    });
                });
            }
            ui.add_space(S2);
            // --- event table
            let w = ui.available_width();
            let cols = [32.0, 22.0, 80.0, w - 32.0 - 22.0 - 80.0 - 84.0 - 104.0, 84.0, 104.0];
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                for (i, h) in ["#", "", "Event", "Value", "At", ""].iter().enumerate() {
                    cell(ui, cols[i], 16.0, |ui| { ui.label(caps(&p, h)); });
                }
            });
            ui.add_space(S1);
            let n_events = d.events.len();
            Frame::new().fill(p.sunken).stroke(Stroke::new(1.0, p.i18())).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                if d.events.is_empty() {
                    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 84.0), Sense::hover());
                    let mut c = ui.new_child(egui::UiBuilder::new().max_rect(r.shrink(S4)).layout(Layout::top_down(Align::Min)));
                    c.label(RichText::new("No events yet.").font(serif(T_TITLE - 2.0)).color(p.i100()));
                    c.label(RichText::new("Add a key press, a click or a delay below.").font(sans(T_BODY - 1.0)).color(p.i65()));
                }
                scroll_if(ui, n_events > 14, "macro-events", 26.0 * 14.0, false, |ui| {
                    ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                    let mut at: u32 = 0;
                    for (i, e) in d.events.iter_mut().enumerate() {
                        let is_delay = matches!(e, Ev::Delay(_));
                        let row_h = if is_delay { 20.0 } else { 26.0 };
                        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                        let bad = issues.iter().any(|x| x.ev == Some(i));
                        let is_sel = i == sel_ev;
                        if ui.rect_contains_pointer(rect) && ui.input(|inp| inp.pointer.primary_pressed()) { new_sel_ev = Some(i); }
                        if is_sel {
                            ui.painter().add(Shape::Rect(RectShape::filled(rect, R0, p.i10())));
                            ui.painter().add(Shape::Rect(RectShape::filled(Rect::from_min_size(rect.min, vec2(3.0, rect.height())), R0, p.stamp_bg)));
                        } else if is_delay {
                            ui.painter().add(Shape::Rect(RectShape::filled(rect, R0, p.i06())));
                        }
                        ui.painter().hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, p.i18()));
                        let mut c = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(Layout::left_to_right(Align::Center)));
                        c.spacing_mut().item_spacing = vec2(0.0, 0.0);
                        c.spacing_mut().interact_size.y = 20.0;
                        // ink-65 on the selected row of the light theme is 4.45:1, so its secondary text goes to full ink
                        let sec = if is_sel { p.i100() } else { p.i65() };
                        let dim = if is_delay { sec } else { p.i100() };
                        cell(&mut c, cols[0], row_h, |ui| { ui.add_space(S3); ui.label(RichText::new(format!("{:02}", i + 1)).font(mono(T_DATA)).color(sec)); });
                        cell(&mut c, cols[1], row_h, |ui| { if bad { ui.label(RichText::new("×").font(mono_bold(T_DATA + 2.0)).color(p.bad)); } });
                        cell(&mut c, cols[2], row_h, |ui| { ui.label(tracked(e.kind(), mono_med(T_LABEL), dim, 0.10)); });
                        let kind = e.kind();
                        cell(&mut c, cols[3], row_h, |ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            ui.style_mut().override_font_id = Some(mono(T_DATA + 1.0));
                            match e {
                                Ev::Key(t, _) => {
                                    let r = mono_field(ui, &p, t, 190.0);
                                    if r.lost_focus() {
                                        // spell it the way the file will (`ctrl` -> KEY_LEFTCTRL)
                                        if let Ok(k) = remap::parse_key(t) { *t = format!("{k:?}"); }
                                    }
                                }
                                Ev::Button(k, _) => {
                                    let items: Vec<String> = BUTTON_CODES.iter().map(|b| format!("{b:?}")).collect();
                                    let cur = BUTTON_CODES.iter().position(|b| b == k).unwrap_or(0);
                                    if let Some(j) = combo(ui, &p, ("evb", sel, i), 150.0, &items, cur) { *k = BUTTON_CODES[j]; }
                                }
                                Ev::Wheel(n) | Ev::HWheel(n) => {
                                    ui.add(DragValue::new(n).range(-(macros::MAX_WHEEL as i32)..=macros::MAX_WHEEL as i32));
                                    let word = match (kind == "HWHEEL", *n < 0) {
                                        (false, true) => "ticks, down",
                                        (false, false) => "ticks, up",
                                        (true, true) => "ticks, left",
                                        (true, false) => "ticks, right",
                                    };
                                    ui.label(RichText::new(word).font(sans(T_SMALL)).color(p.i65()));
                                }
                                Ev::Move(x, y) => {
                                    let m = macros::MAX_MOVE as i32;
                                    ui.add(DragValue::new(x).prefix("dx ").range(-m..=m));
                                    ui.add(DragValue::new(y).prefix("dy ").range(-m..=m));
                                    ui.label(RichText::new("pixels, relative").font(sans(T_SMALL)).color(p.i65()));
                                }
                                Ev::Delay(n) => {
                                    ui.add(DragValue::new(n).suffix(" ms").range(1..=macros::MAX_DELAY_MS));
                                }
                            }
                        });
                        cell(&mut c, cols[4], row_h, |ui| { ui.label(RichText::new(format!("{:.3} s", at as f32 / 1000.0)).font(mono(T_MICRO + 1.0)).color(sec)); });
                        cell(&mut c, cols[5], row_h, |ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            if small_btn(ui, &p, "↑", i > 0) { act = Act::Up(i); }
                            if small_btn(ui, &p, "↓", i + 1 < n_events) { act = Act::Down(i); }
                            if small_btn(ui, &p, "×", true) { act = Act::Remove(i); }
                        });
                        if let Ev::Delay(n) = e { at += *n; }
                    }
                });
            });
            ui.add_space(S2);
            // --- add bar (left) + validity and run time (right; under the buttons when the panel is narrow)
            let stack_bar = w < 800.0;
            let validity = |ui: &mut Ui| {
                // the problem count lives in the problem box below, next to the messages it counts (DAN-1452 note f)
                if issues.is_empty() {
                    chip(ui, &p, Some("●"), "Valid", ChipStyle::Plain, p.ok);
                    ui.label(RichText::new(format!("{} events · about {} per run", n_events, fmt_s(run_ms))).font(mono(T_DATA)).color(p.i100()));
                }
            };
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = S2;
                ui.label(caps(&p, "Add"));
                if btn(ui, &p, "+ Tap key", Kind::Secondary, true).clicked() {
                    act = Act::Add(vec![Ev::Key("KEY_A".into(), true), Ev::Key("KEY_A".into(), false)]);
                }
                if btn(ui, &p, "+ Click", Kind::Secondary, true).clicked() {
                    act = Act::Add(vec![Ev::Button(KeyCode::BTN_LEFT, true), Ev::Button(KeyCode::BTN_LEFT, false)]);
                }
                if btn(ui, &p, "+ Delay", Kind::Secondary, true).clicked() {
                    act = Act::Add(vec![Ev::Delay(50)]);
                }
                let other = btn(ui, &p, "+ Other  ▾", Kind::Secondary, true);
                egui::Popup::menu(&other).show(|ui| {
                    let items: [(&str, Ev); 7] = [
                        ("Key down", Ev::Key("KEY_A".into(), true)),
                        ("Key up", Ev::Key("KEY_A".into(), false)),
                        ("Button down", Ev::Button(KeyCode::BTN_LEFT, true)),
                        ("Button up", Ev::Button(KeyCode::BTN_LEFT, false)),
                        ("Scroll wheel", Ev::Wheel(1)),
                        ("Horizontal scroll", Ev::HWheel(1)),
                        ("Move cursor", Ev::Move(10, 0)),
                    ];
                    for (l, ev) in items {
                        if ui.button(l).clicked() { act = Act::Add(vec![ev]); }
                    }
                });
                if !stack_bar && issues.is_empty() {
                    ui.add_space(S3);
                    validity(ui);
                }
            });
            if stack_bar && issues.is_empty() {
                ui.add_space(S1);
                ui.horizontal(validity);
            }
            // --- problems
            if !issues.is_empty() {
                ui.add_space(S3);
                Frame::new().fill(p.i06()).stroke(Stroke::new(1.0, p.i45())).inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = 4.0;
                    ui.label(tracked(&format!("× {} PROBLEM{}", issues.len(), if issues.len() == 1 { "" } else { "S" }), mono_med(T_LABEL), p.bad, 0.12));
                    for is in &issues {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            ui.label(RichText::new("×").font(mono_bold(T_DATA + 2.0)).color(p.bad));
                            ui.add(egui::Label::new(RichText::new(&is.msg).font(sans(T_BODY - 1.0)).color(p.i100())).wrap());
                        });
                    }
                });
            }
            ui.add_space(S2);
            // --- action bar
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = S2;
                if btn(ui, &p, "💾  Save macros", Kind::Primary, can_save).clicked() { act = Act::Save { restart: false }; }
                if btn(ui, &p, "Save and restart service", Kind::Secondary, can_save).clicked() { act = Act::Save { restart: true }; }
                if btn(ui, &p, "Revert", Kind::Secondary, dirty).clicked() { act = Act::Revert; }
                ui.add_space(S3);
                if bad_total > 0 {
                    chip(ui, &p, Some("×"), &format!("{bad_total} macro{} invalid", if bad_total == 1 { "" } else { "s" }), ChipStyle::Outline, p.bad);
                } else if dirty {
                    chip(ui, &p, Some("◐"), "Unsaved", ChipStyle::Outline, p.warn);
                } else {
                    chip(ui, &p, Some("●"), "Saved", ChipStyle::Plain, p.i65());
                }
            });
        });
        if let Some(i) = new_sel_ev {
            self.mac.sel_ev = i;
        }
        self.apply(act);
    }

    /// The Delete button: a macro no rule uses goes at once (nothing is final until Save; Revert brings it back),
    /// one a rule uses asks first, so the rule is not left pointing at nothing by accident.
    pub fn request_delete(&mut self) {
        let Some(d) = self.mac.drafts.get(self.mac.sel) else { return };
        if used_by(d, &rule_refs(&self.conf_text)).is_empty() {
            self.mac.delete(false);
        } else {
            self.mac.confirm_delete = true;
        }
        self.ok("Nothing is deleted until you press Save macros.");
    }

    pub(super) fn apply(&mut self, act: Act) {
        let sel = self.mac.sel;
        match act {
            Act::None => {}
            Act::Delete => self.request_delete(),
            Act::DeleteMacro { with_rules } => {
                self.mac.delete(with_rules);
                if with_rules {
                    self.ok("Macro and its rules removed. Nothing is final until you press Save macros.");
                } else {
                    self.err("The rules that used the deleted macro still point at it. Fix them in Rules before saving the config.");
                }
            }
            Act::CancelDelete => self.mac.confirm_delete = false,
            Act::Duplicate => self.mac.duplicate(),
            Act::BindTo => {
                self.mac.bind.does = DOES_MACRO;
                self.mac.bind.macro_name = self.mac.drafts[sel].name.clone();
                self.mac.sub = Sub::Rules;
            }
            Act::Save { restart } => {
                if self.save_macros_from_ui() && restart {
                    // the verdict (running, or why not) arrives once the unit has been read back; see service.rs
                    self.restart_after_save();
                }
            }
            Act::Revert => {
                self.mac.revert();
                self.ok("Reverted to the saved macros.");
            }
            Act::Add(evs) => self.mac.insert_events(evs),
            Act::Up(i) => {
                if let Some(d) = self.mac.drafts.get_mut(sel).filter(|_| i > 0) {
                    d.events.swap(i, i - 1);
                    self.mac.sel_ev = i - 1;
                }
            }
            Act::Down(i) => {
                if let Some(d) = self.mac.drafts.get_mut(sel).filter(|d| i + 1 < d.events.len()) {
                    d.events.swap(i, i + 1);
                    self.mac.sel_ev = i + 1;
                }
            }
            Act::Remove(i) => {
                if let Some(d) = self.mac.drafts.get_mut(sel).filter(|d| i < d.events.len()) {
                    d.events.remove(i);
                    self.mac.sel_ev = i.saturating_sub(1).min(d.events.len().saturating_sub(1));
                }
            }
        }
    }
}

fn how_macros_run(ui: &mut Ui, p: &Pal) {
    panel(ui, p, 20, |ui| {
        ui.label(RichText::new("Played by this computer, not by the mouse.").font(serif(T_TITLE)).color(p.i100()));
        ui.add_space(S3);
        for (k, t) in [
            ("Needs the service", "A macro plays only while razerctl-remap.service is running and holds the mouse. Stop the service, unplug the mouse or reload the rules, and any running macro stops."),
            ("The button stays stock", "Nothing is written to the mouse. On another computer, or with the service stopped, the button does what it did before."),
            ("Playback modes", "Once, a set number of times, repeat while the button is held, or toggle on and off with a second press."),
            ("Typed, not recorded", "Build the event list by hand. Live recording is not part of this version."),
        ] {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = S4;
                ui.allocate_ui_with_layout(vec2(170.0, 20.0), Layout::top_down(Align::Min), |ui| {
                    ui.set_min_width(170.0);
                    ui.label(caps(p, k));
                });
                ui.add(egui::Label::new(RichText::new(t).font(sans(T_BODY - 1.0)).color(p.i65())).wrap());
            });
            ui.add_space(S3);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac(name: &str, events: Vec<Ev>) -> Draft {
        Draft::new(name, events)
    }
    fn key(k: &str, down: bool) -> Ev {
        Ev::Key(k.into(), down)
    }
    fn msgs(d: &Draft, all: &[Draft]) -> Vec<String> {
        issues(d, all).into_iter().map(|i| i.msg).collect()
    }

    #[test]
    fn a_tap_pair_is_valid_and_a_lone_down_names_the_row() {
        let ok = mac("a", vec![key("KEY_A", true), Ev::Delay(20), key("a", false)]);
        assert_eq!(issues(&ok, std::slice::from_ref(&ok)), vec![], "aliases and delays are fine");
        let held = mac("a", vec![key("KEY_LEFTSHIFT", true), key("KEY_END", true), Ev::Delay(20), key("KEY_END", false)]);
        let i = issues(&held, std::slice::from_ref(&held));
        assert_eq!(i.len(), 1);
        assert_eq!(i[0].ev, Some(0), "the earliest press is the row that gets the ×");
        assert_eq!(i[0].msg, "KEY_LEFTSHIFT is pressed at #01 and never released, so it would stay held after the macro ends.");
        let up = mac("a", vec![Ev::Button(KeyCode::BTN_LEFT, false)]);
        assert_eq!(msgs(&up, std::slice::from_ref(&up)), ["BTN_LEFT is released at #01 but was never pressed."]);
        let twice = mac("a", vec![key("KEY_A", true), key("KEY_A", true), key("KEY_A", false)]);
        assert_eq!(issues(&twice, std::slice::from_ref(&twice))[0].ev, Some(1));
    }

    #[test]
    fn empty_macro_bad_names_duplicates_and_bad_values_are_reported() {
        let e = mac("untitled-1", vec![]);
        assert_eq!(msgs(&e, std::slice::from_ref(&e)), ["No events. A macro needs at least one event."]);
        let bad = mac("has space", vec![Ev::Delay(1)]);
        assert!(msgs(&bad, std::slice::from_ref(&bad))[0].starts_with("The name must be 1 to 64 letters, digits, - or _."));
        let (a, b) = (mac("x", vec![Ev::Delay(1)]), mac("x", vec![Ev::Delay(2)]));
        assert_eq!(msgs(&a, &[a.clone(), b]), ["Another macro is already called 'x'."]);
        let key_text = mac("a", vec![key("nonsense_key", true), key("", false), key("left", true)]);
        let m = msgs(&key_text, std::slice::from_ref(&key_text));
        assert!(m[0].starts_with("#01: unknown key/button 'nonsense_key'"), "{m:?}");
        assert!(m[1].starts_with("#02: Type a key name"), "{m:?}");
        assert!(m[2].contains("is a mouse button"), "{m:?}");
        let range = mac("a", vec![Ev::Wheel(0), Ev::Move(0, 0), Ev::Delay(60_001), Ev::HWheel(101)]);
        assert_eq!(issues(&range, std::slice::from_ref(&range)).iter().map(|i| i.ev).collect::<Vec<_>>(), [Some(0), Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn whatever_has_no_issues_is_accepted_by_the_file_writer() {
        let d = mac("tap-ctrl-c", vec![key("ctrl", true), key("c", true), key("c", false), key("ctrl", false), Ev::Wheel(-3), Ev::HWheel(2), Ev::Move(-5, 5)]);
        assert_eq!(issues(&d, std::slice::from_ref(&d)), vec![]);
        let m = d.to_macro().unwrap();
        assert_eq!(m.events[0], MacroEvent::Key(KeyCode::KEY_LEFTCTRL, true));
        assert!(macros::validate(&m).is_ok());
        // the round trip through the model keeps every row
        assert_eq!(Draft::from_macro(&m).events.len(), d.events.len());
    }

    #[test]
    fn run_time_is_the_engines_own_number() {
        // 5 events and 30 ms of delay: no 3 ms pad inside, a 20 ms tail
        let d = mac("a", vec![key("KEY_A", true), Ev::Delay(30), key("KEY_A", false), Ev::Wheel(1), Ev::Wheel(1)]);
        assert_eq!(d.run_ms(), 50);
        let six = mac("a", vec![Ev::Wheel(1); 6]);
        assert_eq!(six.run_ms(), 23);
    }

    const CONF: &str = "# rules\nback = macro:wiggle,toggle   # side\nforward = Macro: rapid-click ,held\nmiddle = ctrl+c\nright = macro:wiggle\n";

    #[test]
    fn rule_scan_finds_the_macro_rules_and_labels_them() {
        let r = rule_refs(CONF);
        assert_eq!(r.iter().map(|r| (r.line, r.name.as_str())).collect::<Vec<_>>(), [(2, "wiggle"), (3, "rapid-click"), (5, "wiggle")]);
        assert_eq!(r[0].mode, Playback::Toggle);
        let d = Draft { orig: Some("wiggle".into()), ..mac("wiggle", vec![]) };
        assert_eq!(used_by(&d, &r), ["BACK · TOGGLE", "RIGHT · ONCE"]);
        assert_eq!(button_label("forward"), "FORWARD");
        assert_eq!(button_label("BTN_SIDE"), "BACK");
        assert_eq!(button_label("f13"), "F13");
        assert_eq!(mode_label(Playback::Times(5)), "5 TIMES");
    }

    #[test]
    fn rename_and_remove_rewrite_only_the_macro_name() {
        let renamed = fix_text(CONF, &[RuleFix::Rename { from: "wiggle".into(), to: "shake".into() }]);
        assert_eq!(
            renamed,
            "# rules\nback = macro:shake,toggle   # side\nforward = Macro: rapid-click ,held\nmiddle = ctrl+c\nright = macro:shake\n"
        );
        let removed = fix_text(CONF, &[RuleFix::Remove("wiggle".into())]);
        assert_eq!(removed, "# rules\nforward = Macro: rapid-click ,held\nmiddle = ctrl+c\n");
        // two macros swapping names must not cascade
        let swap = fix_text("a = macro:x\nb = macro:y", &[
            RuleFix::Rename { from: "x".into(), to: "y".into() },
            RuleFix::Rename { from: "y".into(), to: "x".into() },
        ]);
        assert_eq!(swap, "a = macro:y\nb = macro:x");
        // a comment that mentions a macro is left alone
        assert_eq!(fix_text("# back = macro:wiggle\n", &[RuleFix::Remove("wiggle".into())]), "# back = macro:wiggle\n");
        assert_eq!(remove_line(CONF, 2).lines().count(), 4);
        assert_eq!(retarget_line("a = macro:x,held\nb = macro:x\n", 1, "z"), "a = macro:z,held\nb = macro:x\n");
    }

    fn ui_with(names: &[&str]) -> MacroUi {
        let mut m = MacroUi::empty();
        for n in names {
            let mut d = mac(n, vec![Ev::Button(KeyCode::BTN_LEFT, true), Ev::Button(KeyCode::BTN_LEFT, false)]);
            d.orig = Some(n.to_string());
            m.drafts.push(d);
        }
        m.saved = m.drafts.clone();
        m
    }

    #[test]
    fn dangling_ignores_rules_a_pending_rename_or_remove_will_fix() {
        let conf = "back = macro:wiggle\nforward = macro:gone\n";
        let mut m = ui_with(&["wiggle"]);
        assert_eq!(m.dangling(conf), [(2, "gone".to_string())]);
        m.drafts[0].name = "shake".into();
        assert_eq!(m.dangling(conf), [(2, "gone".to_string())], "rule for the old name follows the rename");
        m.drafts.clear();
        assert_eq!(m.dangling(conf).len(), 2, "deleted without removing the rule: both dangle");
        m.remove_rules.insert("wiggle".into());
        assert_eq!(m.dangling(conf), [(2, "gone".to_string())]);
    }

    #[test]
    fn revert_new_duplicate_and_delete() {
        let mut m = ui_with(&["a", "b"]);
        assert!(!m.dirty() && !m.can_save());
        m.new_macro();
        assert_eq!(m.drafts.last().unwrap().name, "untitled-1");
        m.new_macro();
        assert_eq!(m.drafts.last().unwrap().name, "untitled-2");
        assert!(m.dirty());
        assert_eq!(m.invalid_count(), 2, "new macros have no events yet");
        assert!(!m.can_save(), "Save stays off while any macro is invalid");
        m.revert();
        assert_eq!(m.drafts.len(), 2);
        assert!(!m.dirty());
        m.sel = 0;
        m.duplicate();
        assert_eq!((m.drafts.last().unwrap().name.as_str(), m.drafts.last().unwrap().orig.clone()), ("a-copy", None));
        m.delete(true);
        assert_eq!(m.drafts.len(), 2);
        assert!(m.remove_rules.is_empty() || m.remove_rules.contains("a-copy"));
        m.sel = 0;
        m.delete(true);
        assert!(m.remove_rules.contains("a"));
        assert!(m.fixes().contains(&RuleFix::Remove("a".into())));
        m.revert();
        assert!(m.remove_rules.is_empty() && m.drafts.len() == 2);
    }

    #[test]
    fn inserted_rows_go_after_the_selected_one() {
        let mut m = ui_with(&["a"]);
        m.sel_ev = 0;
        m.insert_events(vec![Ev::Delay(50)]);
        assert_eq!(m.drafts[0].events[1], Ev::Delay(50));
        assert_eq!(m.sel_ev, 1);
        m.insert_events(vec![key("KEY_A", true), key("KEY_A", false)]);
        assert_eq!((m.drafts[0].events.len(), m.sel_ev), (5, 3));
    }

    #[test]
    fn add_a_rule_builds_the_line_and_refuses_what_it_cannot_say() {
        let macros = vec![mac("copy", vec![Ev::Delay(1)])];
        let mut b = Bind { macro_name: "copy".into(), ..Bind::default() };
        assert_eq!(b.line(&macros).unwrap(), "forward = macro:copy,once");
        b.mode = 2;
        assert_eq!(b.line(&macros).unwrap(), "forward = macro:copy,held");
        b.mode = 1;
        b.times = 7;
        assert_eq!(b.line(&macros).unwrap(), "forward = macro:copy,x7");
        b.mode = MODE_SEQUENCE;
        assert_eq!(b.line(&macros).unwrap_err(), "Sequence is not available yet.");
        // design flag 12: no looping left click
        b.button = 0;
        for mode in [2, 3] {
            b.mode = mode;
            assert!(b.line(&macros).unwrap_err().starts_with("Left can't repeat while held or toggled"));
        }
        b.mode = 0;
        assert_eq!(b.line(&macros).unwrap(), "left = macro:copy,once");
        b.mode = 1;
        assert!(b.line(&macros).is_ok(), "a counted run ends by itself");
        assert_eq!(b.line(&[]).unwrap_err(), "Make a macro first.");
        // the other actions
        b.does = 1;
        assert_eq!(b.line(&[]).unwrap(), "left = disable");
        b.does = 3;
        assert_eq!(b.line(&[]).unwrap(), "left = scroll_down");
        b.does = 0;
        assert!(b.line(&[]).unwrap_err().starts_with("Type the key"));
        b.key = " ctrl+c ".into();
        assert_eq!(b.line(&[]).unwrap(), "left = ctrl+c");
        b.key = "nonsense_key".into();
        assert!(b.line(&[]).is_err());
        // every line it can build parses back to the action it names
        let ok = Bind { macro_name: "copy".into(), mode: 3, ..Bind::default() }.line(&macros).unwrap();
        let m = remap::parse_config(&ok).unwrap();
        assert_eq!(m[&KeyCode::BTN_EXTRA], Action::Macro { name: "copy".into(), mode: Playback::Toggle });
    }

    #[test]
    fn starters_are_valid_macros() {
        for n in ["rapid-click", "copy-paste", "double-tap"] {
            let d = starter(n).unwrap();
            assert_eq!(issues(&d, std::slice::from_ref(&d)), vec![], "{n}");
        }
        assert!(starter("nope").is_none());
    }

    #[test]
    fn free_names_count_up() {
        let m = ui_with(&["a", "a-2"]);
        assert_eq!(m.free_name("a"), "a-3");
        assert_eq!(m.free_name("b"), "b");
    }

    // ---- the editor's clicks, applied to a whole App

    #[test]
    fn row_controls_reorder_remove_and_insert_after_the_selection() {
        let mut app = crate::tests::macro_app(Sub::Macros);
        let first = app.mac.drafts[0].events.clone(); // copy: down ctrl, delay, down c, up c, up ctrl
        app.apply(Act::Down(0));
        assert_eq!(app.mac.drafts[0].events[1], first[0]);
        assert_eq!(app.mac.sel_ev, 1, "the selection follows the row");
        app.apply(Act::Up(1));
        assert_eq!(app.mac.drafts[0].events, first);
        app.apply(Act::Up(0)); // already first: nothing
        app.apply(Act::Down(4)); // already last: nothing
        assert_eq!(app.mac.drafts[0].events, first);
        app.apply(Act::Remove(1));
        assert_eq!(app.mac.drafts[0].events.len(), 4);
        app.mac.sel_ev = 0;
        app.apply(Act::Add(vec![Ev::Delay(50)]));
        assert_eq!(app.mac.drafts[0].events[1], Ev::Delay(50));
        assert!(app.mac.dirty());
        app.apply(Act::Revert);
        assert_eq!(app.mac.drafts[0].events, first);
        assert!(!app.mac.dirty() && !app.status_is_error);
    }

    #[test]
    fn delete_buttons_and_bind_to_a_button() {
        let mut app = crate::tests::macro_app(Sub::Macros);
        app.request_delete();
        assert!(app.mac.confirm_delete);
        app.apply(Act::CancelDelete);
        assert!(!app.mac.confirm_delete && app.mac.drafts.len() == 2);
        app.mac.confirm_delete = true;
        app.apply(Act::DeleteMacro { with_rules: false });
        assert_eq!(app.mac.drafts.len(), 1);
        assert!(app.status_is_error, "the orphaned rule is called out");
        assert_eq!(app.mac.dangling(&app.conf_text), [(2, "copy".to_string())]);
        app.apply(Act::Revert);
        assert_eq!(app.mac.drafts.len(), 2);
        app.apply(Act::Duplicate);
        assert_eq!((app.mac.drafts.len(), app.mac.drafts[app.mac.sel].name.as_str()), (3, "copy-copy"));
        app.mac.sel = 0;
        app.apply(Act::BindTo);
        assert_eq!((app.mac.sub, app.mac.bind.does, app.mac.bind.macro_name.as_str()), (Sub::Rules, DOES_MACRO, "copy"));
    }
}
