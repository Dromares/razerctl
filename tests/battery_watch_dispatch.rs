//! `razerctl-gui battery-watch` runs the notifier loop (DAN-1747). The AppImage starts the GUI binary and nothing else, so the
//! `razerctl-battery.service` unit it writes can only ask the GUI binary for the loop. Before the fix the argument was
//! ignored and the unit started a window (with no display: an error exit, and `Restart=on-failure` looping on it).
//!
//! The loop never returns, so the test starts it with no display and notifier disabled in a throwaway config, gives it a
//! moment, and checks it is still running with nothing on stderr from a window toolkit.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
fn the_gui_binary_runs_the_battery_loop_instead_of_opening_a_window() {
    let home = std::env::temp_dir().join(format!("razerctl-battery-watch-{}", std::process::id()));
    let conf = home.join("config/razerctl");
    std::fs::create_dir_all(&conf).unwrap();
    // disabled: the loop sleeps and never touches a mouse or sends a notification
    std::fs::write(
        conf.join("battery.conf"),
        "enabled = false\ninterval_minutes = 60\n",
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_razerctl-gui"))
        .arg("battery-watch")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start razerctl-gui");
    std::thread::sleep(Duration::from_millis(1500));
    let exited = child.try_wait().unwrap();
    let _ = child.kill();
    let _ = child.wait();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let _ = std::fs::remove_dir_all(&home);
    assert!(
        exited.is_none(),
        "battery-watch returned ({exited:?}) instead of looping; stderr: {stderr}"
    );
}
