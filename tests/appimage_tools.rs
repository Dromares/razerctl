//! The AppImage tool pins (DAN-1627, DAN-1634): `examples/fetch_appimage_tools` is the only code that downloads them, so the
//! hash check itself is tested here, and the README table must hold the same four values as the code.

#[path = "../examples/fetch_appimage_tools/sha256.rs"]
mod sha256;
#[path = "../examples/fetch_appimage_tools/tools.rs"]
mod tools;

use std::path::{Path, PathBuf};

fn hex(bytes: &[u8]) -> String {
    sha256::hex_digest(bytes).unwrap()
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("razerctl-appimage-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn sha256_matches_the_fips_vectors() {
    assert_eq!(
        hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    // Crosses many 64 KiB reads and block boundaries: one million 'a'.
    assert_eq!(
        hex(&vec![b'a'; 1_000_000]),
        "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
    );
}

#[test]
fn readme_table_holds_the_same_pins_as_the_code() {
    let readme = include_str!("../README.md");
    let in_readme: Vec<&str> = readme
        .lines()
        .filter(|l| l.starts_with("| `"))
        .flat_map(|l| l.split('`'))
        .filter(|t| t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()))
        .collect();
    let in_code: Vec<&str> = tools::PINS.iter().map(|p| p.sha256).collect();
    assert_eq!(in_readme, in_code, "README pin table and PINS drifted");
}

fn file_url(p: &Path) -> String {
    format!("file://{}", p.display())
}

#[test]
fn matching_download_lands_and_leaves_no_part_file() {
    let dir = scratch("ok");
    let src = dir.join("src");
    std::fs::write(&src, b"abc").unwrap();
    let dest = dir.join("tool");
    let want = hex(b"abc");
    tools::fetch_verified(&file_url(&src), &dest, &want).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"abc");
    assert!(!dir.join("tool.part").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mismatch_fails_closed_with_expected_and_got_and_nothing_in_place() {
    let dir = scratch("bad");
    let src = dir.join("src");
    std::fs::write(&src, b"abc").unwrap();
    let dest = dir.join("tool");
    // A stale file already at the destination must not survive a failed check either.
    std::fs::write(&dest, b"stale").unwrap();
    let want = hex(b"not abc");
    let err = tools::fetch_verified(&file_url(&src), &dest, &want).unwrap_err();
    assert!(err.contains(&format!("expected {want}")), "{err}");
    assert!(err.contains(&format!("got      {}", hex(b"abc"))), "{err}");
    assert!(!dest.exists(), "unverified file left in place");
    assert!(!dir.join("tool.part").exists(), "part file left behind");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn failed_download_fails_closed() {
    let dir = scratch("404");
    let dest = dir.join("tool");
    let err =
        tools::fetch_verified(&file_url(&dir.join("missing")), &dest, &hex(b"abc")).unwrap_err();
    assert!(err.contains("curl failed"), "{err}");
    assert!(!dest.exists());
    assert!(!dir.join("tool.part").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prepare_places_nothing_in_the_tools_dir_when_any_pin_fails() {
    let dir = scratch("prepare");
    let good = dir.join("good");
    std::fs::write(&good, b"abc").unwrap();
    let url: &'static str = Box::leak(file_url(&good).into_boxed_str());
    let ok: &'static str = Box::leak(hex(b"abc").into_boxed_str());
    let pins = [
        tools::Pin {
            name: "first",
            url,
            sha256: ok,
            preseed: true,
        },
        tools::Pin {
            name: "second",
            url,
            sha256: "0".repeat(64).leak(),
            preseed: true,
        },
        tools::Pin {
            name: "runtime",
            url,
            sha256: ok,
            preseed: false,
        },
    ];
    let (dl, tooldir) = (dir.join("dl"), dir.join("tools"));
    assert!(tools::prepare(&pins, &dl, &tooldir).is_err());
    assert_eq!(std::fs::read_dir(&tooldir).unwrap().count(), 0);

    let pins = [
        tools::Pin {
            name: "first",
            url,
            sha256: ok,
            preseed: true,
        },
        tools::Pin {
            name: "runtime",
            url,
            sha256: ok,
            preseed: false,
        },
    ];
    let runtime = tools::prepare(&pins, &dl, &tooldir).unwrap();
    assert_eq!(runtime, dl.join("runtime"));
    assert!(tooldir.join("first").exists());
    assert!(!tooldir.join("runtime").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_pinned_url_is_the_x86_64_file() {
    for pin in &tools::PINS {
        assert!(
            pin.url.ends_with(tools::ARCH)
                || pin.url.ends_with(&format!("{}.AppImage", tools::ARCH)),
            "{}",
            pin.url
        );
        assert_eq!(pin.sha256.len(), 64, "{}", pin.name);
    }
}

#[test]
fn update_info_points_at_this_repos_zsync_asset() {
    // appimagetool `-u` takes `gh-releases-zsync|owner|repo|release|asset-glob`; the release workflow publishes
    // `razerctl-<version>-x86_64.AppImage.zsync` (DAN-1668), and owner and repo must be the ones in Cargo.toml.
    let cargo =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    let repo = cargo
        .lines()
        .find_map(|l| l.strip_prefix("repository = \"https://github.com/"))
        .and_then(|r| r.strip_suffix('"'))
        .expect("repository in Cargo.toml");
    let (owner, name) = repo.split_once('/').unwrap();
    assert_eq!(
        tools::UPDATE_INFO,
        format!("gh-releases-zsync|{owner}|{name}|latest|razerctl-*-x86_64.AppImage.zsync")
    );
}

#[test]
fn the_example_hands_the_update_info_to_the_command_it_runs() {
    // The constant alone embeds nothing: appimagetool reads it from the environment of the command `main` execs.
    let main = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/fetch_appimage_tools/main.rs"),
    )
    .unwrap();
    assert!(main.contains(r#".env("LDAI_UPDATE_INFORMATION", tools::UPDATE_INFO)"#));
}
