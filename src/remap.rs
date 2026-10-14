//! Button remapping — the Linux equivalent of Synapse's DriverKit extension.
//!
//! The macOS `com.razer.appengine.driver.dext` (RazerGenericHIDDriverKit, decompiled
//! with REA/Ghidra) sits on the mouse's IOHIDInterface, and in `handleReport` diverts
//! button state to `HandleMouseButton`, which checks the hook list installed by
//! `SetHook` (an array of button IDs) and reports matching buttons to user space
//! instead of passing them through. The mapping engine then injects the replacement
//! action.
//!
//! Here the same thing is done in user space: EVIOCGRAB the mouse's evdev nodes so
//! the desktop no longer sees them, forward everything to a uinput clone, and
//! rewrite the buttons that have a mapping.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use evdev::raw_stream::RawDevice;
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Press these keys in order, release in reverse (e.g. ctrl+c).
    Keys(Vec<KeyCode>),
    /// Swallow the button.
    Disable,
    /// Scroll wheel ticks (vertical, horizontal); fired on press.
    Scroll(i32, i32),
}

pub type Mapping = HashMap<KeyCode, Action>;

pub fn parse_key(s: &str) -> Result<KeyCode, String> {
    let s = s.trim();
    let up = s.to_ascii_uppercase();
    // friendly aliases
    let alias = match up.as_str() {
        "LEFT" | "MOUSE1" => "BTN_LEFT",
        "RIGHT" | "MOUSE2" => "BTN_RIGHT",
        "MIDDLE" | "MOUSE3" => "BTN_MIDDLE",
        "BACK" | "SIDE" | "MOUSE4" => "BTN_SIDE",
        "FORWARD" | "EXTRA" | "MOUSE5" => "BTN_EXTRA",
        "CTRL" => "KEY_LEFTCTRL",
        "SHIFT" => "KEY_LEFTSHIFT",
        "ALT" => "KEY_LEFTALT",
        "SUPER" | "META" | "WIN" => "KEY_LEFTMETA",
        _ => "",
    };
    let name = if !alias.is_empty() {
        alias.to_string()
    } else if up.starts_with("KEY_") || up.starts_with("BTN_") {
        up
    } else {
        format!("KEY_{up}")
    };
    KeyCode::from_str(&name).map_err(|_| format!("unknown key/button '{s}'"))
}

pub fn parse_action(s: &str) -> Result<Action, String> {
    let t = s.trim();
    match t.to_ascii_lowercase().as_str() {
        "disable" | "none" | "off" => return Ok(Action::Disable),
        "scroll_up" | "wheel_up" => return Ok(Action::Scroll(1, 0)),
        "scroll_down" | "wheel_down" => return Ok(Action::Scroll(-1, 0)),
        "scroll_left" => return Ok(Action::Scroll(0, -1)),
        "scroll_right" => return Ok(Action::Scroll(0, 1)),
        _ => {}
    }
    let keys = t.split('+').map(parse_key).collect::<Result<Vec<_>, _>>()?;
    if keys.is_empty() {
        return Err("empty action".into());
    }
    Ok(Action::Keys(keys))
}

/// Config format, one rule per line:   SOURCE = ACTION     (# comments allowed)
///   back    = ctrl+c
///   forward = KEY_F13
///   middle  = disable
pub fn parse_config(text: &str) -> Result<Mapping, String> {
    let mut m = Mapping::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap().trim();
        if line.is_empty() {
            continue;
        }
        let (from, to) = line
            .split_once('=')
            .ok_or_else(|| format!("line {}: expected SOURCE = ACTION", n + 1))?;
        let from = parse_key(from).map_err(|e| format!("line {}: {e}", n + 1))?;
        let to = parse_action(to).map_err(|e| format!("line {}: {e}", n + 1))?;
        m.insert(from, to);
    }
    Ok(m)
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("razerctl/remap.conf")
}

