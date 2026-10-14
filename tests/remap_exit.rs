//! `razerctl remap` exit statuses, which `systemd/razerctl-remap.service` keys its restart policy on:
//! 78 (EX_CONFIG) = the config is the problem, do not restart; 1 = anything else, retry.
//! Every case fails before a device is looked for, so no hardware or /dev/input is touched.

use std::path::PathBuf;
use std::process::{Command, Output};

fn remap(conf_text: &str, tag: &str) -> Output {
    let dir: PathBuf = std::env::temp_dir().join(format!("razerctl-exit-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let conf = dir.join("remap.conf");
    std::fs::write(&conf, conf_text).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_razerctl"))
        .args(["remap", "--config"]).arg(&conf).args(["--macros"]).arg(dir.join("macros.conf"))
        .output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn no_rules_exits_78_with_the_reason() {
    let o = remap("# back = ctrl+c\n", "norules");
    assert_eq!(o.status.code(), Some(78), "{}", stderr(&o));
    assert!(stderr(&o).contains("razerctl: no remap rules"), "{}", stderr(&o));
}

#[test]
fn unparsable_rule_and_missing_macros_file_exit_78() {
    let o = remap("back = nonsense_key_name\n", "badrule");
    assert_eq!(o.status.code(), Some(78), "{}", stderr(&o));
    let o = remap("back = macro:copy,held\n", "nomacros");
    assert_eq!(o.status.code(), Some(78), "{}", stderr(&o));
    assert!(stderr(&o).contains("not found"), "{}", stderr(&o));
}

#[test]
fn other_failures_stay_exit_1_so_they_are_retried() {
    let o = Command::new(env!("CARGO_BIN_EXE_razerctl")).args(["--backend", "bogus", "list"]).output().unwrap();
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
}

/// DAN-1476: Stop all macros (SIGUSR1) pressed while `razerctl remap` is still starting up must not kill the service
/// (SIGUSR1's default action), which systemd would then restart. The macros file is a FIFO so the process is held
/// deterministically inside startup: past the config, before any device is looked for.
#[test]
fn sigusr1_during_startup_does_not_kill_the_service() {
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::ExitStatusExt;

    let dir: PathBuf = std::env::temp_dir().join(format!("razerctl-exit-usr1-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let conf = dir.join("remap.conf");
    std::fs::write(&conf, "back = macro:copy,held\n").unwrap();
    let fifo = dir.join("macros.conf");
    let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: c_path is a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

    let child = Command::new(env!("CARGO_BIN_EXE_razerctl"))
        .args(["remap", "--device", "no-such-device-dan-1476", "--config"]).arg(&conf).arg("--macros").arg(&fifo)
        .stderr(std::process::Stdio::piped())
        .spawn().unwrap();
    // Opening the write end returns only once the child has opened the read end, i.e. it is inside startup.
    let mut w = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();
    // SAFETY: the pid is our live, unreaped child.
    assert_eq!(unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGUSR1) }, 0);
    // The child is blocked reading the FIFO, so the signal has been delivered by the time it reads the macros.
    std::thread::sleep(std::time::Duration::from_millis(200));
    let _ = w.write_all(b"# whatever the file holds, the process must get there alive\n"); // EPIPE if the child died
    drop(w);

    let out = child.wait_with_output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.signal(), None, "killed by SIGUSR1 during startup: {}", stderr(&out));
    assert!(out.status.code().is_some(), "{}", stderr(&out));
}
