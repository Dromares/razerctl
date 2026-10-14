//! The remap service as the GUI can verify it (DAN-1439). `systemctl restart` returning 0 only means the job was
//! queued: a unit whose binary exits at once (`no remap rules`, an old build with no `macros`) still "restarts"
//! fine and then crash-loops under `Restart=`. So after every start we read the unit back a moment later, and
//! before every start we check the binary the unit will actually run.
//!
//! The first half is parsing and decisions with no process spawning in it; every `systemctl` / `journalctl` /
//! `razerctl macros check` call goes through [`Sys`], so tests stub it and never touch the real service.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use eframe::egui;
use razerctl::remap::{self, Action};

use super::macro_ui::tilde;
use super::{App, SERVICE};

/// How long after a start the unit is read back. `RestartSec=2` means a crash is visible as
/// `activating (auto-restart)` for the two seconds after it; this is inside that window.
pub const SETTLE: Duration = Duration::from_millis(1500);

pub struct Out {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// The only door to the outside: `Sys::run("systemctl", ["--user", ...])` and friends.
pub trait Sys {
    fn run(&self, prog: &str, args: &[&str]) -> Result<Out, String>;
}

pub struct RealSys;

impl Sys for RealSys {
    fn run(&self, prog: &str, args: &[&str]) -> Result<Out, String> {
        let o = Command::new(prog).args(args).output().map_err(|e| e.to_string())?;
        Ok(Out {
            ok: o.status.success(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        })
    }
}

pub fn systemctl(sys: &dyn Sys, args: &[&str]) -> Result<Out, String> {
    let mut a = vec!["--user"];
    a.extend_from_slice(args);
    sys.run("systemctl", &a)
}

// ------------------------------------------------------------------------------------------ unit state

/// What `systemctl --user show` says about the remap unit.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UnitState {
    /// `LoadState`: `not-found` when the unit file is not installed.
    pub load: String,
    pub active: String,
    pub sub: String,
    pub restarts: u32,
    /// `ExecMainStatus`: exit status of the last run of the main process.
    pub exit_status: Option<i32>,
    /// The binary the unit's `ExecStart` runs.
    pub binary: Option<PathBuf>,
}

pub const SHOW_PROPS: [&str; 10] = ["-p", "LoadState", "-p", "ActiveState", "-p", "SubState", "-p", "NRestarts", "-p", "ExecMainStatus"];

pub fn parse_show(text: &str) -> UnitState {
    let mut u = UnitState::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k {
            "LoadState" => u.load = v.into(),
            "ActiveState" => u.active = v.into(),
            "SubState" => u.sub = v.into(),
            "NRestarts" => u.restarts = v.parse().unwrap_or(0),
            "ExecMainStatus" => u.exit_status = v.parse().ok(),
            "ExecStart" => u.binary = exec_binary(v),
            _ => {}
        }
    }
    u
}

/// The executable out of a `show` ExecStart value:
/// `{ path=/home/u/.local/bin/razerctl ; argv[]=/home/u/.local/bin/razerctl remap ; ignore_errors=no ; ... }`.
pub fn exec_binary(value: &str) -> Option<PathBuf> {
    let (_, rest) = value.split_once("path=")?;
    let p = rest.split([';', '}']).next()?.trim();
    (!p.is_empty()).then(|| PathBuf::from(p))
}

impl UnitState {
    /// Only `active (running)` is up. `activating (auto-restart)` is the unit waiting to crash again.
    pub fn running(&self) -> bool {
        self.active == "active" && self.sub == "running"
    }

    /// `activating (auto-restart), exit status 1, 3 restarts`
    pub fn summary(&self) -> String {
        let mut s = if self.sub.is_empty() { self.active.clone() } else { format!("{} ({})", self.active, self.sub) };
        if let Some(c) = self.exit_status.filter(|c| *c != 0) {
            s += &format!(", exit status {c}");
        }
        if self.restarts > 0 {
            s += &format!(", {} restart{}", self.restarts, if self.restarts == 1 { "" } else { "s" });
        }
        s
    }
}