/// Rewrite one key event per the mapping. Returns the events to emit.
pub fn translate(map: &Mapping, ev: &InputEvent) -> Vec<InputEvent> {
    if ev.event_type() != EventType::KEY {
        return vec![*ev];
    }
    let Some(action) = map.get(&KeyCode::new(ev.code())) else {
        return vec![*ev];
    };
    let v = ev.value(); // 1 press, 0 release, 2 autorepeat
    match action {
        Action::Disable => vec![],
        Action::Scroll(y, x) => {
            if v != 1 {
                return vec![];
            }
            let mut out = Vec::new();
            if *y != 0 {
                out.push(InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_WHEEL.0, *y));
                out.push(InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_WHEEL_HI_RES.0, *y * 120));
            }
            if *x != 0 {
                out.push(InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_HWHEEL.0, *x));
                out.push(InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_HWHEEL_HI_RES.0, *x * 120));
            }
            out
        }
        Action::Keys(keys) => {
            let k = |code: &KeyCode, val| InputEvent::new(EventType::KEY.0, code.0, val);
            match v {
                1 => keys.iter().map(|c| k(c, 1)).collect(),
                0 => keys.iter().rev().map(|c| k(c, 0)).collect(),
                _ if keys.len() == 1 => vec![k(&keys[0], 2)],
                _ => vec![],
            }
        }
    }
}

pub struct Source {
    pub path: PathBuf,
    pub name: String,
}

