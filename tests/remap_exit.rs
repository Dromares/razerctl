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