/// Why a unit read back after a start is not healthy; `None` = it is up and has not restarted since `first`
/// (read right after the start command returned).
pub fn judge(first: &UnitState, now: &UnitState) -> Option<String> {
    if now.load == "not-found" {
        return Some(format!("{SERVICE} is not installed"));
    }
    if !now.running() {
        return Some(now.summary());
    }
    (now.restarts > first.restarts).then(|| format!("started, then exited and was restarted ({})", now.summary()))
}

/// What the remap panel shows when the unit is not healthy.
#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    /// The unit's state, from [`judge`] or [`UnitState::summary`].
    pub state: String,
    /// The last thing the service itself logged (`razerctl: no remap rules`).
    pub line: Option<String>,
}

impl Failure {
    pub fn full(&self) -> String {
        match &self.line {
            Some(l) => format!("{l} ({})", self.state),
            None => self.state.clone(),
        }
    }
}

/// The line that says why, out of `journalctl -o cat`: the last `razerctl: …` error, else the last line.
pub fn last_line(journal: &str) -> Option<String> {
    let lines: Vec<&str> = journal.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    lines.iter().rev().find(|l| l.starts_with("razerctl:")).or(lines.last()).map(|l| l.to_string())
}

/// Last journal line the service logged since `since` (unix seconds). Reads the unit's own output only
/// (`_SYSTEMD_USER_UNIT`), not systemd's "Main process exited" bookkeeping about it.
pub fn journal_line(sys: &dyn Sys, since: Option<u64>) -> Option<String> {
    let m = format!("_SYSTEMD_USER_UNIT={SERVICE}");
    let since = since.map(|s| format!("@{s}"));
    let mut a = vec!["--user", m.as_str(), "-n", "20", "-o", "cat", "--no-pager"];
    if let Some(s) = &since {
        a.extend(["--since", s.as_str()]);
    }
    let o = sys.run("journalctl", &a).ok().filter(|o| o.ok)?;
    last_line(&o.stdout)
}

// ------------------------------------------------------------------------- which config the service reads

/// The `remap.conf` the service reads, from `systemctl --user show-environment` (the manager's environment is what
/// the unit inherits): `$XDG_CONFIG_HOME/razerctl/remap.conf`, else `$HOME/.config/razerctl/remap.conf`.
pub fn service_conf(env_text: &str, fallback_home: &Path) -> PathBuf {
    let get = |k: &str| env_text.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix('=')).filter(|v| !v.is_empty());
    let base = match get("XDG_CONFIG_HOME") {
        Some(x) if x.starts_with('/') => PathBuf::from(x),
        _ => get("HOME").map_or_else(|| fallback_home.to_path_buf(), PathBuf::from).join(".config"),
    };
    base.join("razerctl/remap.conf")
}

// -------------------------------------------------------------------------------------------- pre-flight

/// Whether remap.conf text has a `macro:NAME` rule. Text that does not parse says no: the service will say why.
pub fn uses_macros(conf_text: &str) -> bool {
    remap::parse_config(conf_text).is_ok_and(|m| m.values().any(|a| matches!(a, Action::Macro { .. })))
}

/// Before a start: is what the unit would run able to run the rules it will read? Only asks the binary when the
/// rules play a macro. `conf` is the file the service reads, not necessarily the one this window edits.
pub fn preflight(sys: &dyn Sys, unit: &UnitState, conf: &Path) -> Result<(), String> {
    if unit.load == "not-found" {
        return Err(format!("{SERVICE} is not installed. Run ./install.sh first."));
    }
    let Some(bin) = &unit.binary else { return Ok(()) };
    let Ok(text) = std::fs::read_to_string(conf) else { return Ok(()) };
    if !uses_macros(&text) {
        return Ok(());
    }
    let (conf_s, macros_p) = (conf.to_string_lossy(), conf.with_file_name("macros.conf"));
    let macros_s = macros_p.to_string_lossy();
    let bin_s = bin.to_string_lossy();
    match sys.run(&bin_s, &["macros", "check", "--config", &conf_s, "--file", &macros_s]) {
        Err(e) => Err(format!("The service binary {} can't be run ({e}); reinstall razerctl.", tilde(bin))),
        Ok(o) if o.ok => Ok(()),
        Ok(o) if o.stderr.contains("unknown command") => Err(format!(
            "The installed razerctl at {} is too old for macros (it has no `macros` command); reinstall it with ./install.sh.",
            tilde(bin)
        )),
        Ok(o) => {
            let why = last_line(&o.stderr).or_else(|| last_line(&o.stdout)).unwrap_or_else(|| "failed".into());
            Err(format!("`{} macros check` failed for {}: {why}", tilde(bin), tilde(conf)))
        }
    }
}

