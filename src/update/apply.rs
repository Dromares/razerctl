//! The AppImage update itself, run only after the user pressed Update (or answered `y`): download the new `.AppImage` and
//! `SHA256SUMS`, verify, then swap it in over the running file and keep the old one as `<name>.old`. A checksum mismatch
//! refuses and changes nothing; a directory razerctl cannot write to ends with the verified file saved elsewhere.

use super::net::{self, FetchError, Source};
use super::release::{sum_for, Release};
use crate::sha256;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Downloading { done: u64, total: u64 },
    Verifying,
    Replacing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateError {
    Net(FetchError),
    /// The release has no such file (no AppImage for this version, or no `SHA256SUMS`, or no line for the AppImage in it).
    MissingAsset(String),
    /// The download does not hash to what `SHA256SUMS` says. It was deleted; nothing was changed.
    Mismatch { expected: String, got: String },
    Io(String),
    Cancelled,
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateError::Net(e) => write!(f, "download failed: {e}"),
            UpdateError::MissingAsset(n) => write!(f, "the release has no {n}"),
            UpdateError::Mismatch { expected, got } => write!(
                f,
                "the download does not match SHA256SUMS (expected {expected}, got {got}); nothing was changed"
            ),
            UpdateError::Io(e) => f.write_str(e),
            UpdateError::Cancelled => f.write_str("cancelled"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
    /// `target` now holds the new version; the previous file is `old` (absent when `target` had already disappeared).
    Replaced { path: PathBuf, old: Option<PathBuf> },
    /// The new version passed the check and is saved at `path` because `target` could not be replaced (`why`).
    Saved { path: PathBuf, why: String },
}

fn io_err(path: &Path, e: io::Error) -> UpdateError {
    UpdateError::Io(format!("{}: {e}", path.display()))
}

/// Can a file be created in `dir`? Tried for real, because mode bits say nothing about a read-only mount or an ACL.
fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".razerctl-write-test-{}", std::process::id()));
    match fs::File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// `$XDG_DOWNLOAD_DIR`, else `XDG_DOWNLOAD_DIR` from `user-dirs.dirs`, else `$HOME`.
pub fn fallback_dir() -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    if let Some(d) = std::env::var_os("XDG_DOWNLOAD_DIR").filter(|d| !d.is_empty()) {
        return d.into();
    }
    let cfg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    fs::read_to_string(cfg.join("user-dirs.dirs"))
        .ok()
        .and_then(|t| user_dirs_download(&t, &home))
        .unwrap_or(home)
}

/// The `XDG_DOWNLOAD_DIR="$HOME/Downloads"` line of a `user-dirs.dirs` file.
pub fn user_dirs_download(text: &str, home: &Path) -> Option<PathBuf> {
    let line = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("XDG_DOWNLOAD_DIR="))?;
    let v = line.trim().trim_matches('"');
    match v.strip_prefix("$HOME") {
        Some(rest) => Some(home.join(rest.trim_start_matches('/'))),
        None if v.starts_with('/') => Some(v.into()),
        None => None,
    }
}

/// Moves `target` aside as `<target>.old` and puts `staged` in its place. The new file is never absent from `target`'s name:
/// the old one is hard-linked to `.old` first, so `rename` replaces `target` atomically. Where hard links are refused, it is
/// renamed instead and put back if the second rename fails.
fn swap_in(staged: &Path, target: &Path) -> io::Result<Option<PathBuf>> {
    if !target.exists() {
        fs::rename(staged, target)?;
        return Ok(None);
    }
    let mut old_name: OsString = target.as_os_str().to_owned();
    old_name.push(".old");
    let old = PathBuf::from(old_name);
    let _ = fs::remove_file(&old);
    let moved = match fs::hard_link(target, &old) {
        Ok(()) => false,
        Err(_) => {
            fs::rename(target, &old)?;
            true
        }
    };
    if let Err(e) = fs::rename(staged, target) {
        if moved {
            let _ = fs::rename(&old, target);
        } else {
            let _ = fs::remove_file(&old);
        }
        return Err(e);
    }
    Ok(Some(old))
}

