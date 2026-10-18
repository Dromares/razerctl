//! The `remap`, `buttons` and `macros` commands, shared by `razerctl` and `razerctl-gui` (DAN-1750). The AppImage starts the
//! GUI binary and nothing else, so the `razerctl-remap.service` unit it writes asks the GUI binary for `remap`, and the
//! GUI's start-up check asks it for `macros check`. Both binaries call the same code, so the exit status the unit's
//! `RestartPreventExitStatus=78` keys on cannot differ between them.

use crate::update::InstallType;
use crate::{macros, remap};
use std::path::{Path, PathBuf};

/// The macros `remap` needs: None if the mapping has no macro:NAME (the file isn't read),
/// else the loaded file with every referenced name present. Runs before any grab.
pub fn remap_macros(map: &remap::Mapping, file: &Path) -> Result<Option<macros::Macros>, String> {
    if !map
        .values()
        .any(|a| matches!(a, remap::Action::Macro { .. }))
    {
        return Ok(None);
    }
    if !file.exists() {
        return Err(format!(
            "{}: not found (the mapping uses macro:NAME; create it or pass --macros FILE)",
            file.display()
        ));
    }
    let m = macros::load_macros(file)?;
    remap::check_macros(map, &m).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(Some(m))
}

/// `macros [list|show NAME|check|play NAME --dry-run]`. Reads text files only, never a device.
/// A missing default file means "no macros"/"no remap rules"; a missing explicit one is an error.
pub fn macros_cmd(args: &[String]) -> Result<(), String> {
    let (mut file, mut cfg, mut dry, mut pos) =
        (None::<PathBuf>, None::<PathBuf>, false, Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--file" => file = Some(it.next().ok_or("--file needs a path")?.into()),
            "--config" => cfg = Some(it.next().ok_or("--config needs a path")?.into()),
            "--dry-run" => dry = true,
            o if o.starts_with('-') => return Err(format!("unexpected argument '{o}'")),
            o => pos.push(o),
        }
    }
    let sub = pos.first().copied().unwrap_or("list");
    if cfg.is_some() && sub != "check" {
        return Err("--config only applies to `macros check`".into());
    }
    if dry && sub != "play" {
        return Err("--dry-run only applies to `macros play`".into());
    }
    if sub == "play" && !dry {
        return Err("live playback only happens inside `razerctl remap` (bind it with BUTTON = macro:NAME in \
                    remap.conf), and testing it on the device needs the board's go-ahead; \
                    use `macros play NAME --dry-run` to print what would be sent"
            .into());
    }
    let explicit = file.is_some();
    let file = file.unwrap_or_else(macros::macros_path);
    if explicit && !file.exists() {
        return Err(format!("{}: not found", file.display()));
    }
    let m = macros::load_macros(&file)?;
    let get = |name: &str| {
        m.get(name)
            .ok_or_else(|| format!("no macro '{name}' in {}", file.display()))
    };
    match pos.as_slice() {
        [] | ["list"] => {
            if m.is_empty() { eprintln!("no macros in {}", file.display()); }
            for l in macros::describe_macros(&m) { println!("{l}"); }
        }
        ["show", name] => {
            let mac = get(name)?;
            let s = macros::serialize_macros(&macros::Macros::from([(mac.name.clone(), mac.clone())]));
            // drop the file header comment
            print!("{}", s.split_once("\n\n").map_or(s.as_str(), |(_, body)| body));
        }
        ["check"] => {
            println!("{}: {} macro{} OK", file.display(), m.len(), if m.len() == 1 { "" } else { "s" });
            let explicit = cfg.is_some();
            let cfg = cfg.unwrap_or_else(remap::config_path);
            let text = match std::fs::read_to_string(&cfg) {
                Ok(t) => t,
                Err(e) if !explicit && e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(format!("{}: {e}", cfg.display())),
            };
            let map = remap::parse_config(&text).map_err(|e| format!("{}: {e}", cfg.display()))?;
            remap::check_macros(&map, &m).map_err(|e| format!("{}: {e} (not in {})", cfg.display(), file.display()))?;
            let n = map.values().filter(|a| matches!(a, remap::Action::Macro { .. })).count();
            println!("{}: {n} macro reference{} OK", cfg.display(), if n == 1 { "" } else { "s" });
        }
        ["play", name] => {
            let mac = get(name)?;
            eprintln!("{name}: {} events, {} ms (dry run, nothing is sent)", mac.events.len(), macros::play_ms(&mac.events));
            for l in macros::describe_steps(&macros::to_steps(&macros::pad(&mac.events))) { println!("{l}"); }
        }
        _ => return Err("usage: macros [list | show NAME | check [--config FILE] | play NAME --dry-run] [--file FILE]".into()),
    }
    Ok(())
}