// --------------------------------------------------------------------------------------------- App glue

/// A start whose result has not been read back yet.
pub struct Pending {
    pub due: Instant,
    pub first: UnitState,
    pub since: u64,
    /// The macros were saved by the same click, so the message says so.
    pub saved: bool,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl App {
    pub fn unit_state(&self) -> UnitState {
        let mut a = vec!["show", SERVICE];
        a.extend(SHOW_PROPS);
        a.extend(["-p", "ExecStart"]);
        match systemctl(&*self.sys, &a) {
            Ok(o) => parse_show(&o.stdout),
            Err(e) => UnitState { active: format!("systemctl unavailable: {e}"), ..Default::default() },
        }
    }

    /// The remap.conf the service reads, if the manager's environment can be read.
    pub fn service_conf_path(&self) -> Option<PathBuf> {
        let o = systemctl(&*self.sys, &["show-environment"]).ok().filter(|o| o.ok)?;
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        Some(service_conf(&o.stdout, &home))
    }

    /// One line for the banner when this window edits a different config than the service reads.
    pub fn config_dir_warning(&self) -> Option<String> {
        let svc = self.svc_conf.as_ref().filter(|s| **s != self.conf_path)?;
        Some(format!(
            "This window edits {}, but the service reads {}. Saving here does not change what the service runs.",
            tilde(&self.conf_path),
            tilde(svc)
        ))
    }

    /// Re-read the unit. A unit that is `failed` or `activating` gets the service's last log line next to it.
    pub fn refresh_service(&mut self) {
        let u = self.unit_state();
        self.service_state = u.active.clone();
        self.svc_conf = self.service_conf_path();
        if self.svc_pending.is_none() {
            self.svc_failure = match u.active.as_str() {
                _ if u.running() => None,
                "failed" | "activating" => Some(Failure { state: u.summary(), line: journal_line(&*self.sys, None) }),
                _ => None,
            };
        }
        self.batt_service = match systemctl(&*self.sys, &["is-active", battery_unit()]) {
            Ok(o) => o.stdout.trim().to_string(),
            Err(e) => format!("systemctl unavailable: {e}"),
        };
    }

    /// `systemctl --user ARGS` on the remap unit (or any other the GUI manages). Starts are checked first and read
    /// back afterwards; see the module doc.
    pub fn systemctl(&mut self, args: &[&str]) -> bool {
        self.service_cmd(args, false)
    }

    /// The restart after "Save and restart service": the macros are already saved.
    pub fn restart_after_save(&mut self) -> bool {
        self.service_cmd(&["restart", SERVICE], true)
    }

    fn service_cmd(&mut self, args: &[&str], saved: bool) -> bool {
        let starts = matches!(args, ["start" | "restart", u] | ["enable", "--now", u] if *u == SERVICE);
        let mut first = None;
        if starts {
            let unit = self.unit_state();
            let conf = self.service_conf_path().unwrap_or_else(|| self.conf_path.clone());
            if let Err(e) = preflight(&*self.sys, &unit, &conf) {
                self.svc_pending = None;
                self.err(if saved { format!("Macros saved, but the service was not restarted. {e}") } else { format!("The service was not started. {e}") });
                self.refresh_service();
                return false;
            }
        } else if args.contains(&SERVICE) {
            self.svc_pending = None; // stop / disable: whatever was being watched is moot
        }
        let started = unix_now();
        let ok = match systemctl(&*self.sys, args) {
            Ok(o) if o.ok => {
                if starts {
                    first = Some(self.unit_state());
                }
                self.ok(if starts { format!("systemctl {} ok. Checking that the service stays up…", args.join(" ")) } else { format!("systemctl {} ok", args.join(" ")) });
                true
            }
            Ok(o) => {
                self.err(o.stderr.trim().to_string());
                false
            }
            Err(e) => {
                self.err(e);
                false
            }
        };
        match first {
            Some(first) => {
                self.svc_failure = None;
                self.svc_pending = Some(Pending { due: Instant::now() + SETTLE, first, since: started.saturating_sub(1), saved });
                self.service_state = self.unit_state().active;
            }
            None => self.refresh_service(),
        }
        ok
    }

    /// Called every frame: once the settle time is up, read the unit back and say what happened.
    pub fn poll_service(&mut self, ctx: &egui::Context) {
        let Some(due) = self.svc_pending.as_ref().map(|p| p.due) else { return };
        let now = Instant::now();
        if now < due {
            ctx.request_repaint_after(due - now);
            return;
        }
        self.finish_service_check();
    }

    /// Read the unit back now (the deferred half of [`App::systemctl`]).
    pub fn finish_service_check(&mut self) {
        let Some(p) = self.svc_pending.take() else { return };
        let now = self.unit_state();
        self.service_state = now.active.clone();
        let saved = if p.saved { "Macros saved. " } else { "" };
        match judge(&p.first, &now) {
            None => {
                self.svc_failure = None;
                if p.saved {
                    self.ok("Macros saved and the service restarted. It is running.");
                } else {
                    self.ok("The remap service is running.");
                }
            }
            Some(why) => {
                let f = Failure { state: why, line: journal_line(&*self.sys, Some(p.since)) };
                self.err(format!("{saved}The remap service failed: {}", f.full()));
                self.svc_failure = Some(f);
            }
        }
    }
}

fn battery_unit() -> &'static str {
    razerctl::battery::SERVICE
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    /// (success, stdout, stderr) or a spawn error.
    type Answer = Result<(bool, String, String), String>;

