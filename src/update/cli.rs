//! `razerctl update --check` and `razerctl update` (DAN-1669). The CLI never checks on its own: this runs only when the user
//! types it, makes one request, and keeps no memory of it (the 24 hour throttle belongs to the GUI's background check).
//! `update` asks y/N before it downloads anything, and only an AppImage is ever replaced.

use super::{apply_appimage, check, fallback_dir, Applied, CheckResult, InstallType, Release, Source, Stage, Version};
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::atomic::AtomicBool;

pub const USAGE: &str = "usage: update [--check]";

/// `args` are what follows `update`. An update being available is not a failure; a check that could not be made is.
pub fn run(
    args: &[String],
    src: &Source,
    current: &Version,
    install: &InstallType,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<(), String> {
    let only_check = match args {
        [] => false,
        [a] if a == "--check" => true,
        _ => return Err(USAGE.into()),
    };
    let w = |e: std::io::Error| e.to_string();
    writeln!(out, "razerctl {current}").map_err(w)?;
    let rel = match check(src, current).0 {
        CheckResult::Failed(e) => return Err(format!("couldn't check for updates: {e}")),
        CheckResult::UpToDate(_) => return writeln!(out, "You are up to date.").map_err(w),
        CheckResult::Available(r) => r,
    };
    writeln!(out, "razerctl {} is available ({}).", rel.version, rel.page).map_err(w)?;
    if only_check {
        return Ok(());
    }
    let notes = rel.notes_text();
    if !notes.is_empty() {
        writeln!(out, "\nWhat's new:\n{notes}\n").map_err(w)?;
    }
    match install {
        InstallType::Package => writeln!(
            out,
            "razerctl was installed by your package manager, so it is updated there (yay -Syu or paru -Syu). Nothing was changed."
        )
        .map_err(w),
        InstallType::Tarball => writeln!(
            out,
            "razerctl was installed from a tarball or with install.sh, so you update it by hand. Download it from {}",
            rel.page
        )
        .map_err(w),
        InstallType::AppImage { path } => {
            write!(
                out,
                "Download {} and replace {}? The old file is kept as {}.old. [y/N] ",
                rel.appimage_name(),
                path.display(),
                path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
            )
            .map_err(w)?;
            out.flush().map_err(w)?;
            let mut answer = String::new();
            input.read_line(&mut answer).map_err(w)?;
            if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
                return writeln!(out, "Nothing was changed.").map_err(w);
            }
            apply(src, &rel, path, out)
        }
    }
}

fn apply(src: &Source, rel: &Release, target: &Path, out: &mut dyn Write) -> Result<(), String> {
    let cancel = AtomicBool::new(false);
    let mut last = None;
    let mut progress = |s: Stage| {
        let word = match s {
            Stage::Downloading { .. } => "Downloading",
            Stage::Verifying => "Checking it against SHA256SUMS",
            Stage::Replacing => "Replacing the file",
        };
        if last != Some(word) {
            last = Some(word);
            let _ = writeln!(out, "{word}...");
        }
    };
    match apply_appimage(src, rel, target, &fallback_dir(), &cancel, &mut progress) {
        Ok(Applied::Replaced { path, old }) => {
            let kept = old.map(|o| format!(" The old one is kept as {}.", o.display())).unwrap_or_default();
            writeln!(out, "razerctl {} is installed at {}.{kept} Restart razerctl to use it.", rel.version, path.display())
                .map_err(|e| e.to_string())
        }
        Ok(Applied::Saved { path, why }) => writeln!(
            out,
            "razerctl could not replace itself ({why}). The new version passed the check and was downloaded to {}",
            path.display()
        )
        .map_err(|e| e.to_string()),
        Err(e) => Err(format!("update failed: {e}")),
    }
}
