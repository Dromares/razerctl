//! `razerctl-gui remap` and `razerctl-gui macros …` do what `razerctl remap` / `razerctl macros …` do (DAN-1750). The AppImage
//! starts the GUI binary and nothing else, so the `razerctl-remap.service` unit it writes can only ask the GUI binary for `remap`,
//! and `RestartPreventExitStatus=78` in that unit only works if the GUI binary exits 78 for a config problem. Before the fix the
//! arguments were ignored and the unit started a window (with no display: an error exit, and `Restart=on-failure` looping on it).
//!
//! Every case fails before a device is looked for, with no display, so no window opens and no hardware or /dev/input is touched.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Run `bin args…` with no display and a throwaway HOME, killing it if it is still running after 20 s (an ignored argument
/// that opens a window would otherwise hang the test).
fn run(bin: &str, args: &[&std::ffi::OsStr], home: &Path) -> Output {
    let mut child = Command::new(bin)
        .args(args)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .env_remove("APPIMAGE")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

struct Case {
    home: PathBuf,
}

impl Case {
    fn new(tag: &str, remap_conf: &str) -> Case {
        let home = std::env::temp_dir().join(format!(
            "razerctl-gui-dispatch-{tag}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(home.join("config/razerctl")).unwrap();
        std::fs::write(home.join("remap.conf"), remap_conf).unwrap();
        Case { home }
    }

    fn conf(&self) -> PathBuf {
        self.home.join("remap.conf")
    }

    fn macros(&self) -> PathBuf {
        self.home.join("macros.conf")
    }

    /// The same arguments through the GUI binary and through the CLI.
    fn both(&self, args: &[&std::ffi::OsStr]) -> (Output, Output) {
        (
            run(env!("CARGO_BIN_EXE_razerctl-gui"), args, &self.home),
            run(env!("CARGO_BIN_EXE_razerctl"), args, &self.home),
        )
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn remap_args<'a>(conf: &'a Path, macros: &'a Path) -> Vec<&'a std::ffi::OsStr> {
    vec![
        "remap".as_ref(),
        "--config".as_ref(),
        conf.as_os_str(),
        "--macros".as_ref(),
        macros.as_os_str(),
    ]
}

#[test]
fn the_gui_binary_exits_78_for_a_remap_config_problem_like_the_cli() {
    for (tag, conf, why) in [
        ("norules", "# back = ctrl+c\n", "razerctl: no remap rules"),
        ("badrule", "back = nonsense_key_name\n", "razerctl: "),
        ("nomacros", "back = macro:copy,held\n", "not found"),
    ] {
        let c = Case::new(tag, conf);
        let (conf_p, macros_p) = (c.conf(), c.macros());
        let args = remap_args(&conf_p, &macros_p);
        let (gui, cli) = c.both(&args);
        assert_eq!(
            cli.status.code(),
            Some(78),
            "{tag}: the CLI itself: {}",
            stderr(&cli)
        );
        assert_eq!(gui.status.code(), Some(78), "{tag}: {}", stderr(&gui));
        assert!(stderr(&gui).contains(why), "{tag}: {}", stderr(&gui));
        assert_eq!(stderr(&gui), stderr(&cli), "{tag}: same message as the CLI");
    }
}

#[test]
fn other_remap_failures_stay_exit_1_through_the_gui_binary() {
    let c = Case::new("badarg", "back = ctrl+c\n");
    let (gui, cli) = c.both(&["remap".as_ref(), "--nonsense".as_ref()]);
    assert_eq!(cli.status.code(), Some(1), "{}", stderr(&cli));
    assert_eq!(gui.status.code(), Some(1), "{}", stderr(&gui));
    assert_eq!(stderr(&gui), stderr(&cli));
}

#[test]
fn macros_check_through_the_gui_binary_matches_the_cli() {
    let c = Case::new("check", "back = macro:copy,held\n");
    std::fs::write(c.macros(), "[copy]\ntap ctrl\n").unwrap();
    let (conf_p, macros_p) = (c.conf(), c.macros());
    let args: Vec<&std::ffi::OsStr> = vec![
        "macros".as_ref(),
        "check".as_ref(),
        "--config".as_ref(),
        conf_p.as_os_str(),
        "--file".as_ref(),
        macros_p.as_os_str(),
    ];
    let (gui, cli) = c.both(&args);
    assert_eq!(cli.status.code(), Some(0), "{}", stderr(&cli));
    assert_eq!(gui.status.code(), Some(0), "{}", stderr(&gui));
    assert_eq!(gui.stdout, cli.stdout);
    assert!(String::from_utf8_lossy(&gui.stdout).contains("1 macro reference OK"));

    // the same files with the macro missing: both fail the same way, which is what the GUI's pre-flight reports
    std::fs::write(c.macros(), "[other]\ntap a\n").unwrap();
    let (gui, cli) = c.both(&args);
    assert_eq!(cli.status.code(), Some(1), "{}", stderr(&cli));
    assert_eq!(gui.status.code(), Some(1), "{}", stderr(&gui));
    assert!(
        stderr(&gui).contains("undefined macro 'copy'"),
        "{}",
        stderr(&gui)
    );
    assert_eq!(stderr(&gui), stderr(&cli));
}