    /// Canned answers keyed by the full command line; everything it is asked is recorded.
    #[derive(Clone, Default)]
    pub struct Fake {
        pub answers: Rc<RefCell<HashMap<String, Answer>>>,
        pub calls: Rc<RefCell<Vec<String>>>,
    }

    impl Fake {
        pub fn on(&self, cmd: &str, ok: bool, stdout: &str, stderr: &str) {
            self.answers.borrow_mut().insert(cmd.into(), Ok((ok, stdout.into(), stderr.into())));
        }
        pub fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl Sys for Fake {
        fn run(&self, prog: &str, args: &[&str]) -> Result<Out, String> {
            let cmd = format!("{prog} {}", args.join(" "));
            self.calls.borrow_mut().push(cmd.clone());
            // unlisted commands answer "fine, nothing to say"; a key ending in `*` matches any command with that prefix
            let answers = self.answers.borrow();
            let hit = answers.get(&cmd).or_else(|| answers.iter().find(|(k, _)| k.strip_suffix('*').is_some_and(|p| cmd.starts_with(p))).map(|(_, v)| v));
            match hit.cloned().unwrap_or(Ok((true, String::new(), String::new()))) {
                Ok((ok, stdout, stderr)) => Ok(Out { ok, stdout, stderr }),
                Err(e) => Err(e),
            }
        }
    }

    pub const CRASH_LOOP: &str = "LoadState=loaded\nActiveState=activating\nSubState=auto-restart\nNRestarts=31\nExecMainStatus=1\n\
        ExecStart={ path=/home/u/.local/bin/razerctl ; argv[]=/home/u/.local/bin/razerctl remap ; ignore_errors=no ; \
        start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }\n";
    pub const HEALTHY: &str = "LoadState=loaded\nActiveState=active\nSubState=running\nNRestarts=0\nExecMainStatus=0\n";