/// Exit status of `remap` when the config is the problem (no rules, a rule that does not parse, an undefined macro).
/// Retrying cannot fix those, so the unit lists it in `RestartPreventExitStatus=` (`systemd/razerctl-remap.service`)
/// and stops instead of crash-looping; every other failure is exit 1 and is retried. 78 is sysexits' EX_CONFIG.
pub const EXIT_CONFIG: u8 = 78;

/// A command's failure: the message for stderr and the exit status.
pub struct Fail {
    pub msg: String,
    pub code: u8,
}

impl Fail {
    pub fn config(msg: impl Into<String>) -> Fail {
        Fail {
            msg: msg.into(),
            code: EXIT_CONFIG,
        }
    }
}

/// Any other error text is a plain exit-1 failure, so `?` on a `String` / `&str` error keeps working.
impl<S: Into<String>> From<S> for Fail {
    fn from(msg: S) -> Fail {
        Fail {
            msg: msg.into(),
            code: 1,
        }
    }
}

/// The AppImage cannot install the udev rule itself, so a permission error names the line that does (README, "Install, AppImage").
pub const APPIMAGE_UDEV_LINE: &str = "sudo curl -fLo /etc/udev/rules.d/70-razerctl.rules https://github.com/Dromares/razerctl/releases/latest/download/70-razerctl.rules && sudo udevadm control --reload && sudo udevadm trigger";

/// What to do about a permission error on the mouse's input nodes or `/dev/uinput`, for how this copy was installed.
/// Every format needs the udev rule once; the AppImage is the one with no `install.sh` to say so, and its remedy is a line.
pub fn access_hint(install: &InstallType) -> String {
    match install {
        InstallType::AppImage { .. } => {
            format!("install the udev rule with `{APPIMAGE_UDEV_LINE}`, then replug the mouse")
        }
        InstallType::Tarball | InstallType::Package => {
            "install udev/70-razerctl.rules (see README, \"The udev rule\"), then replug the mouse"
                .into()
        }
    }
}

/// `buttons` (print button names) or `remap` (grab the mouse and rewrite buttons). Every config error is found before
/// anything is grabbed and exits [`EXIT_CONFIG`].
pub fn remap_command(command: &str, rest: &[String]) -> Result<(), Fail> {
    let mut inline = Vec::new();
    let mut cfg = remap::config_path();
    let mut filter: Option<String> = None;
    let mut mfile = macros::macros_path();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--map" => inline.push(it.next().ok_or("--map needs FROM=TO")?.clone()),
            "--config" => cfg = it.next().ok_or("--config needs a path")?.into(),
            "--device" => filter = Some(it.next().ok_or("--device needs a name")?.clone()),
            "--macros" if command == "remap" => {
                mfile = it.next().ok_or("--macros needs a path")?.into()
            }
            o => return Err(format!("unexpected argument '{o}'").into()),
        }
    }
    let sources = || -> Result<Vec<remap::Source>, String> {
        let s = remap::find_sources(filter.as_deref()).map_err(|e| e.to_string())?;
        if s.is_empty() {
            return Err(format!(
                "no readable Razer input devices in /dev/input (is the mouse plugged in? if it is, {})",
                access_hint(&InstallType::detect())
            ));
        }
        Ok(s)
    };
    if command == "buttons" {
        let sources = sources()?;
        for s in &sources {
            eprintln!("watching {} ({})", s.path.display(), s.name);
        }
        eprintln!("press buttons; Ctrl+C to stop");
        return remap::watch(&sources).map_err(|e| e.to_string().into());
    }
    // a Stop all macros (SIGUSR1) during startup must not kill the service before it can handle it
    if let Err(e) = remap::install_stop_all_handler() {
        eprintln!("SIGUSR1 (stop all macros) unavailable: {e}");
    }
    // every config error before anything is grabbed
    let map = remap::load_mapping(&cfg, &inline).map_err(Fail::config)?;
    if map.is_empty() {
        return Err(Fail::config("no remap rules"));
    }
    let macros = remap_macros(&map, &mfile).map_err(Fail::config)?;
    for (k, v) in &map {
        eprintln!("  {k:?} -> {v:?}");
    }
    let sources = sources()?;
    match macros {
        Some(m) => remap::run_with_macros(sources, map, m),
        None => remap::run(sources, map),
    }
    .map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            Fail::from(format!(
                "{e}: no permission for an input node or /dev/uinput; {}",
                access_hint(&InstallType::detect())
            ))
        } else {
            Fail::from(e.to_string())
        }
    })
}

