//! razerctl is Rust only: no Python, tooling and mockups included (board rule, DAN-1396; DAN-1479/1480), and no shell build
//! scripts added to the repo (board brief on DAN-1620; DAN-1634). Fails if any tracked file has a Python extension or a
//! Python shebang, or is a shell script outside the allowlist below.

use std::path::Path;
use std::process::Command;

/// The only tracked shell scripts. A new one needs a board decision, not an edit here.
const SHELL_ALLOWLIST: &[&str] = &["install.sh", "scripts/local-ci.sh"];

/// Tracked paths (relative to the crate root), or `None` when this is not a git checkout (a crate tarball, no git binary).
fn tracked_files(root: &Path) -> Option<Vec<String>> {
    let ls = |args: &[&str]| Command::new("git").arg("-C").arg(root).args(args).output();
    match ls(&["ls-files", "--error-unmatch", "Cargo.toml"]) {
        Ok(o) if o.status.success() => {}
        _ => {
            eprintln!("not a git checkout of razerctl; skipping the language guards");
            return None;
        }
    }
    let out = ls(&["ls-files", "-z"]).expect("git ls-files");
    assert!(out.status.success(), "git ls-files failed");
    Some(
        String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
    )
}

/// The first line of a tracked file, empty when it is deleted in the working tree or not readable.
fn first_line(root: &Path, rel: &str) -> String {
    match std::fs::read(root.join(rel)) {
        Ok(bytes) => {
            String::from_utf8_lossy(bytes.split(|&b| b == b'\n').next().unwrap_or(&[])).into_owned()
        }
        Err(_) => String::new(),
    }
}

#[test]
fn no_tracked_python() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let Some(files) = tracked_files(root) else {
        return;
    };
    let mut bad = Vec::new();
    for rel in &files {
        let ext = Path::new(rel)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if matches!(ext, "py" | "pyw" | "pyi") {
            bad.push(format!("{rel}: python extension"));
            continue;
        }
        let first = first_line(root, rel);
        if first.starts_with("#!") && first.contains("python") {
            bad.push(format!("{rel}: python shebang"));
        }
    }
    assert!(bad.is_empty(), "razerctl is Rust only, remove: {bad:#?}");
}

#[test]
fn no_tracked_shell_scripts_outside_the_allowlist() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let Some(files) = tracked_files(root) else {
        return;
    };
    let mut bad = Vec::new();
    for rel in files
        .iter()
        .filter(|f| !SHELL_ALLOWLIST.contains(&f.as_str()))
    {
        let ext = Path::new(rel)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let first = first_line(root, rel);
        let shebang_shell = first.starts_with("#!")
            && first
                .split(|c: char| c == '/' || c.is_whitespace())
                .any(|w| matches!(w, "sh" | "bash" | "dash" | "zsh" | "ksh" | "fish"));
        if matches!(ext, "sh" | "bash" | "zsh" | "fish") || shebang_shell {
            bad.push(rel.as_str());
        }
    }
    assert!(
        bad.is_empty(),
        "razerctl takes no new shell scripts (allowed: {SHELL_ALLOWLIST:?}); write it in Rust like examples/gen_icons, remove: {bad:#?}"
    );
    for allowed in SHELL_ALLOWLIST {
        assert!(
            files.iter().any(|f| f == allowed),
            "{allowed} is on the shell allowlist but is not tracked; drop it from the list"
        );
    }
}