    #[test]
    fn show_output_parses_including_the_crash_loop_state() {
        let u = parse_show(CRASH_LOOP);
        assert_eq!((u.load.as_str(), u.active.as_str(), u.sub.as_str()), ("loaded", "activating", "auto-restart"));
        assert_eq!((u.restarts, u.exit_status), (31, Some(1)));
        assert_eq!(u.binary, Some(PathBuf::from("/home/u/.local/bin/razerctl")));
        assert!(!u.running());
        assert_eq!(u.summary(), "activating (auto-restart), exit status 1, 31 restarts");
        let h = parse_show(HEALTHY);
        assert!(h.running() && h.binary.is_none());
        assert_eq!(h.summary(), "active (running)");
        // a unit file that is not installed
        let n = parse_show("LoadState=not-found\nActiveState=inactive\nSubState=dead\nNRestarts=0\nExecMainStatus=0\nExecStart=\n");
        assert_eq!((n.load.as_str(), n.binary), ("not-found", None));
    }

    #[test]
    fn exec_start_resolves_to_the_binary_the_unit_runs() {
        let v = "{ path=/usr/bin/razerctl ; argv[]=/usr/bin/razerctl remap ; ignore_errors=no ; }";
        assert_eq!(exec_binary(v), Some(PathBuf::from("/usr/bin/razerctl")));
        // path= is the executable even when argv[0] is something else, and a space in the path survives
        assert_eq!(exec_binary("{ path=/opt/my tools/razerctl ; argv[]=razerctl remap ; }"), Some(PathBuf::from("/opt/my tools/razerctl")));
        assert_eq!(exec_binary("{ path=/bin/x }"), Some(PathBuf::from("/bin/x")));
        assert_eq!(exec_binary(""), None);
        assert_eq!(exec_binary("{ argv[]=razerctl }"), None);
    }

    #[test]
    fn auto_restart_is_a_failure_not_running() {
        let first = parse_show(HEALTHY);
        let looping = parse_show(CRASH_LOOP);
        assert_eq!(judge(&first, &looping).as_deref(), Some("activating (auto-restart), exit status 1, 31 restarts"));
        assert_eq!(judge(&first, &first), None, "active (running), no restarts since");
        // up at the moment we look, but it has already died and come back since the start
        let mut flicker = first.clone();
        flicker.restarts = 2;
        assert!(judge(&first, &flicker).unwrap().starts_with("started, then exited and was restarted"));
        // a unit that ran all week keeps its old count: only restarts since the start count
        let mut old = first.clone();
        old.restarts = 4;
        assert_eq!(judge(&old, &old), None);
        let failed = parse_show("LoadState=loaded\nActiveState=failed\nSubState=failed\nNRestarts=0\nExecMainStatus=78\n");
        assert_eq!(judge(&first, &failed).as_deref(), Some("failed (failed), exit status 78"));
        assert!(judge(&first, &parse_show("LoadState=not-found\nActiveState=inactive\n")).unwrap().contains("not installed"));
    }

    #[test]
    fn the_services_own_log_line_is_picked_over_systemds() {
        let j = "razerctl: no remap rules\nrazerctl: no remap rules\n\n";
        assert_eq!(last_line(j).as_deref(), Some("razerctl: no remap rules"));
        assert_eq!(last_line("grabbed /dev/input/event2\nrazerctl: boom\nremapping 2 rule(s)\n").as_deref(), Some("razerctl: boom"));
        assert_eq!(last_line("some panic text\n").as_deref(), Some("some panic text"));
        assert_eq!(last_line("  \n"), None);
        let f = Failure { state: "failed (failed)".into(), line: Some("razerctl: no remap rules".into()) };
        assert_eq!(f.full(), "razerctl: no remap rules (failed (failed))");
        assert_eq!(Failure { state: "inactive".into(), line: None }.full(), "inactive");
        let sys = Fake::default();
        sys.on(
            "journalctl --user _SYSTEMD_USER_UNIT=razerctl-remap.service -n 20 -o cat --no-pager --since @100",
            true, "razerctl: no remap rules\n", "",
        );
        assert_eq!(journal_line(&sys, Some(100)).as_deref(), Some("razerctl: no remap rules"));
        assert_eq!(journal_line(&Fake::default(), None), None, "nothing logged");
    }

