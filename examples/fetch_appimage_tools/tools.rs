//! The pinned AppImage build tools and the fetch-check-place step (DAN-1627, DAN-1634).

use super::sha256;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

pub const ARCH: &str = "x86_64";

pub struct Pin {
    pub name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    /// Copied into cargo-packager's tools directory. The runtime is not: it is handed to the plugin by `LDAI_RUNTIME_FILE`.
    pub preseed: bool,
}

/// Upstream tags are mutable (`continuous`, `linuxdeploy`, `apprun-old`); when one moves, the check fails closed. To bump a pin:
/// download the new file, inspect it, and replace its hash here and in the README table (`tests/appimage_tools.rs` compares the two).
pub const PINS: [Pin; 4] = [
    Pin {
        name: "AppRun-x86_64",
        url: "https://github.com/tauri-apps/binary-releases/releases/download/apprun-old/AppRun-x86_64",
        sha256: "f30140a43a0a59e46db21bdefdf749b9e9f2c6946e92afabbacf98b8ae73fb4f",
        preseed: true,
    },
    Pin {
        name: "linuxdeploy-x86_64.AppImage",
        url: "https://github.com/tauri-apps/binary-releases/releases/download/linuxdeploy/linuxdeploy-x86_64.AppImage",
        sha256: "e762bea85c8eb0d4b3508d46e5c1f037f717d0f9303ae3b4aafc8b04991fa1ef",
        preseed: true,
    },
    Pin {
        name: "linuxdeploy-plugin-appimage.AppImage",
        url: "https://github.com/linuxdeploy/linuxdeploy-plugin-appimage/releases/download/continuous/linuxdeploy-plugin-appimage-x86_64.AppImage",
        sha256: "49d6a17160675a6bd1781699aae6bdf7692d98552e02a3671d2183d10547842e",
        preseed: true,
    },
    Pin {
        name: "runtime-x86_64",
        url: "https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-x86_64",
        sha256: "156f4bdbde9c52d01814600013e0a273f0118dc2de98975f3c8c63427ec79074",
        preseed: false,
    },
];

pub fn sha256_of(path: &Path) -> io::Result<String> {
    sha256::hex_digest(fs::File::open(path)?)
}

fn has_hash(path: &Path, want: &str) -> bool {
    sha256_of(path).is_ok_and(|got| got == want)
}

/// Makes `dest` a file whose sha256 is `want`: keeps it if it already matches, otherwise downloads `url` to `dest.part`
/// and moves it into place only on a match. A mismatching `dest` is removed first and the part file is always deleted, so a
/// failure leaves nothing unverified behind. The error text carries both the expected and the received hash.
pub fn fetch_verified(url: &str, dest: &Path, want: &str) -> Result<(), String> {
    if has_hash(dest, want) {
        return Ok(());
    }
    let name = dest.file_name().unwrap_or_default().to_string_lossy();
    let _ = fs::remove_file(dest);
    let part = dest.with_file_name(format!("{name}.part"));
    eprintln!("fetching {name}");
    // curl does the TLS; std has no HTTPS client and the tree takes no HTTP dependency for a dev-time download.
    let curl = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--output",
        ])
        .arg(&part)
        .arg(url)
        .status()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    let result = if !curl.success() {
        Err(format!("curl failed for {url} ({curl})"))
    } else {
        match sha256_of(&part) {
            Ok(got) if got == want => Ok(()),
            Ok(got) => Err(format!(
                "{name} does not match the pinned sha256\n  expected {want}\n  got      {got}"
            )),
            Err(e) => Err(format!("cannot read {}: {e}", part.display())),
        }
    };
    match result {
        Ok(()) => fs::rename(&part, dest).map_err(|e| format!("{}: {e}", dest.display())),
        Err(e) => {
            let _ = fs::remove_file(&part);
            Err(e)
        }
    }
}

/// Verifies every pin into `dl`, and only then copies the pre-seeded ones into `tools` (fresh each run: the packager patches
/// linuxdeploy's ELF header in place). Returns the checked runtime's path.
pub fn prepare(pins: &[Pin], dl: &Path, tools: &Path) -> Result<std::path::PathBuf, String> {
    fs::create_dir_all(dl).map_err(|e| format!("{}: {e}", dl.display()))?;
    fs::create_dir_all(tools).map_err(|e| format!("{}: {e}", tools.display()))?;
    for pin in pins {
        let path = dl.join(pin.name);
        fetch_verified(pin.url, &path, pin.sha256).map_err(|e| format!("{}: {e}", pin.name))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    for pin in pins.iter().filter(|p| p.preseed) {
        let to = tools.join(pin.name);
        let tmp = tools.join(format!("{}.part", pin.name));
        fs::copy(dl.join(pin.name), &tmp)
            .and_then(|_| fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)))
            .and_then(|_| fs::rename(&tmp, &to))
            .map_err(|e| format!("{}: {e}", to.display()))?;
    }
    let runtime = pins
        .iter()
        .find(|p| !p.preseed)
        .ok_or("no runtime pin")?
        .name;
    Ok(dl.join(runtime))
}
