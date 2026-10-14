//! Button macros, played back on the host.
//!
//! Synapse configures this mouse for host macros only (docs/macro-protocol.md §1, §3):
//! nothing is written to the device. The button is diverted by the evdev grab in
//! `remap.rs` and the macro is injected through its uinput device. This module is the
//! data side: the macro model, the `macros.conf` text format, Synapse's flood-guard
//! padding and the exact evdev encoding of each event. No I/O beyond the text file.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use evdev::{EventType, InputEvent, KeyCode, RelativeAxisCode};

use crate::remap::parse_key;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MacroEvent {
    /// KEY_* code, true = down, false = up
    Key(KeyCode, bool),
    /// BTN_* code (mouse buttons), true = down
    Button(KeyCode, bool),
    /// vertical notches, +1 = up (away from user)
    Wheel(i32),
    /// horizontal notches, +1 = right
    HWheel(i32),
    /// relative cursor move dx, dy
    Move(i32, i32),
    /// milliseconds
    Delay(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Macro {
    pub name: String,
    /// Synapse guid, preserved verbatim for a future import; never generated.
    pub guid: Option<String>,
    pub events: Vec<MacroEvent>,
}

/// Keyed by name.
pub type Macros = BTreeMap<String, Macro>;

/// One SYN-terminated batch to emit, or a pause.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Emit(Vec<InputEvent>),
    Sleep(Duration),
}

pub const MAX_NAME: usize = 64;
pub const MAX_EVENTS: usize = 1000;
pub const MAX_DELAY_MS: u32 = 60_000;
pub const MAX_WHEEL: u32 = 100;
pub const MAX_MOVE: u32 = 10_000;

/// Synapse's `{each:5, delay:3, endDelay:20}` (doc §3).
const PAD_EACH: usize = 5;
const PAD_MS: u32 = 3;
const PAD_END_MS: u32 = 20;

/// Wheel delta per notch in the hi-res axes (and in Synapse's host model).
const HI_RES_NOTCH: i32 = 120;

pub fn macros_path() -> PathBuf {
    crate::remap::config_path().with_file_name("macros.conf")
}

fn is_button(k: KeyCode) -> bool {
    format!("{k:?}").starts_with("BTN_")
}

/// down/up/tap target: BTN_* codes are mouse buttons, everything else a key.
fn press(k: KeyCode, down: bool) -> MacroEvent {
    if is_button(k) {
        MacroEvent::Button(k, down)
    } else {
        MacroEvent::Key(k, down)
    }
}

/// The file format has no quoting, so a guid must be one bare word without '#'.
fn check_guid(g: &str) -> Result<(), String> {
    if g.is_empty()
        || g.chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '#')
    {
        return Err(format!("bad guid '{g}' (one word, no '#')"));
    }
    Ok(())
}

pub fn validate(m: &Macro) -> Result<(), String> {
    check(m).map_err(|(_, e)| e)
}

/// `validate`, plus the index of the offending event (None = the macro as a whole)
/// so `parse_macros` can point at a line.
fn check(m: &Macro) -> Result<(), (Option<usize>, String)> {
    let name = &m.name;
    let ok_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    if name.is_empty() || name.len() > MAX_NAME || !name.chars().all(ok_char) {
        return Err((
            None,
            format!("bad macro name '{name}' (1 to {MAX_NAME} of A-Z a-z 0-9 _ -)"),
        ));
    }
    if let Some(g) = &m.guid {
        check_guid(g).map_err(|e| (None, format!("macro '{name}': {e}")))?;
    }
    if m.events.is_empty() {
        return Err((None, format!("macro '{name}' is empty")));
    }
    if m.events.len() > MAX_EVENTS {
        let e = format!(
            "macro '{name}' has {} events (max {MAX_EVENTS})",
            m.events.len()
        );
        return Err((Some(MAX_EVENTS), e));
    }
    // code -> index of the down that pressed it
    let mut held: BTreeMap<u16, usize> = BTreeMap::new();
    for (i, ev) in m.events.iter().enumerate() {
        let bad = |e: String| Err((Some(i), format!("macro '{name}': {e}")));
        match *ev {
            MacroEvent::Key(k, down) | MacroEvent::Button(k, down) => {
                // only named codes: the serializer writes the name and must read it back
                if k.0 == 0 || parse_key(&format!("{k:?}")) != Ok(k) {
                    return bad(format!("unknown key code {}", k.0));
                }
                let btn = matches!(ev, MacroEvent::Button(..));
                if btn != is_button(k) {
                    return bad(format!(
                        "{k:?} is not a {}",
                        if btn { "mouse button" } else { "key" }
                    ));
                }
                if down {
                    if held.insert(k.0, i).is_some() {
                        return bad(format!("{k:?} is already down"));
                    }
                } else if held.remove(&k.0).is_none() {
                    return bad(format!("up {k:?} but it is not down"));
                }
            }
            MacroEvent::Wheel(n) | MacroEvent::HWheel(n) => {
                if n == 0 || n.unsigned_abs() > MAX_WHEEL {
                    return bad(format!(
                        "wheel {n} out of range (non-zero, at most {MAX_WHEEL} notches)"
                    ));
                }
            }
            MacroEvent::Move(dx, dy) => {
                if (dx == 0 && dy == 0)
                    || dx.unsigned_abs() > MAX_MOVE
                    || dy.unsigned_abs() > MAX_MOVE
                {
                    return bad(format!(
                        "move {dx} {dy} out of range (not both 0, each at most {MAX_MOVE})"
                    ));
                }
            }
            MacroEvent::Delay(ms) => {
                if ms > MAX_DELAY_MS {
                    return bad(format!("delay {ms} ms too long (max {MAX_DELAY_MS})"));
                }
            }
        }
    }
    // a stuck key on the desktop is the worst outcome: point at the earliest press
    if let Some((&code, &i)) = held.iter().min_by_key(|(_, &i)| i) {
        return Err((
            Some(i),
            format!("macro '{name}' leaves {:?} pressed", KeyCode::new(code)),
        ));
    }
    Ok(())
}