    #[test]
    fn service_config_follows_the_managers_environment_not_ours() {
        let home = Path::new("/home/me");
        assert_eq!(service_conf("HOME=/home/me\nPATH=/bin\n", home), PathBuf::from("/home/me/.config/razerctl/remap.conf"));
        assert_eq!(service_conf("XDG_CONFIG_HOME=/x/cfg\nHOME=/home/me\n", home), PathBuf::from("/x/cfg/razerctl/remap.conf"));
        assert_eq!(service_conf("XDG_CONFIG_HOME=\n", home), PathBuf::from("/home/me/.config/razerctl/remap.conf"), "empty or relative is ignored");
        assert_eq!(service_conf("XDG_CONFIG_HOME=rel\n", home), PathBuf::from("/home/me/.config/razerctl/remap.conf"));
        assert_eq!(service_conf("HOME=/home/other\n", home), PathBuf::from("/home/other/.config/razerctl/remap.conf"));
    }

    #[test]
    fn only_macro_rules_make_the_binary_get_asked() {
        assert!(uses_macros("back = macro:copy,held\n"));
        assert!(!uses_macros("back = ctrl+c\n# forward = macro:x\n"));
        assert!(!uses_macros("this is not a rule"), "unparsable: the service will say");
        assert!(!uses_macros(""));
    }

    struct Dir(PathBuf);
    impl Dir {
        fn new(tag: &str, conf: &str) -> Dir {
            let d = std::env::temp_dir().join(format!("razerctl-svc-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("remap.conf"), conf).unwrap();
            Dir(d)
        }
        fn conf(&self) -> PathBuf {
            self.0.join("remap.conf")
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn preflight_blocks_a_binary_too_old_for_macros_and_a_failing_check() {
        let unit = parse_show(CRASH_LOOP);
        let d = Dir::new("pre", "back = macro:copy,held\n");
        let cmd = format!("/home/u/.local/bin/razerctl macros check --config {} --file {}", d.conf().display(), d.0.join("macros.conf").display());
        let sys = Fake::default();
        // an old build: `macros` is not a command
        sys.on(&cmd, false, "", "razerctl: unknown command 'macros'\n\nUSAGE: ...");
        let e = preflight(&sys, &unit, &d.conf()).unwrap_err();
        assert!(e.contains("too old for macros") && e.contains("reinstall") && e.contains("/home/u/.local/bin/razerctl"), "{e}");
        // a new build that rejects the config
        sys.on(&cmd, false, "", "razerctl: /x/macros.conf: not found (the mapping uses macro:NAME)\n");
        let e = preflight(&sys, &unit, &d.conf()).unwrap_err();
        assert!(e.contains("macros check") && e.contains("not found") && !e.contains("restarted"), "{e}");
        // a binary that cannot run at all
        sys.answers.borrow_mut().insert(cmd.clone(), Err("No such file or directory (os error 2)".into()));
        assert!(preflight(&sys, &unit, &d.conf()).unwrap_err().contains("can't be run"));
        // a good one
        sys.on(&cmd, true, "ok", "");
        assert_eq!(preflight(&sys, &unit, &d.conf()), Ok(()));
    }

    #[test]
    fn preflight_does_not_ask_the_binary_without_macro_rules_and_blocks_a_missing_unit() {
        let unit = parse_show(CRASH_LOOP);
        let d = Dir::new("plain", "back = ctrl+c\n");
        let sys = Fake::default();
        assert_eq!(preflight(&sys, &unit, &d.conf()), Ok(()));
        assert_eq!(preflight(&sys, &unit, Path::new("/nonexistent/remap.conf")), Ok(()), "no file: nothing to check");
        assert!(sys.calls().is_empty(), "the binary was never run: {:?}", sys.calls());
        let gone = parse_show("LoadState=not-found\n");
        assert!(preflight(&sys, &gone, &d.conf()).unwrap_err().contains("not installed"));
    }
}