/// Downloads and verifies `rel`'s AppImage and replaces `target` with it. `progress` is told each stage; `cancel` stops the
/// download (the partial file is deleted).
pub fn apply_appimage(
    src: &Source,
    rel: &Release,
    target: &Path,
    fallback: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Stage),
) -> Result<Applied, UpdateError> {
    let asset = rel
        .appimage()
        .ok_or_else(|| UpdateError::MissingAsset(rel.appimage_name()))?;
    let sums_asset = rel
        .sums()
        .ok_or_else(|| UpdateError::MissingAsset(super::release::SUMS_NAME.into()))?;
    let sums = net::fetch_text(src, &sums_asset.url).map_err(UpdateError::Net)?;
    let expected = sum_for(&sums, &asset.name).ok_or_else(|| {
        UpdateError::MissingAsset(format!("{} line in {}", asset.name, sums_asset.name))
    })?;

    let target_dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let in_place = writable(target_dir);
    let stage_dir = if in_place { target_dir } else { fallback };
    fs::create_dir_all(stage_dir).map_err(|e| io_err(stage_dir, e))?;
    let staged = stage_dir.join(format!("{}.part", asset.name));

    let mut on_bytes = |done, total| progress(Stage::Downloading { done, total });
    net::download(src, &asset.url, &staged, net::MAX_APPIMAGE, cancel, &mut on_bytes).map_err(
        |e| match e {
            FetchError::Cancelled => UpdateError::Cancelled,
            e => UpdateError::Net(e),
        },
    )?;

    progress(Stage::Verifying);
    let got = fs::File::open(&staged)
        .and_then(sha256::hex_digest)
        .map_err(|e| io_err(&staged, e))?;
    if got != expected {
        let _ = fs::remove_file(&staged);
        return Err(UpdateError::Mismatch { expected, got });
    }
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = fs::remove_file(&staged);
        return Err(UpdateError::Cancelled);
    }
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).map_err(|e| io_err(&staged, e))?;

    if in_place {
        progress(Stage::Replacing);
        match swap_in(&staged, target) {
            Ok(old) => {
                return Ok(Applied::Replaced {
                    path: target.to_path_buf(),
                    old,
                })
            }
            Err(e) => {
                let why = format!("{}: {e}", target.display());
                return save_aside(&staged, fallback, &asset.name, why);
            }
        }
    }
    let why = format!("{} is not writable", target_dir.display());
    save_aside(&staged, fallback, &asset.name, why)
}

/// Gives the verified `staged` file its final name in `dir` (moving it there when it is somewhere else).
fn save_aside(staged: &Path, dir: &Path, name: &str, why: String) -> Result<Applied, UpdateError> {
    fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
    let path = dir.join(name);
    if fs::rename(staged, &path).is_err() {
        fs::copy(staged, &path).map_err(|e| io_err(&path, e))?;
        let _ = fs::remove_file(staged);
    }
    Ok(Applied::Saved { path, why })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_dirs_names_the_download_folder() {
        let home = Path::new("/home/u");
        let t = "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOWNLOAD_DIR=\"$HOME/Downloads\"\n";
        assert_eq!(user_dirs_download(t, home), Some("/home/u/Downloads".into()));
        assert_eq!(
            user_dirs_download("XDG_DOWNLOAD_DIR=\"/mnt/dl\"\n", home),
            Some("/mnt/dl".into())
        );
        assert_eq!(user_dirs_download("XDG_DOWNLOAD_DIR=\"$HOME\"\n", home), Some("/home/u".into()));
        assert_eq!(user_dirs_download("XDG_DOWNLOAD_DIR=\"rel\"\n", home), None);
        assert_eq!(user_dirs_download("", home), None);
    }
}
