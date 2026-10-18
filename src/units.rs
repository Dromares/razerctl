//! The low-battery notifier's user unit for an AppImage (DAN-1746, DAN-1747).
//!
//! `install.sh` and the PKGBUILD put `systemd/razerctl-battery.service` in place; the AppImage installs nothing, and the
//! shipped unit runs `~/.local/bin/razerctl`, which an AppImage user does not have. So when the GUI runs as an AppImage
//! and the user turns the notifier on, it writes its own copy of the unit with `ExecStart=` pointing at the AppImage file
//! (`razerctl-gui battery-watch`; the GUI binary answers that argument, see `razerctl-gui.rs`).
//!
//! The decisions are pure ([`plan_enable`], [`plan_disable`]) and take what is on disk as arguments. Only a unit that
//! carries [`MARKER`] on its first line is ours to rewrite or remove; a unit from `install.sh`, a package, or a person is
//! never touched. Non-AppImage installs are [`Plan::NotApplicable`]: they keep calling `systemctl` on the unit they
//! already have.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::battery;
use crate::update::InstallType;

/// The shipped unit. The AppImage copy is this text with the `ExecStart=` line swapped, so the two cannot drift apart.
const TEMPLATE: &str = include_str!("../systemd/razerctl-battery.service");

/// First line of a unit this module wrote. Its presence is what makes a unit ours.
pub const MARKER: &str = "# razerctl-appimage-unit: written by the razerctl AppImage";

/// What to do about the unit file before calling `systemctl enable --now`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Not an AppImage: use the unit the install already has.
    NotApplicable,
    /// Our unit is there and already names this AppImage.
    UpToDate,
    /// Write this text. `replaces` is true when an older copy of our unit is being rewritten (the AppImage moved), so a
    /// running service must be restarted onto the new path.
    Write { text: String, replaces: bool },
    /// Something else owns the unit; leave the file alone and enable it as it is.
    LeaveAlone(Owner),
}

/// Why a unit is left alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// A package or system directory has it (`/usr/lib/systemd/user`, `/etc/systemd/user`).
    Packaged,
    /// A unit in `~/.config/systemd/user` that is not marked as ours: `install.sh`, a tarball copy, or hand-made.
    Unmarked,
}

/// What to do about the unit file when the notifier is turned off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisablePlan {
    /// Not an AppImage: `systemctl disable --now` the unit the install has.
    NotApplicable,
    /// Our unit: `disable --now`, then delete the file.
    Remove,
    /// Someone else's unit: `disable --now`, keep the file.
    LeaveAlone(Owner),
    /// No unit exists, so there is nothing running and nothing to disable.
    NothingInstalled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnitError {
    /// `$APPIMAGE` is not an absolute path, so a unit started by systemd could not find it.
    RelativePath(PathBuf),
    /// A control character in the path cannot be put in a unit file.
    BadPath(PathBuf),
}

impl fmt::Display for UnitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnitError::RelativePath(p) => write!(
                f,
                "the AppImage path {} is not absolute, so a systemd unit cannot start it",
                p.display()
            ),
            UnitError::BadPath(p) => write!(
                f,
                "the AppImage path {p:?} has a control character a systemd unit cannot hold"
            ),
        }
    }
}

