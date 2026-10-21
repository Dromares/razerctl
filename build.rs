//! Build script: stamp the commit the binary was built from into `RAZERCTL_GIT_SHA`, which `firmware probe-bootloader`
//! prints at the head of its report. `RAZERCTL_GIT_SHA` in the environment wins (for builds without a `.git`); with
//! neither, the binary says `unknown`.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=RAZERCTL_GIT_SHA");
    let sha = std::env::var("RAZERCTL_GIT_SHA")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| git(&["rev-parse", "HEAD"]));
    // a commit moves HEAD's branch ref, not HEAD itself
    for p in ["HEAD", "packed-refs"] {
        if let Some(path) = git(&["rev-parse", "--git-path", p]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git(&["rev-parse", "--git-path", &r]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(sha) = sha {
        println!("cargo:rustc-env=RAZERCTL_GIT_SHA={sha}");
    }
}