/// One event line (already split into words) -> its events (`tap`/`click` give two).
fn parse_line(w: &[&str]) -> Result<Vec<MacroEvent>, String> {
    use MacroEvent::*;
    let kw = w[0].to_ascii_lowercase();
    let args = &w[1..];
    let want = |n: usize| {
        if args.len() == n {
            Ok(())
        } else {
            Err(format!(
                "'{kw}' takes {n} argument{}",
                if n == 1 { "" } else { "s" }
            ))
        }
    };
    let int = |s: &str| s.parse::<i32>().map_err(|_| format!("bad number '{s}'"));
    Ok(match kw.as_str() {
        "down" | "up" | "tap" | "click" => {
            want(1)?;
            let k = parse_key(args[0])?;
            match kw.as_str() {
                "down" => vec![press(k, true)],
                "up" => vec![press(k, false)],
                "click" if !is_button(k) => {
                    return Err(format!("click takes a mouse button, not {k:?}"))
                }
                _ => vec![press(k, true), press(k, false)],
            }
        }
        "delay" => {
            want(1)?;
            let ms = args[0]
                .parse()
                .map_err(|_| format!("bad delay '{}' (milliseconds)", args[0]))?;
            vec![Delay(ms)]
        }
        "wheel" => {
            want(1)?;
            vec![Wheel(int(args[0])?)]
        }
        "hwheel" => {
            want(1)?;
            vec![HWheel(int(args[0])?)]
        }
        "move" => {
            want(2)?;
            vec![Move(int(args[0])?, int(args[1])?)]
        }
        _ => return Err(format!("unknown command '{}'", w[0])),
    })
}

/// A macro being parsed: the macro, its header line, the line of each event.
type Pending = (Macro, usize, Vec<usize>);

fn finish(out: &mut Macros, cur: Option<Pending>) -> Result<(), String> {
    let Some((m, header, lines)) = cur else {
        return Ok(());
    };
    check(&m).map_err(|(i, e)| format!("line {}: {e}", i.map_or(header, |i| lines[i])))?;
    out.insert(m.name.clone(), m);
    Ok(())
}

/// File format (`#` comments, keywords case-insensitive, keys as in remap.conf):
///   [copy]              macro name, [A-Za-z0-9_-]+
///   guid = {6f1c...}    optional, directly after the header, kept verbatim
///   down ctrl           also: up KEY, tap KEY (down+up), click BUTTON (down+up)
///   delay 30            ms
///   wheel 1             vertical notches (+ = up); hwheel N (+ = right)
///   move 10 -5          relative dx dy
pub fn parse_macros(text: &str) -> Result<Macros, String> {
    let mut out = Macros::new();
    let mut cur: Option<Pending> = None;
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap().trim();
        if line.is_empty() {
            continue;
        }
        let err = |e: String| format!("line {}: {e}", n + 1);
        if let Some(rest) = line.strip_prefix('[') {
            let name = rest
                .strip_suffix(']')
                .ok_or_else(|| err("expected [name]".into()))?
                .trim();
            finish(&mut out, cur.take())?;
            if out.contains_key(name) {
                return Err(err(format!("duplicate macro name '{name}'")));
            }
            cur = Some((
                Macro {
                    name: name.into(),
                    guid: None,
                    events: vec![],
                },
                n + 1,
                vec![],
            ));
            continue;
        }
        let Some((m, _, lines)) = cur.as_mut() else {
            return Err(err("expected a [name] header before events".into()));
        };
        let guid = line
            .split_once('=')
            .filter(|(k, _)| k.trim().eq_ignore_ascii_case("guid"));
        if let Some((_, g)) = guid {
            if m.guid.is_some() || !m.events.is_empty() {
                return Err(err("guid must directly follow the [name] header".into()));
            }
            check_guid(g.trim()).map_err(err)?;
            m.guid = Some(g.trim().into());
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let evs = parse_line(&words).map_err(err)?;
        lines.extend(evs.iter().map(|_| n + 1));
        m.events.extend(evs);
    }
    finish(&mut out, cur)?;
    Ok(out)
}

