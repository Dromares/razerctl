//! How this copy of razerctl was installed, which decides what an update may do (board decision D3, DAN-1665): an AppImage can
//! replace itself after the user agrees, a package-managed binary is never touched, anything else is told where to download.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallType {
    /// Started through an AppImage; `path` is the `.AppImage` file itself (`$APPIMAGE`), not the squashfs mount.
    AppImage { path: PathBuf },
    /// The binary is under `/usr/` (pacman and friends own it). Notify only, and off by default.
    Package,
    /// A release tarball, `install.sh` (`~/.local/bin`) or a hand-copied binary. Notify only, with a link.
    Tarball,
}

impl InstallType {
    /// What the install type implies when the user has not chosen: package-managed copies stay quiet.
    pub fn check_by_default(&self) -> bool {
        !matches!(self, InstallType::Package)
    }

    /// The install type of the running process.
    pub fn detect() -> InstallType {
        let exe = std::env::current_exe().unwrap_or_default();
        classify(std::env::var_os("APPIMAGE").as_deref(), &exe)
    }
}

/// `appimage` is `$APPIMAGE` and wins over the path: inside an AppImage the executable lives in `/tmp/.mount_…/usr/bin`, which
/// would otherwise read as package-managed. `/usr/local/` is not a package manager's: pacman never installs there, so a binary
/// copied there by hand is a tarball install.
pub fn classify(appimage: Option<&OsStr>, exe: &Path) -> InstallType {
    match appimage {
        Some(p) if !p.is_empty() => InstallType::AppImage { path: p.into() },
        _ if exe.starts_with("/usr/") && !exe.starts_with("/usr/local/") => InstallType::Package,
        _ => InstallType::Tarball,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(appimage: Option<&str>, exe: &str) -> InstallType {
        classify(appimage.map(OsStr::new), Path::new(exe))
    }

    #[test]
    fn appimage_env_wins_even_though_the_mount_has_usr_in_its_path() {
        let want = InstallType::AppImage {
            path: "/home/u/Apps/razerctl-0.2.0-x86_64.AppImage".into(),
        };
        let a = "/home/u/Apps/razerctl-0.2.0-x86_64.AppImage";
        assert_eq!(class(Some(a), "/tmp/.mount_razerckAbc/usr/bin/razerctl-gui"), want);
        assert_eq!(class(Some(a), "/usr/bin/razerctl"), want);
    }

    #[test]
    fn usr_is_package_managed_and_everything_else_is_a_tarball() {
        assert_eq!(class(None, "/usr/bin/razerctl-gui"), InstallType::Package);
        assert_eq!(class(None, "/usr/lib/razerctl/razerctl"), InstallType::Package);
        assert_eq!(class(None, "/home/u/.local/bin/razerctl"), InstallType::Tarball);
        assert_eq!(class(None, "/opt/razerctl/razerctl"), InstallType::Tarball);
        assert_eq!(class(None, "/usr/local/bin/razerctl"), InstallType::Tarball);
        assert_eq!(class(None, "/usrx/bin/razerctl"), InstallType::Tarball, "a prefix, not a path component");
        assert_eq!(class(None, ""), InstallType::Tarball);
    }

    #[test]
    fn an_empty_appimage_variable_is_not_an_appimage() {
        assert_eq!(class(Some(""), "/home/u/bin/razerctl"), InstallType::Tarball);
        assert_eq!(class(Some(""), "/usr/bin/razerctl"), InstallType::Package);
    }

    #[test]
    fn only_a_package_install_starts_with_the_check_off() {
        assert!(InstallType::AppImage { path: "/a".into() }.check_by_default());
        assert!(InstallType::Tarball.check_by_default());
        assert!(!InstallType::Package.check_by_default());
    }
}