/// Find evdev nodes belonging to Razer devices (vendor 0x1532), optionally
/// filtered by a name substring.
pub fn find_sources(filter: Option<&str>) -> io::Result<Vec<Source>> {
    let mut out = Vec::new();
    for e in fs::read_dir("/dev/input")?.flatten() {
        let p = e.path();
        if !p.file_name().map_or(false, |n| n.to_string_lossy().starts_with("event")) {
            continue;
        }
        let Ok(d) = RawDevice::open(&p) else { continue };
        let name = d.name().unwrap_or("").to_string();
        if name.starts_with("razerctl") {
            continue; // our own virtual device
        }
        let is_razer = d.input_id().vendor() == crate::hidraw::RAZER_VID;
        let ok = match filter {
            Some(f) => name.to_lowercase().contains(&f.to_lowercase()),
            None => is_razer,
        };
        // only nodes that can produce buttons/keys are interesting
        if ok && d.supported_keys().map_or(false, |k| k.iter().next().is_some()) {
            out.push(Source { path: p, name });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Print button events as they happen (no grab) so the user can learn names.
pub fn watch(sources: &[Source]) -> io::Result<()> {
    let handles: Vec<_> = sources
        .iter()
        .map(|s| {
            let path = s.path.clone();
            std::thread::spawn(move || -> io::Result<()> {
                let mut d = RawDevice::open(&path)?;
                loop {
                    for ev in d.fetch_events()? {
                        if ev.event_type() == EventType::KEY && ev.value() == 1 {
                            println!("{:<12} {:?}", path.display(), KeyCode::new(ev.code()));
                        }
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap()?;
    }
    Ok(())
}

pub fn run(sources: Vec<Source>, map: Mapping) -> io::Result<()> {
    // union of capabilities across all grabbed nodes + every key we may emit
    let mut keys = AttributeSet::<KeyCode>::new();
    let mut rels = AttributeSet::<RelativeAxisCode>::new();
    let mut devs = Vec::new();
    for s in &sources {
        let mut d = RawDevice::open(&s.path)?;
        if let Some(k) = d.supported_keys() {
            k.iter().for_each(|c| keys.insert(c));
        }
        if let Some(r) = d.supported_relative_axes() {
            r.iter().for_each(|c| rels.insert(c));
        }
        d.grab().map_err(|e| io::Error::new(e.kind(), format!("grab {}: {e}", s.path.display())))?;
        eprintln!("grabbed {} ({})", s.path.display(), s.name);
        devs.push(d);
    }
    for a in map.values() {
        match a {
            Action::Keys(ks) => ks.iter().for_each(|k| keys.insert(*k)),
            Action::Scroll(..) => {
                for r in [RelativeAxisCode::REL_WHEEL, RelativeAxisCode::REL_WHEEL_HI_RES,
                          RelativeAxisCode::REL_HWHEEL, RelativeAxisCode::REL_HWHEEL_HI_RES] {
                    rels.insert(r);
                }
            }
            Action::Disable => {}
        }
    }
    let virt = VirtualDevice::builder()?
        .name("razerctl remapped mouse")
        .with_keys(&keys)?
        .with_relative_axes(&rels)?
        .build()?;
    let virt = Arc::new(Mutex::new(virt));
    let map = Arc::new(map);
    eprintln!("remapping {} rule(s); Ctrl+C to stop", map.len());

    let handles: Vec<_> = devs
        .into_iter()
        .map(|mut d| {
            let virt = virt.clone();
            let map = map.clone();
            std::thread::spawn(move || -> io::Result<()> {
                let mut batch: Vec<InputEvent> = Vec::new();
                loop {
                    for ev in d.fetch_events()? {
                        match ev.event_type() {
                            EventType::SYNCHRONIZATION => {
                                if !batch.is_empty() {
                                    virt.lock().unwrap().emit(&batch)?;
                                    batch.clear();
                                }
                            }
                            EventType::MISC => {} // MSC_SCAN etc. — not forwarded
                            _ => batch.extend(translate(&map, &ev)),
                        }
                    }
                }
            })
        })
        .collect();
    for h in handles {
        // a device disappearing (unplug / sleep) ends the daemon; systemd restarts it
        h.join().unwrap()?;
    }
    Ok(())
}

pub fn load_mapping(path: &Path, inline: &[String]) -> Result<Mapping, String> {
    let mut map = if inline.is_empty() || path.exists() {
        match fs::read_to_string(path) {
            Ok(t) => parse_config(&t).map_err(|e| format!("{}: {e}", path.display()))?,
            Err(_) if !inline.is_empty() => Mapping::new(),
            Err(e) => return Err(format!("{}: {e} (create it or pass --map FROM=TO)", path.display())),
        }
    } else {
        Mapping::new()
    };
    for rule in inline {
        map.extend(parse_config(rule)?);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, v: i32) -> InputEvent {
        InputEvent::new(EventType::KEY.0, code.0, v)
    }

    #[test]
    fn parses_config_with_aliases() {
        let m = parse_config("# c\nback = ctrl+c\nforward=KEY_F13\n middle = disable \nBTN_RIGHT = scroll_up\n").unwrap();
        assert_eq!(m[&KeyCode::BTN_SIDE], Action::Keys(vec![KeyCode::KEY_LEFTCTRL, KeyCode::KEY_C]));
        assert_eq!(m[&KeyCode::BTN_EXTRA], Action::Keys(vec![KeyCode::KEY_F13]));
        assert_eq!(m[&KeyCode::BTN_MIDDLE], Action::Disable);
        assert_eq!(m[&KeyCode::BTN_RIGHT], Action::Scroll(1, 0));
        assert!(parse_config("back = nonsense_key").is_err());
        assert!(parse_config("back ctrl").is_err());
    }

    #[test]
    fn combo_press_and_reverse_release() {
        let m = parse_config("back = ctrl+shift+t").unwrap();
        let down = translate(&m, &key(KeyCode::BTN_SIDE, 1));
        let codes: Vec<_> = down.iter().map(|e| (e.code(), e.value())).collect();
        assert_eq!(codes, vec![(KeyCode::KEY_LEFTCTRL.0, 1), (KeyCode::KEY_LEFTSHIFT.0, 1), (KeyCode::KEY_T.0, 1)]);
        let up: Vec<_> = translate(&m, &key(KeyCode::BTN_SIDE, 0)).iter().map(|e| (e.code(), e.value())).collect();
        assert_eq!(up, vec![(KeyCode::KEY_T.0, 0), (KeyCode::KEY_LEFTSHIFT.0, 0), (KeyCode::KEY_LEFTCTRL.0, 0)]);
    }

    #[test]
    fn passthrough_and_disable() {
        let m = parse_config("middle = disable").unwrap();
        assert!(translate(&m, &key(KeyCode::BTN_MIDDLE, 1)).is_empty());
        let l = key(KeyCode::BTN_LEFT, 1);
        assert_eq!(translate(&m, &l)[0].code(), KeyCode::BTN_LEFT.0);
        let rel = InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_X.0, 5);
        assert_eq!(translate(&m, &rel)[0].value(), 5);
    }
}