/// Canonical text: macros by name, `tap`/`click` written as down/up, evdev key names.
pub fn serialize_macros(m: &Macros) -> String {
    let mut s = String::from("# razerctl macros (rewritten on save; comments are not kept)\n");
    for mac in m.values() {
        let _ = writeln!(s, "\n[{}]", mac.name);
        if let Some(g) = &mac.guid {
            let _ = writeln!(s, "guid = {g}");
        }
        for ev in &mac.events {
            let _ = match ev {
                MacroEvent::Key(k, d) | MacroEvent::Button(k, d) => {
                    writeln!(s, "{} {k:?}", if *d { "down" } else { "up" })
                }
                MacroEvent::Wheel(n) => writeln!(s, "wheel {n}"),
                MacroEvent::HWheel(n) => writeln!(s, "hwheel {n}"),
                MacroEvent::Move(dx, dy) => writeln!(s, "move {dx} {dy}"),
                MacroEvent::Delay(ms) => writeln!(s, "delay {ms}"),
            };
        }
    }
    s
}

pub fn load_macros(path: &Path) -> Result<Macros, String> {
    match fs::read_to_string(path) {
        Ok(t) => parse_macros(&t).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Macros::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Validate everything, then write a temp file next to `path` and rename it over,
/// so a crash never leaves a half-written file.
pub fn save_macros(path: &Path, m: &Macros) -> Result<(), String> {
    let fail = |e: String| format!("{}: {e}", path.display());
    for (key, mac) in m {
        if *key != mac.name {
            return Err(fail(format!(
                "macro '{}' is stored under name '{key}'",
                mac.name
            )));
        }
        validate(mac).map_err(fail)?;
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(|e| fail(e.to_string()))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let write = || -> io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(serialize_macros(m).as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    };
    write().map_err(|e| {
        let _ = fs::remove_file(&tmp);
        fail(e.to_string())
    })
}

/// Synapse's flood guard: a 3 ms pause after every 5th event and a 20 ms tail.
/// razerctl's reading of the boundary (the doc doesn't settle it): all events count,
/// user delays included; no 3 ms pad after the last event, since the 20 ms tail
/// follows it anyway.
pub fn pad(events: &[MacroEvent]) -> Vec<MacroEvent> {
    let mut out = Vec::with_capacity(events.len() + events.len() / PAD_EACH + 1);
    for (i, ev) in events.iter().enumerate() {
        out.push(ev.clone());
        if (i + 1).is_multiple_of(PAD_EACH) && i + 1 < events.len() {
            out.push(MacroEvent::Delay(PAD_MS));
        }
    }
    out.push(MacroEvent::Delay(PAD_END_MS));
    out
}

/// Exact evdev encoding, one `Emit` (one SYN report) per event so a down and its up
/// never share a frame. Consecutive delays merge into one `Sleep`. No padding here.
pub fn to_steps(events: &[MacroEvent]) -> Vec<Step> {
    use RelativeAxisCode as R;
    let rel = |c: R, v: i32| InputEvent::new(EventType::RELATIVE.0, c.0, v);
    let hi = |n: i32| n.saturating_mul(HI_RES_NOTCH);
    let mut out = Vec::new();
    for ev in events {
        let frame = match *ev {
            MacroEvent::Key(k, d) | MacroEvent::Button(k, d) => {
                vec![InputEvent::new(EventType::KEY.0, k.0, d as i32)]
            }
            MacroEvent::Wheel(n) => vec![rel(R::REL_WHEEL, n), rel(R::REL_WHEEL_HI_RES, hi(n))],
            MacroEvent::HWheel(n) => vec![rel(R::REL_HWHEEL, n), rel(R::REL_HWHEEL_HI_RES, hi(n))],
            MacroEvent::Move(dx, dy) => [(R::REL_X, dx), (R::REL_Y, dy)]
                .into_iter()
                .filter(|&(_, v)| v != 0)
                .map(|(c, v)| rel(c, v))
                .collect(),
            MacroEvent::Delay(ms) => {
                let d = Duration::from_millis(ms.into());
                match out.last_mut() {
                    Some(Step::Sleep(t)) => *t += d,
                    _ => out.push(Step::Sleep(d)),
                }
                continue;
            }
        };
        if !frame.is_empty() {
            out.push(Step::Emit(frame));
        }
    }
    out
}

/// Run time of one playback, Synapse padding included (sum of the delays of `pad`).
pub fn play_ms(events: &[MacroEvent]) -> u64 {
    pad(events)
        .iter()
        .map(|e| {
            if let MacroEvent::Delay(ms) = e {
                u64::from(*ms)
            } else {
                0
            }
        })
        .sum()
}

/// `macros list`: one line per macro, name, events as written, padded run time.
pub fn describe_macros(m: &Macros) -> Vec<String> {
    let w = m.keys().map(String::len).max().unwrap_or(0);
    m.values()
        .map(|mac| {
            let n = mac.events.len();
            format!(
                "{:<w$}  {n} event{}, {} ms",
                mac.name,
                if n == 1 { "" } else { "s" },
                play_ms(&mac.events)
            )
        })
        .collect()
}

/// `macros play --dry-run`: one line per step, `+N ms` for a sleep, else the frame's
/// events by evdev name (`KEY_A down`, `BTN_LEFT up`, `REL_WHEEL 1, REL_WHEEL_HI_RES 120`).
pub fn describe_steps(steps: &[Step]) -> Vec<String> {
    let ev = |e: &InputEvent| match e.event_type() {
        EventType::KEY => {
            let v = match e.value() {
                0 => "up".to_string(),
                1 => "down".to_string(),
                v => v.to_string(),
            };
            format!("{:?} {v}", KeyCode::new(e.code()))
        }
        EventType::RELATIVE => format!("{:?} {}", RelativeAxisCode(e.code()), e.value()),
        t => format!("{t:?} {} {}", e.code(), e.value()),
    };
    steps
        .iter()
        .map(|s| match s {
            Step::Sleep(d) => format!("+{}ms", d.as_millis()),
            Step::Emit(f) => f.iter().map(ev).collect::<Vec<_>>().join(", "),
        })
        .collect()
}

/// What a uinput device must advertise to play `m`: sorted, deduplicated.
pub fn capabilities(m: &Macro) -> (Vec<KeyCode>, Vec<RelativeAxisCode>) {
    use RelativeAxisCode as R;
    let mut keys = BTreeSet::new();
    let mut rels = BTreeSet::new();
    for ev in &m.events {
        match *ev {
            MacroEvent::Key(k, _) | MacroEvent::Button(k, _) => {
                keys.insert(k);
            }
            MacroEvent::Wheel(_) => rels.extend([R::REL_WHEEL.0, R::REL_WHEEL_HI_RES.0]),
            MacroEvent::HWheel(_) => rels.extend([R::REL_HWHEEL.0, R::REL_HWHEEL_HI_RES.0]),
            MacroEvent::Move(..) => rels.extend([R::REL_X.0, R::REL_Y.0]),
            MacroEvent::Delay(_) => {}
        }
    }
    (
        keys.into_iter().collect(),
        rels.into_iter().map(RelativeAxisCode).collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use MacroEvent::*;

    const SAMPLE: &str = "\
# sample covering every keyword
[copy]                 # header
guid = {6f1c-AB12}
down ctrl
DOWN c
delay 30
up c
Up CTRL
tap KEY_F13
click left
down back
up BTN_SIDE
wheel 1
hwheel -1
move 10 -5

[b-2]
Click Forward
WHEEL -3
TAP a
";

    fn mac(name: &str, events: Vec<MacroEvent>) -> Macro {
        Macro {
            name: name.into(),
            guid: None,
            events,
        }
    }

    fn parse_err(text: &str) -> String {
        parse_macros(text).unwrap_err()
    }

    #[test]
    fn parses_every_keyword_and_sugar() {
        let m = parse_macros(SAMPLE).unwrap();
        assert_eq!(m.keys().collect::<Vec<_>>(), ["b-2", "copy"]);
        let c = &m["copy"];
        assert_eq!(c.name, "copy");
        assert_eq!(c.guid.as_deref(), Some("{6f1c-AB12}"));
        assert_eq!(
            c.events,
            vec![
                Key(KeyCode::KEY_LEFTCTRL, true),
                Key(KeyCode::KEY_C, true),
                Delay(30),
                Key(KeyCode::KEY_C, false),
                Key(KeyCode::KEY_LEFTCTRL, false),
                Key(KeyCode::KEY_F13, true),
                Key(KeyCode::KEY_F13, false),
                Button(KeyCode::BTN_LEFT, true),
                Button(KeyCode::BTN_LEFT, false),
                Button(KeyCode::BTN_SIDE, true),
                Button(KeyCode::BTN_SIDE, false),
                Wheel(1),
                HWheel(-1),
                Move(10, -5),
            ]
        );
        let b = &m["b-2"];
        assert_eq!(b.guid, None);
        assert_eq!(
            b.events,
            vec![
                Button(KeyCode::BTN_EXTRA, true),
                Button(KeyCode::BTN_EXTRA, false),
                Wheel(-3),
                Key(KeyCode::KEY_A, true),
                Key(KeyCode::KEY_A, false),
            ]
        );
    }

    #[test]
    fn serialize_round_trips_and_is_canonical() {
        let m = parse_macros(SAMPLE).unwrap();
        let text = serialize_macros(&m);
        assert_eq!(parse_macros(&text).unwrap(), m);
        assert_eq!(serialize_macros(&parse_macros(&text).unwrap()), text);
        let one =
            parse_macros("[z]\ntap ctrl\nclick right\ndelay 5\n[a]\nmove 0 3\nhwheel 2\n").unwrap();
        assert_eq!(
            serialize_macros(&one),
            "# razerctl macros (rewritten on save; comments are not kept)\n\
             \n[a]\nmove 0 3\nhwheel 2\n\
             \n[z]\ndown KEY_LEFTCTRL\nup KEY_LEFTCTRL\ndown BTN_RIGHT\nup BTN_RIGHT\ndelay 5\n"
        );
        assert_eq!(
            serialize_macros(&Macros::new()),
            "# razerctl macros (rewritten on save; comments are not kept)\n"
        );
        assert!(parse_macros("").unwrap().is_empty());
    }

    #[test]
    fn rejects_bad_names_with_line() {
        assert!(parse_err("# c\n[bad name]\ntap a").starts_with("line 2: bad macro name"));
        assert!(parse_err("[]\ntap a").starts_with("line 1: bad macro name"));
        assert!(parse_err("[a.b]\ntap a").starts_with("line 1: bad macro name"));
        assert!(parse_err("[copy\ntap a").starts_with("line 1: expected [name]"));
        let long = "x".repeat(MAX_NAME + 1);
        assert!(parse_err(&format!("[{long}]\ntap a")).starts_with("line 1: bad macro name"));
        assert!(parse_macros(&format!("[{}]\ntap a", "x".repeat(MAX_NAME))).is_ok());
        assert!(validate(&mac("", vec![Delay(1)])).is_err());
    }

    #[test]
    fn rejects_duplicate_and_empty_macros() {
        assert_eq!(
            parse_err("[a]\ntap a\n\n[a]\ntap b"),
            "line 4: duplicate macro name 'a'"
        );
        assert_eq!(parse_err("[a]\n[b]\ntap a"), "line 1: macro 'a' is empty");
        assert_eq!(
            parse_err("[a]\ntap a\n[b]\n# nothing"),
            "line 3: macro 'b' is empty"
        );
        assert!(validate(&mac("a", vec![])).is_err());
    }

    #[test]
    fn rejects_too_many_events() {
        let ok = format!("[a]\n{}", "delay 1\n".repeat(MAX_EVENTS));
        assert_eq!(parse_macros(&ok).unwrap()["a"].events.len(), MAX_EVENTS);
        let bad = format!("[a]\n{}", "delay 1\n".repeat(MAX_EVENTS + 1));
        assert_eq!(
            parse_err(&bad),
            "line 1002: macro 'a' has 1001 events (max 1000)"
        );
        // tap counts as two events
        let taps = format!("[a]\n{}", "tap a\n".repeat(MAX_EVENTS / 2 + 1));
        assert!(parse_err(&taps).contains("has 1002 events"));
    }

    #[test]
    fn rejects_out_of_range_values() {
        assert!(parse_macros("[a]\ndelay 60000").is_ok());
        assert!(
            parse_err("[a]\ndelay 1\ndelay 60001").starts_with("line 3: macro 'a': delay 60001 ms")
        );
        assert!(parse_err("[a]\ndelay -1").starts_with("line 2: bad delay"));
        assert!(parse_err("[a]\nwheel 0").starts_with("line 2: macro 'a': wheel 0"));
        assert!(parse_err("[a]\nwheel 101").starts_with("line 2:"));
        assert!(parse_err("[a]\nhwheel -101").starts_with("line 2:"));
        assert!(parse_err("[a]\nhwheel 0").starts_with("line 2:"));
        assert!(parse_macros("[a]\nwheel -100\nhwheel 100").is_ok());
        assert!(parse_err("[a]\nmove 0 0").starts_with("line 2: macro 'a': move 0 0"));
        assert!(parse_err("[a]\nmove 10001 0").starts_with("line 2:"));
        assert!(parse_err("[a]\nmove 1 -10001").starts_with("line 2:"));
        assert!(parse_macros("[a]\nmove -10000 10000").is_ok());
        assert!(validate(&mac("a", vec![Wheel(i32::MIN)])).is_err());
        assert!(validate(&mac("a", vec![Move(i32::MIN, 0)])).is_err());
    }

    #[test]
    fn rejects_unbalanced_presses() {
        assert_eq!(
            parse_err("[x]\ndown a\ndelay 5"),
            "line 2: macro 'x' leaves KEY_A pressed"
        );
        assert_eq!(
            parse_err("[x]\ndown ctrl\ndown left\nup ctrl"),
            "line 3: macro 'x' leaves BTN_LEFT pressed"
        );
        assert_eq!(
            parse_err("[x]\ntap a\nup a"),
            "line 3: macro 'x': up KEY_A but it is not down"
        );
        assert_eq!(
            parse_err("[x]\ndown a\ndown a\nup a"),
            "line 3: macro 'x': KEY_A is already down"
        );
        assert_eq!(
            validate(&mac("x", vec![Key(KeyCode::KEY_A, true)])).unwrap_err(),
            "macro 'x' leaves KEY_A pressed"
        );
        assert!(validate(&mac("x", vec![Button(KeyCode::BTN_LEFT, false)])).is_err());
    }

    #[test]
    fn rejects_mismatched_or_unknown_codes() {
        assert!(validate(&mac(
            "x",
            vec![Key(KeyCode::BTN_LEFT, true), Key(KeyCode::BTN_LEFT, false)]
        ))
        .is_err());
        assert!(validate(&mac(
            "x",
            vec![Button(KeyCode::KEY_A, true), Button(KeyCode::KEY_A, false)]
        ))
        .is_err());
        let unknown = KeyCode::new(0x2ff);
        assert!(validate(&mac("x", vec![Key(unknown, true), Key(unknown, false)])).is_err());
        let reserved = KeyCode::KEY_RESERVED;
        assert!(validate(&mac("x", vec![Key(reserved, true), Key(reserved, false)])).is_err());
        let bad_guid = Macro {
            guid: Some("a b".into()),
            ..mac("x", vec![Delay(1)])
        };
        assert!(validate(&bad_guid).is_err());
    }

    #[test]
    fn rejects_syntax_errors_with_line() {
        assert!(parse_err("tap a").starts_with("line 1: expected a [name] header"));
        assert!(parse_err("[a]\n\njump 3").starts_with("line 3: unknown command 'jump'"));
        assert!(parse_err("[a]\ntap nonsense_key").starts_with("line 2: unknown key/button"));
        assert!(parse_err("[a]\nclick a").starts_with("line 2: click takes a mouse button"));
        assert!(parse_err("[a]\ntap").starts_with("line 2: 'tap' takes 1 argument"));
        assert!(parse_err("[a]\ndown a b").starts_with("line 2: 'down' takes 1 argument"));
        assert!(parse_err("[a]\nmove 3").starts_with("line 2: 'move' takes 2 arguments"));
        assert!(parse_err("[a]\nwheel x").starts_with("line 2: bad number 'x'"));
        assert!(
            parse_err("[a]\ntap a\nguid = {x}").starts_with("line 3: guid must directly follow")
        );
        assert!(
            parse_err("[a]\nguid = 1\nguid = 2").starts_with("line 3: guid must directly follow")
        );
        assert!(parse_err("[a]\nguid =\ntap a").starts_with("line 2: bad guid"));
        assert!(parse_err("[a]\nguid = a b\ntap a").starts_with("line 2: bad guid"));
        // guid may follow comments/blank lines, keyword is case-insensitive
        let m = parse_macros("[a]\n# c\n\nGUID={X-1}\ntap a").unwrap();
        assert_eq!(m["a"].guid.as_deref(), Some("{X-1}"));
    }

    fn numbered(n: usize) -> Vec<MacroEvent> {
        (1..=n as i32).map(Wheel).collect()
    }

    #[test]
    fn pads_like_synapse() {
        let d3 = Delay(3);
        let d20 = Delay(20);
        assert_eq!(pad(&[]), vec![d20.clone()]);
        assert_eq!(pad(&numbered(1)), vec![Wheel(1), d20.clone()]);
        let mut five = numbered(5);
        five.push(d20.clone());
        assert_eq!(pad(&numbered(5)), five);
        let six = vec![
            Wheel(1),
            Wheel(2),
            Wheel(3),
            Wheel(4),
            Wheel(5),
            d3.clone(),
            Wheel(6),
            d20.clone(),
        ];
        assert_eq!(pad(&numbered(6)), six);
        let e = numbered(11);
        let mut eleven = e[..5].to_vec();
        eleven.push(d3.clone());
        eleven.extend_from_slice(&e[5..10]);
        eleven.push(d3.clone());
        eleven.push(e[10].clone());
        eleven.push(d20.clone());
        assert_eq!(pad(&e), eleven);
        // 10 events: no 3 ms pad after the last one
        let p10 = pad(&numbered(10));
        assert_eq!(p10.len(), 12);
        assert_eq!(p10[5], d3);
        assert_eq!(p10[11], d20);
        // user delays count as events
        let p = pad(&[Delay(7), Delay(7), Delay(7), Delay(7), Delay(7), Wheel(1)]);
        assert_eq!(p[5], d3);
    }

    #[derive(Debug, PartialEq)]
    enum S {
        E(Vec<(u16, u16, i32)>),
        W(u64),
    }

    fn dump(steps: &[Step]) -> Vec<S> {
        steps
            .iter()
            .map(|s| match s {
                Step::Emit(v) => S::E(
                    v.iter()
                        .map(|e| (e.event_type().0, e.code(), e.value()))
                        .collect(),
                ),
                Step::Sleep(d) => S::W(d.as_millis() as u64),
            })
            .collect()
    }

    const KEY: u16 = EventType::KEY.0;
    const REL: u16 = EventType::RELATIVE.0;

    #[test]
    fn steps_have_exact_evdev_encoding() {
        let a = KeyCode::KEY_A.0;
        let l = KeyCode::BTN_LEFT.0;
        let steps = to_steps(&[
            Key(KeyCode::KEY_A, true),
            Key(KeyCode::KEY_A, false),
            Button(KeyCode::BTN_LEFT, true),
            Button(KeyCode::BTN_LEFT, false),
        ]);
        assert_eq!(
            dump(&steps),
            vec![
                S::E(vec![(KEY, a, 1)]),
                S::E(vec![(KEY, a, 0)]),
                S::E(vec![(KEY, l, 1)]),
                S::E(vec![(KEY, l, 0)])
            ]
        );

        use RelativeAxisCode as R;
        let w = to_steps(&[Wheel(-1), HWheel(2)]);
        assert_eq!(
            dump(&w),
            vec![
                S::E(vec![
                    (REL, R::REL_WHEEL.0, -1),
                    (REL, R::REL_WHEEL_HI_RES.0, -120)
                ]),
                S::E(vec![
                    (REL, R::REL_HWHEEL.0, 2),
                    (REL, R::REL_HWHEEL_HI_RES.0, 240)
                ]),
            ]
        );

        let m = to_steps(&[Move(7, 0), Move(0, -3), Move(1, 2)]);
        let (x, y) = (R::REL_X.0, R::REL_Y.0);
        assert_eq!(
            dump(&m),
            vec![
                S::E(vec![(REL, x, 7)]),
                S::E(vec![(REL, y, -3)]),
                S::E(vec![(REL, x, 1), (REL, y, 2)])
            ]
        );
    }

    #[test]
    fn steps_merge_consecutive_delays() {
        let a = KeyCode::KEY_A.0;
        let (down, up) = (Key(KeyCode::KEY_A, true), Key(KeyCode::KEY_A, false));
        let s = to_steps(&[
            Delay(5),
            down,
            Delay(30),
            Delay(3),
            Delay(20),
            up,
            Delay(20),
        ]);
        assert_eq!(
            dump(&s),
            vec![
                S::W(5),
                S::E(vec![(KEY, a, 1)]),
                S::W(53),
                S::E(vec![(KEY, a, 0)]),
                S::W(20)
            ]
        );
        // padded output keeps one Sleep where pad and user delays meet
        let p = to_steps(&pad(&[
            Key(KeyCode::KEY_A, true),
            Key(KeyCode::KEY_A, false),
            Delay(10),
        ]));
        assert_eq!(
            dump(&p),
            vec![S::E(vec![(KEY, a, 1)]), S::E(vec![(KEY, a, 0)]), S::W(30)]
        );
        assert!(to_steps(&[]).is_empty());
    }

    #[test]
    fn describe_steps_shows_padded_frames_and_sleeps() {
        let m = parse_macros("[m]\ntap a\ndelay 20\nclick left\nwheel 1\n").unwrap();
        // 6 events: 3 ms pad after the 5th, 20 ms tail
        assert_eq!(
            describe_steps(&to_steps(&pad(&m["m"].events))),
            [
                "KEY_A down",
                "KEY_A up",
                "+20ms",
                "BTN_LEFT down",
                "BTN_LEFT up",
                "+3ms",
                "REL_WHEEL 1, REL_WHEEL_HI_RES 120",
                "+20ms",
            ]
        );
        // the pad merges with a user delay at the 5th event; move drops a zero axis
        let m = parse_macros("[m]\ntap a\ntap b\ndelay 10\nmove 0 -4\nhwheel -1").unwrap();
        assert_eq!(
            describe_steps(&to_steps(&pad(&m["m"].events))),
            [
                "KEY_A down",
                "KEY_A up",
                "KEY_B down",
                "KEY_B up",
                "+13ms",
                "REL_Y -4",
                "REL_HWHEEL -1, REL_HWHEEL_HI_RES -120",
                "+20ms",
            ]
        );
        assert!(describe_steps(&[]).is_empty());
    }

    #[test]
    fn list_shows_event_count_and_padded_duration() {
        assert_eq!(play_ms(&[]), 20);
        assert_eq!(play_ms(&numbered(5)), 20);
        assert_eq!(play_ms(&numbered(11)), 3 + 3 + 20);
        let m = parse_macros("[w]\nwheel 1\n[long-name]\ndelay 100\ndelay 50\ntap x\n[a]\ntap a\n")
            .unwrap();
        assert_eq!(play_ms(&m["long-name"].events), 170);
        assert_eq!(
            describe_macros(&m),
            [
                "a          2 events, 20 ms",
                "long-name  4 events, 170 ms",
                "w          1 event, 20 ms"
            ]
        );
        assert!(describe_macros(&Macros::new()).is_empty());
    }

    #[test]
    fn capabilities_are_sorted_and_deduplicated() {
        let m = parse_macros(SAMPLE).unwrap();
        let (keys, rels) = capabilities(&m["copy"]);
        assert_eq!(
            keys,
            vec![
                KeyCode::KEY_LEFTCTRL,
                KeyCode::KEY_C,
                KeyCode::KEY_F13,
                KeyCode::BTN_LEFT,
                KeyCode::BTN_SIDE
            ]
        );
        let rels: Vec<u16> = rels.iter().map(|r| r.0).collect();
        let mut want = vec![
            RelativeAxisCode::REL_X.0,
            RelativeAxisCode::REL_Y.0,
            RelativeAxisCode::REL_WHEEL.0,
            RelativeAxisCode::REL_HWHEEL.0,
            RelativeAxisCode::REL_WHEEL_HI_RES.0,
            RelativeAxisCode::REL_HWHEEL_HI_RES.0,
        ];
        want.sort();
        assert_eq!(rels, want);
        let (keys, rels) = capabilities(&mac("d", vec![Delay(1), Wheel(1), Wheel(2)]));
        assert!(keys.is_empty());
        assert_eq!(
            rels,
            vec![
                RelativeAxisCode::REL_WHEEL,
                RelativeAxisCode::REL_WHEEL_HI_RES
            ]
        );
    }

    #[test]
    fn path_is_next_to_remap_conf() {
        assert!(macros_path().ends_with("razerctl/macros.conf"));
        assert_eq!(macros_path().parent(), crate::remap::config_path().parent());
    }

    /// Unique dir under the system temp dir, removed on drop (also when a test fails).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let name = format!("razerctl-macros-{tag}-{}-{nanos}", std::process::id());
            TempDir(std::env::temp_dir().join(name))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = TempDir::new("missing");
        assert_eq!(
            load_macros(&dir.0.join("nope/macros.conf")).unwrap(),
            Macros::new()
        );
        assert!(!dir.0.exists());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = TempDir::new("save");
        let path = dir.0.join("sub/macros.conf");
        let m = parse_macros(SAMPLE).unwrap();
        save_macros(&path, &m).unwrap();
        assert_eq!(load_macros(&path).unwrap(), m);
        // overwrite, and no temp file left behind
        let mut fewer = m.clone();
        fewer.remove("b-2");
        save_macros(&path, &fewer).unwrap();
        assert_eq!(load_macros(&path).unwrap(), fewer);
        let names: Vec<_> = fs::read_dir(dir.0.join("sub"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["macros.conf"]);
        // invalid input is refused before touching the file
        let mut bad = fewer.clone();
        bad.insert("other".into(), mac("copy", vec![Delay(1)]));
        assert!(save_macros(&path, &bad)
            .unwrap_err()
            .contains("stored under name 'other'"));
        bad.remove("other");
        bad.insert("x".into(), mac("x", vec![Key(KeyCode::KEY_A, true)]));
        assert!(save_macros(&path, &bad)
            .unwrap_err()
            .ends_with("macro 'x' leaves KEY_A pressed"));
        assert_eq!(load_macros(&path).unwrap(), fewer);
        // parse errors carry the path
        fs::write(&path, "[a]\nbogus").unwrap();
        let e = load_macros(&path).unwrap_err();
        assert!(
            e.starts_with(&format!("{}: line 2:", path.display())),
            "{e}"
        );
    }
}
