//! The unsaved UI state that survives "hide to the tray" (DAN-1516).
//!
//! Hiding really closes the window and *Show razerctl* `exec`s a fresh GUI (see `tray.rs`), so everything the old
//! `App` held in memory is gone. `Carry` is what the user typed or changed and has not saved yet; the closing window
//! fills it in, `reopen_window` hands it to the new process through a private file named by `RAZERCTL_CARRY`, and
//! `App::new` applies it. Anything already on disk or on the mouse is deliberately not in here: the new process reads
//! that again, and a stale copy must never win over it.
//!
//! The format is one line per fact, fields separated by a tab, with `\`, tab, CR and LF escaped inside a field. It is
//! only ever read by the same binary a moment later, so there is no version negotiation: an unreadable file is
//! dropped and the GUI starts as it always did.

use razerctl::remap;

use super::macro_ui::{Draft, Ev, Sub};

/// Env var the old process sets for the new one. Not `RAZERCTL_GUI_*`: that prefix switches the tray off (`demo.rs`).
pub const ENV: &str = "RAZERCTL_CARRY";
const FILE_PREFIX: &str = "razerctl-gui-carry-";

#[derive(Debug, PartialEq, Default)]
pub struct Carry {
    pub tab: Option<String>,
    /// The `remap.conf` editor text, when it differs from the file.
    pub rules: Option<String>,
    /// (button id, wanted action) for each row edited and not yet written to the mouse.
    pub buttons: Vec<(u8, String)>,
    pub dpi: Option<u16>,
    pub poll: Option<u32>,
    /// The macro editor, when it has unsaved drafts or pending rule removals.
    pub macros: Option<Macros>,
}

#[derive(Debug, PartialEq)]
pub struct Macros {
    pub sub: Sub,
    pub sel: usize,
    pub sel_ev: usize,
    pub drafts: Vec<Draft>,
    pub remove_rules: Vec<String>,
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '\t' => o.push_str("\\t"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            c => o.push(c),
        }
    }
    o
}

fn unesc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            o.push(c);
            continue;
        }
        match it.next() {
            Some('t') => o.push('\t'),
            Some('n') => o.push('\n'),
            Some('r') => o.push('\r'),
            Some(c) => o.push(c),
            None => {}
        }
    }
    o
}

/// `None` is the empty field, `Some(s)` is `=s`: so `Some("")` and `None` stay different.
fn opt(s: &Option<String>) -> String {
    s.as_ref()
        .map(|s| format!("={}", esc(s)))
        .unwrap_or_default()
}

fn unopt(s: &str) -> Option<String> {
    s.strip_prefix('=').map(unesc)
}

fn line(out: &mut String, fields: &[&str]) {
    out.push_str(&fields.join("\t"));
    out.push('\n');
}

impl Carry {
    pub fn encode(&self) -> String {
        let mut o = String::new();
        if let Some(t) = &self.tab {
            line(&mut o, &["tab", &esc(t)]);
        }
        if let Some(r) = &self.rules {
            line(&mut o, &["rules", &esc(r)]);
        }
        for (id, a) in &self.buttons {
            line(&mut o, &["button", &id.to_string(), &esc(a)]);
        }
        if let Some(d) = self.dpi {
            line(&mut o, &["dpi", &d.to_string()]);
        }
        if let Some(p) = self.poll {
            line(&mut o, &["poll", &p.to_string()]);
        }
        if let Some(m) = &self.macros {
            let sub = if m.sub == Sub::Macros {
                "macros"
            } else {
                "rules"
            };
            line(
                &mut o,
                &["macros", sub, &m.sel.to_string(), &m.sel_ev.to_string()],
            );
            for r in &m.remove_rules {
                line(&mut o, &["unrule", &esc(r)]);
            }
            for d in &m.drafts {
                line(
                    &mut o,
                    &["draft", &esc(&d.name), &opt(&d.guid), &opt(&d.orig)],
                );
                for e in &d.events {
                    match e {
                        Ev::Key(t, down) => line(&mut o, &["key", flag(*down), &esc(t)]),
                        Ev::Button(k, down) => {
                            line(&mut o, &["btn", flag(*down), &format!("{k:?}")])
                        }
                        Ev::Wheel(n) => line(&mut o, &["wheel", &n.to_string()]),
                        Ev::HWheel(n) => line(&mut o, &["hwheel", &n.to_string()]),
                        Ev::Move(x, y) => line(&mut o, &["move", &x.to_string(), &y.to_string()]),
                        Ev::Delay(n) => line(&mut o, &["delay", &n.to_string()]),
                    }
                }
            }
        }
        o
    }

