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
//!
//! Macros (docs/macro-protocol.md §3) ride the same path: the press of a macro button
//! is swallowed and a `Player` worker thread plays the macro through the uinput device.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use evdev::raw_stream::RawDevice;
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode};

use crate::macros::{self, Step};

/// How a macro button plays (razerctl's semantics; Synapse's native engine wasn't decompiled).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Playback {
    /// One run per press.
    Once,
    /// N runs back to back per press.
    Times(u32),
    /// Loop while the button is held.
    Held,
    /// Press starts looping, the next press stops.
    Toggle,
}

/// Upper bound of `macro:NAME,xN`.
pub const MAX_TIMES: u32 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Press these keys in order, release in reverse (e.g. ctrl+c).
    Keys(Vec<KeyCode>),
    /// Swallow the button.
    Disable,
    /// Scroll wheel ticks (vertical, horizontal); fired on press.
    Scroll(i32, i32),
    /// Play the macro of that name (macros.conf) on a worker thread.
    Macro { name: String, mode: Playback },
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
    if t.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("macro:")) {
        return parse_macro_action(&t[6..]);
    }
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

/// What follows `macro:`: `NAME[,once|,xN|,held|,toggle]`, NAME case-sensitive, mode not.
fn parse_macro_action(s: &str) -> Result<Action, String> {
    let (name, mode) = s.split_once(',').unwrap_or((s, "once"));
    let name = name.trim();
    let ok = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    if name.is_empty() || name.len() > macros::MAX_NAME || !name.chars().all(ok) {
        return Err(format!("bad macro name '{name}' (macro:NAME[,once|xN|held|toggle])"));
    }
    let m = mode.trim().to_ascii_lowercase();
    let times = m.strip_prefix('x').filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    let mode = match (m.as_str(), times.and_then(|n| n.parse::<u32>().ok())) {
        ("once", _) => Playback::Once,
        ("held", _) => Playback::Held,
        ("toggle", _) => Playback::Toggle,
        (_, Some(n @ 1..=MAX_TIMES)) => Playback::Times(n),
        _ => return Err(format!("bad macro mode '{}' (once, x1 to x{MAX_TIMES}, held, toggle)", mode.trim())),
    };
    Ok(Action::Macro { name: name.into(), mode })
}

/// Config format, one rule per line:   SOURCE = ACTION     (# comments allowed)
///   back    = ctrl+c
///   forward = KEY_F13
///   middle  = disable
///   right   = macro:copy,held   (also macro:NAME, ,once ,xN ,toggle; see macros.rs)
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
        // the grab loop hands the press/release to the `Player` (see `drive`)
        Action::Disable | Action::Macro { .. } => vec![],
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

/// Anything the engine can emit SYN-terminated frames to. VirtualDevice in production, a recorder in tests.
pub trait Sink: Send {
    fn emit(&mut self, frame: &[InputEvent]) -> io::Result<()>;
}

impl Sink for VirtualDevice {
    fn emit(&mut self, frame: &[InputEvent]) -> io::Result<()> {
        VirtualDevice::emit(self, frame) // the inherent one, appends SYN_REPORT
    }
}

/// Records every frame. Public so the CLI's --dry-run can use it.
#[derive(Debug, Default)]
pub struct Recorder {
    pub frames: Vec<Vec<InputEvent>>,
}

impl Sink for Recorder {
    fn emit(&mut self, frame: &[InputEvent]) -> io::Result<()> {
        self.frames.push(frame.to_vec());
        Ok(())
    }
}

fn emit(sink: &Mutex<dyn Sink>, frame: &[InputEvent]) -> io::Result<()> {
    sink.lock().unwrap_or_else(PoisonError::into_inner).emit(frame)
}

