//! The AUR `razerctl-bin` recipe in `packaging/aur/razerctl-bin` (DAN-1670). The AUR builds from `.SRCINFO`, not from
//! the PKGBUILD, so a bump that edits one and forgets to regenerate the other publishes the old version. This pins the
//! two together and the parts of the recipe a user relies on.

use std::path::Path;

fn read(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("packaging/aur/razerctl-bin")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// `key=value` from a PKGBUILD, without quotes or the surrounding `( )`.
fn pkgbuild(src: &str, key: &str) -> String {
    let prefix = format!("{key}=");
    let line = src
        .lines()
        .find(|l| l.starts_with(&prefix))
        .unwrap_or_else(|| panic!("PKGBUILD has no {key}="));
    line[prefix.len()..]
        .trim_matches(|c| matches!(c, '(' | ')' | '\'' | '"'))
        .to_string()
}

/// `key = value` from a .SRCINFO (the first one).
fn srcinfo(src: &str, key: &str) -> String {
    src.lines()
        .filter_map(|l| l.trim().split_once(" = "))
        .find(|(k, _)| *k == key)
        .unwrap_or_else(|| panic!(".SRCINFO has no {key}"))
        .1
        .to_string()
}

#[test]
fn srcinfo_matches_the_pkgbuild() {
    let (pkgbuild_src, info) = (read("PKGBUILD"), read(".SRCINFO"));
    for key in ["pkgver", "pkgrel"] {
        assert_eq!(
            pkgbuild(&pkgbuild_src, key),
            srcinfo(&info, key),
            "{key}: regenerate .SRCINFO with `makepkg --printsrcinfo > .SRCINFO`"
        );
    }
    assert_eq!(
        pkgbuild(&pkgbuild_src, "sha256sums"),
        srcinfo(&info, "sha256sums"),
        "sha256sums differ: regenerate .SRCINFO"
    );
    let ver = pkgbuild(&pkgbuild_src, "pkgver");
    let url = format!("releases/download/v{ver}/razerctl-{ver}-x86_64-linux.tar.gz");
    assert!(
        srcinfo(&info, "source").ends_with(&url),
        ".SRCINFO source must end with {url}"
    );
}

#[test]
fn replaces_razerctl_and_installs_the_udev_rule() {
    let (pkgbuild_src, info) = (read("PKGBUILD"), read(".SRCINFO"));
    assert_eq!(srcinfo(&info, "provides"), "razerctl");
    assert_eq!(srcinfo(&info, "conflicts"), "razerctl");
    assert!(
        pkgbuild_src.contains("/usr/lib/udev/rules.d/70-razerctl.rules"),
        "the udev rule must be packaged"
    );
    assert!(
        pkgbuild_src.contains("/usr/share/licenses/$pkgname/LICENSE"),
        "the licence must be packaged"
    );
}

#[test]
fn install_message_tells_the_user_to_replug() {
    let msg = read("razerctl-bin.install");
    assert!(msg.contains("unplug and replug"), "{msg}");
    assert!(
        msg.contains("sudo udevadm control --reload && sudo udevadm trigger"),
        "{msg}"
    );
}