/// One `ExecStart=` word: bare when it only has characters systemd reads literally, else double-quoted with `\`, `"`,
/// `%` (specifier) and `$` (variable) escaped.
fn exec_word(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._+-:@=,".contains(c));
    if plain {
        return s.to_string();
    }
    let mut out = String::from('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The unit text for an AppImage at `appimage`: the shipped unit with the marker on top and `ExecStart=` running the
/// AppImage's `battery-watch`.
pub fn appimage_unit_text(appimage: &Path) -> Result<String, UnitError> {
    let s = appimage.to_string_lossy();
    if s.chars().any(char::is_control) {
        return Err(UnitError::BadPath(appimage.to_path_buf()));
    }
    if !appimage.is_absolute() {
        return Err(UnitError::RelativePath(appimage.to_path_buf()));
    }
    let exec = format!("ExecStart={} battery-watch", exec_word(&s));
    let mut out = format!("{MARKER}\n# Turning the notifier off in razerctl removes this file.\n");
    for line in TEMPLATE.lines() {
        out.push_str(if line.starts_with("ExecStart=") {
            &exec
        } else {
            line
        });
        out.push('\n');
    }
    Ok(out)
}

fn is_ours(existing: &str) -> bool {
    existing.lines().next().is_some_and(|l| l == MARKER)
}

/// Before `enable --now`. `existing` is `~/.config/systemd/user/razerctl-battery.service` if it is there; `packaged` says
/// whether a system directory has the unit.
pub fn plan_enable(
    install: &InstallType,
    existing: Option<&str>,
    packaged: bool,
) -> Result<Plan, UnitError> {
    let InstallType::AppImage { path } = install else {
        return Ok(Plan::NotApplicable);
    };
    Ok(match existing {
        Some(old) if !is_ours(old) => Plan::LeaveAlone(Owner::Unmarked),
        None if packaged => Plan::LeaveAlone(Owner::Packaged),
        old => {
            let text = appimage_unit_text(path)?;
            match old {
                Some(old) if old == text => Plan::UpToDate,
                old => Plan::Write {
                    text,
                    replaces: old.is_some(),
                },
            }
        }
    })
}

/// Before `disable --now`.
pub fn plan_disable(install: &InstallType, existing: Option<&str>, packaged: bool) -> DisablePlan {
    if !matches!(install, InstallType::AppImage { .. }) {
        return DisablePlan::NotApplicable;
    }
    match existing {
        Some(old) if is_ours(old) => DisablePlan::Remove,
        Some(_) => DisablePlan::LeaveAlone(Owner::Unmarked),
        None if packaged => DisablePlan::LeaveAlone(Owner::Packaged),
        None => DisablePlan::NothingInstalled,
    }
}

/// Where the unit lives and where a package would have put it.
#[derive(Clone, Debug)]
pub struct Site {
    /// `$XDG_CONFIG_HOME/systemd/user` (else `~/.config/systemd/user`).
    pub user_dir: PathBuf,
    /// Directories a package or the system owns, searched for the unit.
    pub packaged_dirs: Vec<PathBuf>,
}

impl Site {
    pub fn from_env() -> Site {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
            });
        Site {
            user_dir: base.join("systemd/user"),
            packaged_dirs: ["/usr/lib/systemd/user", "/etc/systemd/user"]
                .map(PathBuf::from)
                .to_vec(),
        }
    }

    pub fn unit_path(&self) -> PathBuf {
        self.user_dir.join(battery::SERVICE)
    }

    /// The user unit's text, if there is one.
    pub fn existing(&self) -> Option<String> {
        std::fs::read_to_string(self.unit_path()).ok()
    }

    pub fn packaged(&self) -> bool {
        self.packaged_dirs
            .iter()
            .any(|d| d.join(battery::SERVICE).exists())
    }

    /// Write `text` as the unit, through a temporary file so a reader never sees half of it.
    pub fn write(&self, text: &str) -> io::Result<()> {
        std::fs::create_dir_all(&self.user_dir)?;
        let path = self.unit_path();
        let tmp = path.with_extension("service.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)
    }

    pub fn remove(&self) -> io::Result<()> {
        match std::fs::remove_file(self.unit_path()) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPIMAGE: &str = "/home/u/Apps/razerctl-0.3.0-x86_64.AppImage";

    fn appimage(p: &str) -> InstallType {
        InstallType::AppImage { path: p.into() }
    }

    fn exec_line(text: &str) -> &str {
        text.lines()
            .find(|l| l.starts_with("ExecStart="))
            .expect("an ExecStart line")
    }

    #[test]
    fn the_appimage_unit_runs_battery_watch_from_the_appimage_and_keeps_the_rest_of_the_shipped_unit(
    ) {
        let text = appimage_unit_text(Path::new(APPIMAGE)).unwrap();
        assert_eq!(
            exec_line(&text),
            format!("ExecStart={APPIMAGE} battery-watch")
        );
        assert_eq!(text.lines().next(), Some(MARKER));
        assert!(
            !text.contains(".local/bin"),
            "the tarball path must be gone:\n{text}"
        );
        // every other shipped line survives, in order
        let kept: Vec<&str> = text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.starts_with("ExecStart="))
            .collect();
        let shipped: Vec<&str> = TEMPLATE
            .lines()
            .filter(|l| !l.starts_with("ExecStart="))
            .collect();
        assert_eq!(kept, shipped);
        assert_eq!(TEMPLATE.matches("ExecStart=").count(), 1);
    }

    #[test]
    fn paths_with_spaces_and_systemd_specials_are_quoted() {
        let text = appimage_unit_text(Path::new("/home/u/My Apps/100%/a$b\"c.AppImage")).unwrap();
        assert_eq!(
            exec_line(&text),
            r#"ExecStart="/home/u/My Apps/100%%/a$$b\"c.AppImage" battery-watch"#
        );
    }

    #[test]
    fn a_relative_or_control_character_path_is_an_error_not_a_unit() {
        assert_eq!(
            appimage_unit_text(Path::new("razerctl.AppImage")),
            Err(UnitError::RelativePath("razerctl.AppImage".into()))
        );
        assert!(matches!(
            appimage_unit_text(Path::new("/tmp/a\nExecStop=/bin/true")),
            Err(UnitError::BadPath(_))
        ));
        assert!(plan_enable(&appimage("rel.AppImage"), None, false).is_err());
    }

    #[test]
    fn a_fresh_appimage_gets_the_unit_written() {
        let want = appimage_unit_text(Path::new(APPIMAGE)).unwrap();
        assert_eq!(
            plan_enable(&appimage(APPIMAGE), None, false),
            Ok(Plan::Write {
                text: want,
                replaces: false
            })
        );
    }

    #[test]
    fn the_same_path_is_up_to_date_so_updates_in_place_do_not_rewrite_it() {
        let have = appimage_unit_text(Path::new(APPIMAGE)).unwrap();
        assert_eq!(
            plan_enable(&appimage(APPIMAGE), Some(&have), false),
            Ok(Plan::UpToDate)
        );
    }

    #[test]
    fn a_moved_appimage_rewrites_our_unit_to_the_new_path() {
        let old = appimage_unit_text(Path::new(APPIMAGE)).unwrap();
        let moved = "/home/u/Downloads/razerctl-0.3.0-x86_64.AppImage";
        let Ok(Plan::Write { text, replaces }) = plan_enable(&appimage(moved), Some(&old), false)
        else {
            panic!("a moved AppImage must rewrite the unit");
        };
        assert!(replaces);
        assert_eq!(exec_line(&text), format!("ExecStart={moved} battery-watch"));
        assert!(!text.contains("/Apps/"));
    }

    #[test]
    fn a_unit_from_install_sh_or_a_package_is_never_clobbered() {
        let tarball = TEMPLATE; // what install.sh copies: ExecStart=%h/.local/bin/razerctl battery-watch
        let usr = TEMPLATE.replace("%h/.local/bin/razerctl", "/usr/bin/razerctl");
        for existing in [tarball, usr.as_str(), "[Service]\nExecStart=/opt/x\n", ""] {
            assert_eq!(
                plan_enable(&appimage(APPIMAGE), Some(existing), false),
                Ok(Plan::LeaveAlone(Owner::Unmarked)),
                "{existing:?}"
            );
        }
        // a unit under /usr/lib/systemd/user with nothing in ~/.config
        assert_eq!(
            plan_enable(&appimage(APPIMAGE), None, true),
            Ok(Plan::LeaveAlone(Owner::Packaged))
        );
    }

    #[test]
    fn other_install_types_keep_todays_behaviour() {
        for install in [InstallType::Tarball, InstallType::Package] {
            assert_eq!(plan_enable(&install, None, false), Ok(Plan::NotApplicable));
            assert_eq!(
                plan_disable(&install, Some(TEMPLATE), false),
                DisablePlan::NotApplicable
            );
        }
    }

    #[test]
    fn turning_it_off_removes_only_our_unit() {
        let ours = appimage_unit_text(Path::new(APPIMAGE)).unwrap();
        let ai = appimage(APPIMAGE);
        assert_eq!(plan_disable(&ai, Some(&ours), false), DisablePlan::Remove);
        assert_eq!(
            plan_disable(&ai, Some(TEMPLATE), false),
            DisablePlan::LeaveAlone(Owner::Unmarked)
        );
        assert_eq!(
            plan_disable(&ai, None, true),
            DisablePlan::LeaveAlone(Owner::Packaged)
        );
        assert_eq!(
            plan_disable(&ai, None, false),
            DisablePlan::NothingInstalled
        );
    }

    #[test]
    fn site_writes_rewrites_and_removes_the_unit_file() {
        let dir = std::env::temp_dir().join(format!("razerctl-units-{}", std::process::id()));
        let site = Site {
            user_dir: dir.join("systemd/user"),
            packaged_dirs: vec![dir.join("usr-lib")],
        };
        assert_eq!(site.existing(), None);
        assert!(!site.packaged());
        site.write("one\n").unwrap();
        site.write("two\n").unwrap();
        assert_eq!(site.existing().as_deref(), Some("two\n"));
        assert!(!site.user_dir.join("razerctl-battery.service.tmp").exists());
        site.remove().unwrap();
        site.remove().unwrap(); // already gone is fine
        assert_eq!(site.existing(), None);
        std::fs::create_dir_all(dir.join("usr-lib")).unwrap();
        std::fs::write(dir.join("usr-lib/razerctl-battery.service"), "x").unwrap();
        assert!(site.packaged());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
