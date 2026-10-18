//! `--version` / `-V` on both binaries, and `--help` on the GUI (DAN-1698). `razerctl --version` once fell through to
//! device discovery and exited 1 with "no Razer HID devices found", and `razerctl-gui` ignored every argument and
//! tried to open a window, so the AppImage (whose AppRun starts the GUI) had no way to print its version.
//!
//! Each binary runs with no display and `HOME` pointing at an empty directory, so a flag that does not return before
//! the window or the config is touched fails here. The test also never reaches a mouse when the flags work.

use std::process::{Command, Output};

const VERSION_LINE: &str = concat!("razerctl ", env!("CARGO_PKG_VERSION"), "\n");

fn run(exe: &str, args: &[&str]) -> Output {
    let home = std::env::temp_dir().join(format!("razerctl-version-flags-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let out = Command::new(exe)
        .args(args)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .output()
        .unwrap_or_else(|e| panic!("{exe}: {e}"));
    let _ = std::fs::remove_dir_all(&home);
    out
}

fn assert_prints(out: &Output, what: &str, expected: impl Fn(&str) -> bool) {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "{what}: exit {:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status.code()
    );
    assert!(expected(&stdout), "{what}: unexpected stdout {stdout:?}\nstderr: {stderr}");
}

#[test]
fn cli_prints_its_version() {
    for flag in ["--version", "-V"] {
        let out = run(env!("CARGO_BIN_EXE_razerctl"), &[flag]);
        assert_prints(&out, &format!("razerctl {flag}"), |s| s == VERSION_LINE);
    }
}

#[test]
fn cli_help_is_unchanged() {
    for flag in ["--help", "-h"] {
        let out = run(env!("CARGO_BIN_EXE_razerctl"), &[flag]);
        assert_prints(&out, &format!("razerctl {flag}"), |s| s.contains("USAGE:") && s.contains("COMMANDS:"));
    }
}

#[test]
fn gui_prints_its_version_without_a_display() {
    for flag in ["--version", "-V"] {
        let out = run(env!("CARGO_BIN_EXE_razerctl-gui"), &[flag]);
        assert_prints(&out, &format!("razerctl-gui {flag}"), |s| s == VERSION_LINE);
    }
}

#[test]
fn gui_prints_usage_without_a_display() {
    for flag in ["--help", "-h"] {
        let out = run(env!("CARGO_BIN_EXE_razerctl-gui"), &[flag]);
        assert_prints(&out, &format!("razerctl-gui {flag}"), |s| {
            s.contains("USAGE:") && s.contains("razerctl-gui") && s.contains("--version")
        });
    }
}