    /// `None` when any line is not understood: a half-restored editor is worse than a fresh one.
    pub fn decode(text: &str) -> Option<Carry> {
        let mut c = Carry::default();
        for l in text.lines() {
            let f: Vec<&str> = l.split('\t').collect();
            match (f[0], &f[1..]) {
                ("tab", [t]) => c.tab = Some(unesc(t)),
                ("rules", [r]) => c.rules = Some(unesc(r)),
                ("button", [id, a]) => c.buttons.push((id.parse().ok()?, unesc(a))),
                ("dpi", [d]) => c.dpi = Some(d.parse().ok()?),
                ("poll", [p]) => c.poll = Some(p.parse().ok()?),
                ("macros", [sub, sel, sel_ev]) => {
                    c.macros = Some(Macros {
                        sub: if *sub == "macros" {
                            Sub::Macros
                        } else {
                            Sub::Rules
                        },
                        sel: sel.parse().ok()?,
                        sel_ev: sel_ev.parse().ok()?,
                        drafts: Vec::new(),
                        remove_rules: Vec::new(),
                    })
                }
                ("unrule", [r]) => c.macros.as_mut()?.remove_rules.push(unesc(r)),
                ("draft", [name, guid, orig]) => c.macros.as_mut()?.drafts.push(Draft {
                    name: unesc(name),
                    guid: unopt(guid),
                    orig: unopt(orig),
                    events: Vec::new(),
                }),
                (kind, args) => {
                    let ev = match (kind, args) {
                        ("key", [d, t]) => Ev::Key(unesc(t), *d == "1"),
                        ("btn", [d, k]) => Ev::Button(remap::parse_key(k).ok()?, *d == "1"),
                        ("wheel", [n]) => Ev::Wheel(n.parse().ok()?),
                        ("hwheel", [n]) => Ev::HWheel(n.parse().ok()?),
                        ("move", [x, y]) => Ev::Move(x.parse().ok()?, y.parse().ok()?),
                        ("delay", [n]) => Ev::Delay(n.parse().ok()?),
                        _ => return None,
                    };
                    c.macros.as_mut()?.drafts.last_mut()?.events.push(ev);
                }
            }
        }
        Some(c)
    }
}

fn flag(down: bool) -> &'static str {
    if down {
        "1"
    } else {
        "0"
    }
}

// ------------------------------------------------------------------------------------------------ the hand-over file

/// Where the old process leaves the state: the runtime dir (private to the user), else the temp dir. The name is the pid
/// (unique between processes; the receiver is handed the path, so it never has to guess) plus a per-process counter, so
/// two stashes in one process (the tests share a pid and run in parallel) never write the same file.
fn path_for(pid: u32, seq: u64) -> std::path::PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|d| d.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    dir.join(format!("{FILE_PREFIX}{pid}-{seq}"))
}

/// Write `text` for the next process. Returns the path to put in `ENV`.
pub fn stash(text: &str) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = path_for(
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    );
    let _ = std::fs::remove_file(&path);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    f.write_all(text.as_bytes())?;
    Ok(path)
}

/// The new process: read and delete the file `ENV` names. The variable is removed so a later `exec` cannot inherit it.
pub fn take_from_env() -> Option<Carry> {
    let path = std::path::PathBuf::from(std::env::var_os(ENV)?);
    std::env::remove_var(ENV);
    take_file(&path)
}

pub fn take_file(path: &std::path::Path) -> Option<Carry> {
    // only ever a file `stash` made: the variable is not a way to make this process read or delete something else
    if !path.file_name()?.to_string_lossy().starts_with(FILE_PREFIX) {
        return None;
    }
    let text = std::fs::read_to_string(path);
    let _ = std::fs::remove_file(path);
    Carry::decode(&text.ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use evdev::KeyCode;

    fn sample() -> Carry {
        Carry {
            tab: Some("Software".into()),
            rules: Some("back = ctrl+c\n\tweird \\ line\r\n".into()),
            buttons: vec![(4, "key:f13".into()), (5, "dpi-up".into())],
            dpi: Some(1600),
            poll: Some(500),
            macros: Some(Macros {
                sub: Sub::Macros,
                sel: 1,
                sel_ev: 2,
                remove_rules: vec!["old".into()],
                drafts: vec![
                    Draft {
                        name: "copy".into(),
                        guid: Some("g-1".into()),
                        orig: Some("copy".into()),
                        events: vec![
                            Ev::Key("ctrl".into(), true),
                            Ev::Key("half-typ".into(), false),
                            Ev::Button(KeyCode::BTN_SIDE, true),
                            Ev::Wheel(-2),
                            Ev::HWheel(3),
                            Ev::Move(-5, 7),
                            Ev::Delay(40),
                        ],
                    },
                    Draft::new("fresh", vec![]),
                ],
            }),
        }
    }

    #[test]
    fn encode_decode_round_trips_every_field() {
        let c = sample();
        assert_eq!(Carry::decode(&c.encode()), Some(c));
    }

    #[test]
    fn an_unreadable_line_drops_the_whole_thing() {
        assert_eq!(Carry::decode("tab\tApp\nnonsense\t1\n"), None);
        assert_eq!(Carry::decode("draft\tx\t\t\n"), None, "draft before macros");
        assert_eq!(Carry::decode("dpi\tmany\n"), None);
    }

    #[test]
    fn empty_text_is_an_empty_carry() {
        assert_eq!(Carry::decode(""), Some(Carry::default()));
    }

    #[test]
    fn two_stashes_in_one_process_do_not_share_a_file() {
        let a = stash("dpi\t800\n").unwrap();
        let b = stash("dpi\t1600\n").unwrap();
        assert_ne!(a, b);
        assert_eq!(take_file(&a).and_then(|c| c.dpi), Some(800));
        assert_eq!(take_file(&b).and_then(|c| c.dpi), Some(1600));
    }

    #[test]
    fn only_a_file_this_module_made_is_read_or_deleted() {
        let dir = std::env::temp_dir().join(format!("razerctl-carry-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let other = dir.join("precious.txt");
        std::fs::write(&other, "dpi\t800\n").unwrap();
        assert_eq!(take_file(&other), None);
        assert!(other.exists(), "not ours: left alone");
        let ours = dir.join(format!("{FILE_PREFIX}1"));
        std::fs::write(&ours, "dpi\t800\n").unwrap();
        assert_eq!(
            take_file(&ours),
            Some(Carry {
                dpi: Some(800),
                ..Carry::default()
            })
        );
        assert!(!ours.exists(), "consumed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