/// `runs` passes of `steps` (None = until stopped). Keys/buttons go into `held` before
/// their down is emitted and leave it after their up was, so `held` never misses one.
fn play_runs(
    sink: &Mutex<dyn Sink>,
    steps: &[Step],
    runs: Option<u32>,
    stop: &mpsc::Receiver<()>,
    held: &mut Vec<u16>,
) -> io::Result<()> {
    let mut n = 0;
    while runs.is_none_or(|r| n < r) {
        n += 1;
        for s in steps {
            match s {
                Step::Emit(frame) => {
                    if !matches!(stop.try_recv(), Err(mpsc::TryRecvError::Empty)) {
                        return Ok(());
                    }
                    let keys = || frame.iter().filter(|e| e.event_type() == EventType::KEY);
                    for e in keys().filter(|e| e.value() == 1) {
                        if !held.contains(&e.code()) {
                            held.push(e.code());
                        }
                    }
                    emit(sink, frame)?;
                    for e in keys().filter(|e| e.value() == 0) {
                        held.retain(|&c| c != e.code());
                    }
                }
                // interruptible: a stop (or the Player going away) ends the wait at once
                Step::Sleep(d) => {
                    if !matches!(stop.recv_timeout(*d), Err(mpsc::RecvTimeoutError::Timeout)) {
                        return Ok(());
                    }
                }
            }
        }
    }
    Ok(())
}

/// Worker body: play, then release whatever is still down (stopped early or sink error),
/// one frame per code, in reverse press order. A redundant up is dropped by the kernel.
fn play(sink: &Mutex<dyn Sink>, steps: &[Step], runs: Option<u32>, stop: &mpsc::Receiver<()>) -> io::Result<()> {
    let mut held = Vec::new();
    let r = play_runs(sink, steps, runs, stop, &mut held);
    let mut rel = Ok(());
    for code in held.into_iter().rev() {
        rel = rel.and(emit(sink, &[InputEvent::new(EventType::KEY.0, code, 0)]));
    }
    r.and(rel)
}

struct Worker {
    mode: Playback,
    stop: mpsc::Sender<()>,
    handle: JoinHandle<()>,
}

/// Plays macros on worker threads, at most one per button.
pub struct Player {
    sink: Arc<Mutex<dyn Sink>>,
    /// macro name -> `to_steps(pad(events))`
    steps: HashMap<String, Arc<Vec<Step>>>,
    running: HashMap<KeyCode, Worker>,
}

impl Player {
    pub fn new(sink: Arc<Mutex<dyn Sink>>, macros: macros::Macros) -> Player {
        let steps = macros
            .into_iter()
            .map(|(name, m)| (name, Arc::new(macros::to_steps(&macros::pad(&m.events)))))
            .collect();
        Player { sink, steps, running: HashMap::new() }
    }

    /// Physical press of `button` bound to macro `name` with `mode`.
    pub fn press(&mut self, button: KeyCode, name: &str, mode: Playback) {
        if self.is_running(button) {
            if mode == Playback::Toggle {
                self.stop(button);
            }
            return; // Once / Times / Held: no queueing, no restart
        }
        self.stop(button); // reap a finished worker
        let Some(steps) = self.steps.get(name).cloned() else {
            eprintln!("macro '{name}' is not defined; {button:?} ignored");
            return;
        };
        let runs = match mode {
            Playback::Once => Some(1),
            Playback::Times(n) => Some(n),
            Playback::Held | Playback::Toggle => None,
        };
        let (tx, rx) = mpsc::channel();
        let sink = self.sink.clone();
        let tag = format!("macro '{name}' on {button:?}");
        let worker = thread::Builder::new().name(format!("macro {name}")).spawn({
            let tag = tag.clone();
            move || {
                if let Err(e) = play(&sink, &steps, runs, &rx) {
                    eprintln!("{tag}: {e}; stopped");
                }
            }
        });
        match worker {
            Ok(handle) => {
                self.running.insert(button, Worker { mode, stop: tx, handle });
            }
            Err(e) => eprintln!("{tag}: {e}"),
        }
    }

    pub fn release(&mut self, button: KeyCode) {
        if self.running.get(&button).is_some_and(|w| w.mode == Playback::Held) {
            self.stop(button);
        }
    }

    /// Stop `button`'s worker, if any, and wait until it has released what it holds.
    fn stop(&mut self, button: KeyCode) {
        if let Some(w) = self.running.remove(&button) {
            let _ = w.stop.send(());
            let _ = w.handle.join();
        }
    }

    /// Stop every running macro and release anything they still hold. Blocks until the workers have exited.
    pub fn stop_all(&mut self) {
        let workers: Vec<Worker> = self.running.drain().map(|(_, w)| w).collect();
        for w in &workers {
            let _ = w.stop.send(()); // all at once, so they wind down in parallel
        }
        for w in workers {
            let _ = w.handle.join();
        }
    }

