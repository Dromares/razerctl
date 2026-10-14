//! razerctl is Rust only: no Python, tooling and mockups included (board rule, DAN-1396; DAN-1479/1480).
//! Fails if any tracked file has a Python extension or a Python shebang.

use std::path::Path;
use std::process::Command;

#[test]
fn no_tracked_python() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let ls = |args: &[&str]| Command::new("git").arg("-C").arg(root).args(args).output();
    // Not a git checkout (a crate tarball) or no git binary: nothing to check.
    match ls(&["ls-files", "--error-unmatch", "Cargo.toml"]) {
        Ok(o) if o.status.success() => {}
        _ => {
            eprintln!("not a git checkout of razerctl; skipping the no-python guard");
            return;
        }
    }
    let out = ls(&["ls-files", "-z"]).expect("git ls-files");
    assert!(out.status.success(), "git ls-files failed");
    let mut bad = Vec::new();
    for rel in String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|s| !s.is_empty())
    {
        let ext = Path::new(rel)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if matches!(ext, "py" | "pyw" | "pyi") {
            bad.push(format!("{rel}: python extension"));
            continue;
        }
        // Tracked but deleted in the working tree, or a non-UTF-8 binary: not a script.
        if let Ok(bytes) = std::fs::read(root.join(rel)) {
            let first = bytes.split(|&b| b == b'\n').next().unwrap_or(&[]);
            if first.starts_with(b"#!") && String::from_utf8_lossy(first).contains("python") {
                bad.push(format!("{rel}: python shebang"));
            }
        }
    }
    assert!(bad.is_empty(), "razerctl is Rust only, remove: {bad:#?}");
}