/// Print a failure the way every razerctl command does and return its exit status.
pub fn report(result: Result<(), Fail>) -> u8 {
    match result {
        Ok(()) => 0,
        Err(f) => {
            eprintln!("razerctl: {}", f.msg);
            f.code
        }
    }
}

/// The commands the GUI binary answers instead of opening a window, so the AppImage (which starts the GUI and nothing
/// else) can run its own `razerctl-remap.service` and answer `macros check`. `Some(exit status)` when `args` was one of them.
pub fn embedded(args: &[String]) -> Option<u8> {
    let (command, rest) = args.split_first()?;
    match command.as_str() {
        "remap" => Some(report(remap_command("remap", rest))),
        "macros" => Some(report(macros_cmd(rest).map_err(Fail::from))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remap_reads_macros_only_when_mapped_and_checks_names() {
        let dir = std::env::temp_dir().join(format!("razerctl-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("macros.conf");
        let plain = remap::parse_config("back = ctrl+c").unwrap();
        let uses = remap::parse_config("back = macro:copy,held").unwrap();
        // no macro actions: the file is never needed
        assert_eq!(remap_macros(&plain, &file), Ok(None));
        assert!(remap_macros(&uses, &file)
            .unwrap_err()
            .contains("not found"));
        std::fs::write(&file, "[other]\ntap a\n").unwrap();
        assert!(remap_macros(&uses, &file)
            .unwrap_err()
            .ends_with("undefined macro 'copy'"));
        std::fs::write(&file, "[copy]\ndown a\n").unwrap();
        assert!(remap_macros(&uses, &file)
            .unwrap_err()
            .contains("leaves KEY_A pressed"));
        std::fs::write(&file, "[copy]\ntap ctrl\n").unwrap();
        assert_eq!(remap_macros(&uses, &file).unwrap().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_remap_and_macros_are_answered_by_the_gui_binary() {
        let a = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(embedded(&[]), None);
        assert_eq!(embedded(&a(&["battery-watch"])), None);
        assert_eq!(embedded(&a(&["--version"])), None);
        // a bad argument is an ordinary failure (exit 1), found before any file or device is looked at
        assert_eq!(embedded(&a(&["remap", "--nonsense"])), Some(1));
    }

    #[test]
    fn the_permission_hint_names_the_udev_line_for_an_appimage_and_never_install_sh() {
        let appimage = access_hint(&InstallType::AppImage {
            path: "/home/u/Apps/razerctl.AppImage".into(),
        });
        assert!(
            appimage.contains("sudo curl -fLo /etc/udev/rules.d/70-razerctl.rules"),
            "{appimage}"
        );
        for install in [InstallType::Tarball, InstallType::Package] {
            let h = access_hint(&install);
            assert!(
                h.contains("70-razerctl.rules") && !h.contains("sudo curl"),
                "{h}"
            );
        }
        assert!(!appimage.contains("install.sh"));
    }
}