    pub fn is_running(&self, button: KeyCode) -> bool {
        self.running.get(&button).is_some_and(|w| !w.handle.is_finished())
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// Grab-loop side of a macro button: press starts, release stops (per mode), autorepeat
/// is ignored. Locks the player only for macro-bound key events.
fn drive(player: &Mutex<Player>, map: &Mapping, ev: &InputEvent) {
    if ev.event_type() != EventType::KEY {
        return;
    }
    let button = KeyCode::new(ev.code());
    if let Some(Action::Macro { name, mode }) = map.get(&button) {
        let mut p = player.lock().unwrap_or_else(PoisonError::into_inner);
        match ev.value() {
            1 => p.press(button, name, *mode),
            0 => p.release(button),
            _ => {}
        }
    }
}

/// SIGUSR1 means "stop every macro that is playing" (the GUI's and the tray's *Stop all macros*, sent with
/// `systemctl --user kill`). The handler only sets this flag; `watch_stop_all` acts on it.
static STOP_ALL_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_usr1(_: libc::c_int) {
    STOP_ALL_REQUESTED.store(true, Ordering::SeqCst);
}

/// Install the SIGUSR1 handler. Until it is installed the signal's default action kills the process, so this runs
/// first thing in `razerctl remap` (and again, harmlessly, in `run_with_macros`): a *Stop all macros* that lands while
/// the config is loading or the devices are being grabbed only sets the flag, and `watch_stop_all` acts on it once the
/// player exists. `SA_RESTART` keeps the evdev reads from seeing `EINTR`.
pub fn install_stop_all_handler() -> io::Result<()> {
    // SAFETY: the handler is async-signal-safe (one atomic store) and `sa` is fully initialised before the call.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_usr1 as extern "C" fn(libc::c_int) as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        if libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Start the thread that turns SIGUSR1 into `Player::stop_all`: running macros end, held keys and buttons are
/// released, and remapping carries on. A request already pending from the startup window is served immediately.
fn watch_stop_all(player: Arc<Mutex<Player>>) -> io::Result<()> {
    install_stop_all_handler()?;
    thread::Builder::new().name("stop-all".into()).spawn(move || loop {
        if STOP_ALL_REQUESTED.swap(false, Ordering::SeqCst) {
            player.lock().unwrap_or_else(PoisonError::into_inner).stop_all();
            eprintln!("stopped all macros (SIGUSR1)");
        }
        thread::sleep(std::time::Duration::from_millis(20));
    })?;
    Ok(())
}

/// Every Action::Macro in `map` names a macro present in `macros`; error names the missing one(s).
pub fn check_macros(map: &Mapping, macros: &macros::Macros) -> Result<(), String> {
    let missing: BTreeSet<&str> = map
        .values()
        .filter_map(|a| match a {
            Action::Macro { name, .. } if !macros.contains_key(name) => Some(name.as_str()),
            _ => None,
        })
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let list: Vec<String> = missing.iter().map(|n| format!("'{n}'")).collect();
    Err(format!("undefined macro{} {}", if list.len() == 1 { "" } else { "s" }, list.join(", ")))
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
    run_with_macros(sources, map, macros::Macros::new())
}

/// `run`, but with macros: check_macros first, then grab and forward, playing macro
/// buttons through the same uinput device.
pub fn run_with_macros(sources: Vec<Source>, map: Mapping, macros: macros::Macros) -> io::Result<()> {
    if let Err(e) = install_stop_all_handler() {
        eprintln!("SIGUSR1 (stop all macros) unavailable: {e}");
    }
    check_macros(&map, &macros).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
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
            Action::Macro { name, .. } => {
                let (ks, rs) = macros::capabilities(&macros[name]);
                ks.into_iter().for_each(|k| keys.insert(k));
                rs.into_iter().for_each(|r| rels.insert(r));
            }
        }
    }
    let virt = VirtualDevice::builder()?
        .name("razerctl remapped mouse")
        .with_keys(&keys)?
        .with_relative_axes(&rels)?
        .build()?;
    let virt = Arc::new(Mutex::new(virt));
    let sink: Arc<Mutex<dyn Sink>> = virt.clone();
    let player = Arc::new(Mutex::new(Player::new(sink, macros)));
    if let Err(e) = watch_stop_all(player.clone()) {
        eprintln!("SIGUSR1 (stop all macros) unavailable: {e}");
    }
    let map = Arc::new(map);
    eprintln!("remapping {} rule(s); Ctrl+C to stop", map.len());

    let handles: Vec<_> = devs
        .into_iter()
        .map(|mut d| {
            let virt = virt.clone();
            let map = map.clone();
            let player = player.clone();
            std::thread::spawn(move || -> io::Result<()> {
                let mut batch: Vec<InputEvent> = Vec::new();
                let r = (|| -> io::Result<()> {
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
                                _ => {
                                    drive(&player, &map, &ev);
                                    batch.extend(translate(&map, &ev))
                                }
                            }
                        }
                    }
                })();
                // unplug / sleep: release whatever the macros hold before the error ends the daemon
                player.lock().unwrap_or_else(PoisonError::into_inner).stop_all();
                r
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

    fn mac(name: &str, mode: Playback) -> Action {
        Action::Macro { name: name.into(), mode }
    }

    #[test]
    fn parses_macro_actions() {
        let m = parse_config(
            "back = macro:copy\nforward = MACRO:copy,once\nmiddle = macro:Big-1_x,x5\n\
             right = Macro:a , held\nleft = macro:a,TOGGLE\nBTN_TASK = macro:a,X100\nBTN_0 = macro:a,x1\n",
        )
        .unwrap();
        assert_eq!(m[&KeyCode::BTN_SIDE], mac("copy", Playback::Once));
        assert_eq!(m[&KeyCode::BTN_EXTRA], mac("copy", Playback::Once));
        assert_eq!(m[&KeyCode::BTN_MIDDLE], mac("Big-1_x", Playback::Times(5)));
        assert_eq!(m[&KeyCode::BTN_RIGHT], mac("a", Playback::Held));
        assert_eq!(m[&KeyCode::BTN_LEFT], mac("a", Playback::Toggle));
        assert_eq!(m[&KeyCode::BTN_TASK], mac("a", Playback::Times(100)));
        assert_eq!(m[&KeyCode::BTN_0], mac("a", Playback::Times(1)));
        for bad in [
            "macro:", "macro:a,x0", "macro:a,x101", "macro:a,sequence", "macro:bad name", "macro:a,",
            "macro:a,x", "macro:a,x+5", "macro:a,held,x2", "macro:a.b", "macro:,held", "macro:a,x99999999999",
        ] {
            assert!(parse_action(bad).is_err(), "{bad}");
        }
        assert!(parse_config("back = macro:a,x0").unwrap_err().starts_with("line 1: bad macro mode 'x0'"));
        assert!(parse_config("back = macro:").unwrap_err().starts_with("line 1: bad macro name"));
        // existing actions are unaffected
        assert_eq!(parse_action("ctrl+c").unwrap(), Action::Keys(vec![KeyCode::KEY_LEFTCTRL, KeyCode::KEY_C]));
        assert_eq!(parse_action("disable").unwrap(), Action::Disable);
    }

    const KEY: u16 = EventType::KEY.0;
    type Frame = Vec<(u16, u16, i32)>;

    fn dump(f: &[InputEvent]) -> Frame {
        f.iter().map(|e| (e.event_type().0, e.code(), e.value())).collect()
    }

    fn k(code: KeyCode, v: i32) -> Frame {
        vec![(KEY, code.0, v)]
    }

    fn defs() -> macros::Macros {
        macros::parse_macros(
            "[tap]\ntap a\n\
             [slow]\ntap a\ndelay 200\ntap b\n\
             [ctrl]\ndown ctrl\ndelay 1000\nup ctrl\n\
             [alt]\ndown alt\ndelay 1000\nup alt\n\
             [shift]\ndown shift\ndelay 30\ntap b\nup shift\n\
             [combo]\ndown ctrl\ntap a\nup ctrl\n",
        )
        .unwrap()
    }

    /// The frames one run of `name` emits, as the Player plays it.
    fn emits(name: &str) -> Vec<Frame> {
        let steps = macros::to_steps(&macros::pad(&defs()[name].events));
        steps.iter().filter_map(|s| if let Step::Emit(f) = s { Some(dump(f)) } else { None }).collect()
    }

    fn player() -> (Arc<Mutex<Recorder>>, Player) {
        let rec = Arc::new(Mutex::new(Recorder::default()));
        (rec.clone(), Player::new(rec, defs()))
    }

    fn frames(rec: &Mutex<Recorder>) -> Vec<Frame> {
        rec.lock().unwrap().frames.iter().map(|f| dump(f)).collect()
    }

    /// Poll until `cond`, at most 2 s.
    fn wait(what: &str, cond: impl Fn() -> bool) {
        let t = std::time::Instant::now();
        while !cond() {
            assert!(t.elapsed() < std::time::Duration::from_secs(2), "timed out waiting for {what}");
            thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Every down has a matching later up, nothing pressed twice, nothing left down.
    fn assert_no_stuck(frames: &[Frame]) {
        let mut down = BTreeSet::new();
        for &(t, c, v) in frames.iter().flatten() {
            if t == KEY {
                if v == 1 {
                    assert!(down.insert(c), "{c} pressed twice in {frames:?}");
                } else {
                    assert!(down.remove(&c), "{c} released but not down in {frames:?}");
                }
            }
        }
        assert!(down.is_empty(), "stuck {down:?} in {frames:?}");
    }

    /// After a stop: whole runs (the last one cut short), then only ups; frame count stays put.
    fn assert_stopped_cleanly(rec: &Mutex<Recorder>, name: &str) {
        let got = frames(rec);
        thread::sleep(std::time::Duration::from_millis(60));
        assert_eq!(frames(rec), got, "still playing after stop");
        let one = emits(name);
        let same = got.iter().zip(one.iter().cycle()).take_while(|(a, b)| a == b).count();
        assert!(same > one.len(), "less than one full run: {got:?}");
        assert!(got[same..].iter().flatten().all(|e| e.0 == KEY && e.2 == 0), "{got:?}");
        assert_no_stuck(&got);
    }

    #[test]
    fn check_macros_names_missing() {
        let m = parse_config("back = macro:tap\nforward = macro:nope,held\nmiddle = macro:zzz\nright = ctrl+c").unwrap();
        assert_eq!(check_macros(&m, &defs()).unwrap_err(), "undefined macros 'nope', 'zzz'");
        let one = parse_config("back = macro:tap\nforward = macro:nope").unwrap();
        assert_eq!(check_macros(&one, &defs()).unwrap_err(), "undefined macro 'nope'");
        let ok = parse_config("back = macro:tap\nforward = macro:shift,toggle\nmiddle = disable").unwrap();
        assert!(check_macros(&ok, &defs()).is_ok());
        assert!(check_macros(&Mapping::new(), &macros::Macros::new()).is_ok());
    }

    #[test]
    fn translate_swallows_macro_buttons() {
        let m = parse_config("back = macro:tap\nforward = macro:shift,held\nmiddle = ctrl+c").unwrap();
        for v in [1, 0, 2] {
            assert!(translate(&m, &key(KeyCode::BTN_SIDE, v)).is_empty());
            assert!(translate(&m, &key(KeyCode::BTN_EXTRA, v)).is_empty());
        }
        assert_eq!(translate(&m, &key(KeyCode::BTN_MIDDLE, 1)).len(), 2);
        assert_eq!(dump(&translate(&m, &key(KeyCode::BTN_LEFT, 1))), k(KeyCode::BTN_LEFT, 1));
    }

    #[test]
    fn drive_presses_releases_and_ignores_autorepeat() {
        let (rec, p) = player();
        let p = Mutex::new(p);
        let m = parse_config("forward = macro:shift,held\nesc = macro:tap").unwrap();
        let fwd = KeyCode::BTN_EXTRA;
        // a non-key event whose code collides with a mapped key (REL_Y == KEY_ESC) is not a press
        drive(&p, &m, &InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_Y.0, 1));
        assert!(!p.lock().unwrap().is_running(KeyCode::KEY_ESC));
        drive(&p, &m, &key(fwd, 1));
        assert!(p.lock().unwrap().is_running(fwd));
        wait("one full run", || frames(&rec).len() > emits("shift").len());
        drive(&p, &m, &key(fwd, 2));
        assert!(p.lock().unwrap().is_running(fwd));
        drive(&p, &m, &key(fwd, 0));
        assert!(!p.lock().unwrap().is_running(fwd));
        assert_stopped_cleanly(&rec, "shift");
        // unmapped buttons never touch the player
        drive(&p, &m, &key(KeyCode::BTN_LEFT, 1));
        assert!(p.lock().unwrap().running.is_empty());
    }

    #[test]
    fn once_plays_exact_frames_and_ignores_press_while_running() {
        let (rec, mut p) = player();
        let b = KeyCode::BTN_SIDE;
        let one = emits("slow");
        assert_eq!(one, vec![k(KeyCode::KEY_A, 1), k(KeyCode::KEY_A, 0), k(KeyCode::KEY_B, 1), k(KeyCode::KEY_B, 0)]);
        p.press(b, "slow", Playback::Once);
        assert!(p.is_running(b));
        p.press(b, "slow", Playback::Once); // ignored, no queueing
        p.release(b); // does nothing for Once
        assert!(p.is_running(b));
        wait("run to finish", || !p.is_running(b));
        assert_eq!(frames(&rec), one);
        p.press(b, "slow", Playback::Once);
        wait("second run to finish", || !p.is_running(b));
        assert_eq!(frames(&rec), [one.clone(), one].concat());
    }

    #[test]
    fn times_plays_n_copies_in_order() {
        let (rec, mut p) = player();
        let b = KeyCode::BTN_SIDE;
        p.press(b, "tap", Playback::Times(3));
        wait("3 runs", || !p.is_running(b));
        let one = emits("tap");
        assert_eq!(frames(&rec), [one.clone(), one.clone(), one].concat());
    }

    #[test]
    fn held_loops_until_release_without_stuck_keys() {
        let (rec, mut p) = player();
        let b = KeyCode::BTN_EXTRA;
        p.press(b, "shift", Playback::Held);
        wait("one full run", || frames(&rec).len() > emits("shift").len());
        p.press(b, "shift", Playback::Held); // ignored while running
        p.release(b);
        assert!(!p.is_running(b));
        assert_stopped_cleanly(&rec, "shift");
    }

    #[test]
    fn toggle_second_press_stops_without_stuck_keys() {
        let (rec, mut p) = player();
        let b = KeyCode::BTN_EXTRA;
        p.press(b, "shift", Playback::Toggle);
        p.release(b); // does nothing for Toggle
        wait("one full run", || frames(&rec).len() > emits("shift").len());
        assert!(p.is_running(b));
        p.release(b);
        p.press(b, "shift", Playback::Toggle);
        assert!(!p.is_running(b));
        p.release(b);
        assert_stopped_cleanly(&rec, "shift");
    }

    #[test]
    fn stop_all_and_drop_release_held_keys_promptly() {
        let (rec, mut p) = player();
        p.press(KeyCode::BTN_SIDE, "ctrl", Playback::Once);
        wait("ctrl down", || !frames(&rec).is_empty());
        let t = std::time::Instant::now();
        p.stop_all();
        assert!(t.elapsed() < std::time::Duration::from_millis(500), "stop_all took {:?}", t.elapsed());
        assert!(!p.is_running(KeyCode::BTN_SIDE));
        assert_eq!(frames(&rec), vec![k(KeyCode::KEY_LEFTCTRL, 1), k(KeyCode::KEY_LEFTCTRL, 0)]);

        let (rec, mut p) = player();
        p.press(KeyCode::BTN_SIDE, "alt", Playback::Toggle);
        wait("alt down", || !frames(&rec).is_empty());
        let t = std::time::Instant::now();
        drop(p);
        assert!(t.elapsed() < std::time::Duration::from_millis(500), "drop took {:?}", t.elapsed());
        assert_eq!(frames(&rec), vec![k(KeyCode::KEY_LEFTALT, 1), k(KeyCode::KEY_LEFTALT, 0)]);
    }

    #[test]
    fn sigusr1_stops_every_running_macro_and_releases_what_they_hold() {
        let rec = Arc::new(Mutex::new(Recorder::default()));
        let player = Arc::new(Mutex::new(Player::new(rec.clone(), defs())));
        watch_stop_all(player.clone()).unwrap();
        {
            let mut p = player.lock().unwrap();
            p.press(KeyCode::BTN_SIDE, "ctrl", Playback::Toggle);
            p.press(KeyCode::BTN_EXTRA, "shift", Playback::Held);
        }
        wait("both macros playing", || {
            let f = frames(&rec);
            f.contains(&k(KeyCode::KEY_LEFTCTRL, 1)) && f.contains(&k(KeyCode::KEY_LEFTSHIFT, 1))
        });
        let t = std::time::Instant::now();
        // SAFETY: the handler is installed above; raise only sets the flag the watcher thread reads.
        assert_eq!(unsafe { libc::raise(libc::SIGUSR1) }, 0);
        wait("macros stopped", || !player.lock().unwrap().is_running(KeyCode::BTN_SIDE) && !player.lock().unwrap().is_running(KeyCode::BTN_EXTRA));
        assert!(t.elapsed() < std::time::Duration::from_millis(250), "AC-M1: stop took {:?}", t.elapsed());
        let got = frames(&rec);
        thread::sleep(std::time::Duration::from_millis(60));
        assert_eq!(frames(&rec), got, "still playing after SIGUSR1");
        assert_no_stuck(&got);
    }

    /// Succeeds for the first `ok` frames, then fails; records every attempt.
    struct Flaky {
        ok: usize,
        tried: Vec<Frame>,
    }

    impl Sink for Flaky {
        fn emit(&mut self, frame: &[InputEvent]) -> io::Result<()> {
            self.tried.push(dump(frame));
            if self.tried.len() > self.ok { Err(io::Error::other("unplugged")) } else { Ok(()) }
        }
    }

    #[test]
    fn sink_error_ends_worker_and_still_tries_to_release() {
        let sink = Arc::new(Mutex::new(Flaky { ok: 1, tried: vec![] }));
        let mut p = Player::new(sink.clone(), defs());
        let b = KeyCode::BTN_SIDE;
        p.press(b, "combo", Playback::Held);
        wait("worker to exit", || !p.is_running(b));
        // ctrl down ok, a down fails -> ups for a and ctrl are still attempted, newest first
        let (ctrl, a) = (KeyCode::KEY_LEFTCTRL, KeyCode::KEY_A);
        assert_eq!(sink.lock().unwrap().tried, vec![k(ctrl, 1), k(a, 1), k(a, 0), k(ctrl, 0)]);
        p.release(b);

        let dead = Arc::new(Mutex::new(Flaky { ok: 0, tried: vec![] }));
        let mut p = Player::new(dead.clone(), defs());
        p.press(b, "tap", Playback::Times(5));
        wait("worker to exit", || !p.is_running(b));
        assert_eq!(dead.lock().unwrap().tried, vec![k(a, 1), k(a, 0)]);
        // the player still works after a failure
        p.press(b, "tap", Playback::Once);
        wait("worker to exit", || !p.is_running(b));
    }

    #[test]
    fn unknown_macro_does_nothing() {
        let (rec, mut p) = player();
        p.press(KeyCode::BTN_SIDE, "nope", Playback::Toggle);
        assert!(!p.is_running(KeyCode::BTN_SIDE));
        assert!(frames(&rec).is_empty());
    }

    #[test]
    fn buttons_run_concurrently() {
        let (rec, mut p) = player();
        let (b1, b2) = (KeyCode::BTN_SIDE, KeyCode::BTN_EXTRA);
        p.press(b1, "ctrl", Playback::Once);
        p.press(b2, "alt", Playback::Once);
        wait("both downs", || frames(&rec).len() >= 2);
        assert!(p.is_running(b1) && p.is_running(b2));
        // both are down at the same time: neither 1 s macro has reached its up yet
        let mut downs = frames(&rec);
        downs.sort();
        let (ctrl, alt) = (KeyCode::KEY_LEFTCTRL, KeyCode::KEY_LEFTALT);
        let mut want = vec![k(ctrl, 1), k(alt, 1)];
        want.sort();
        assert_eq!(downs, want);
        p.stop_all();
        let got = frames(&rec);
        assert_eq!(got.len(), 4);
        assert_no_stuck(&got);
    }
}
