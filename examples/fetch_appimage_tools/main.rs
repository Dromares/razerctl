//! Pre-seeds cargo-packager's AppImage build tools from sha256-pinned downloads, so the third-party binaries that end up in the
//! shipped AppImage are the ones recorded in `tools.rs` and not whatever a moving release tag serves today (DAN-1627, DAN-1634).
//!
//!     cargo run --release --example fetch_appimage_tools -- cargo packager --release --formats appimage
//!
//! cargo-packager 0.11.x downloads AppRun, linuxdeploy and linuxdeploy-plugin-appimage into
//! `${XDG_CACHE_HOME:-~/.cache}/.cargo-packager/AppImage/` only when they are missing, and never checks them. The plugin runs
//! appimagetool, which downloads type2-runtime `continuous` unless `LDAI_RUNTIME_FILE` names one. This downloads each file to a
//! `.part`, checks its sha256, moves it into place only on a match, copies the packager's three into its tools directory, sets
//! `LDAI_RUNTIME_FILE` to the checked runtime and then replaces itself with the given command. On a mismatch it prints the expected
//! and the received hash, deletes the part file and exits 1 before anything is built. Dev-time only, like `gen_icons`.

mod sha256;
mod tools;

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{exit, Command};

fn main() {
    if std::env::consts::ARCH != tools::ARCH {
        eprintln!("fetch_appimage_tools: only {} is pinned", tools::ARCH);
        exit(2);
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| {
            eprintln!("fetch_appimage_tools: neither XDG_CACHE_HOME nor HOME is set");
            exit(2);
        });
    let runtime = tools::prepare(
        &tools::PINS,
        &root.join("target/appimage-tools"),
        &cache.join(".cargo-packager/AppImage"),
    )
    .unwrap_or_else(|e| {
        eprintln!("fetch_appimage_tools: {e}");
        exit(1);
    });
    eprintln!("tools verified; LDAI_RUNTIME_FILE={}", runtime.display());
    let mut cmd = std::env::args_os().skip(1);
    if let Some(prog) = cmd.next() {
        let err = Command::new(&prog)
            .args(cmd)
            .env("LDAI_RUNTIME_FILE", &runtime)
            .exec();
        eprintln!(
            "fetch_appimage_tools: cannot run {}: {err}",
            prog.to_string_lossy()
        );
        exit(127);
    }
}
