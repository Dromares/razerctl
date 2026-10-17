//! Prints the release notes for a tag, and refuses a tag that is not the crate's version (DAN-1668).
//!
//!     cargo run --release --locked --example release_notes -- v0.2.0 > notes.md
//!
//! Exits 1, printing nothing to stdout, unless the tag is `v` + the `[package]` version of `Cargo.toml` and `CHANGELOG.md` has a
//! `## v<version>` section with text in it; otherwise prints that section. The release workflow runs this before it builds
//! anything, so a tag that disagrees with the crate never gets as far as a draft release. Dev-time only, like `gen_icons`.

mod notes;

use std::path::PathBuf;
use std::process::exit;

fn read(path: &PathBuf) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("release_notes: {}: {e}", path.display());
        exit(2);
    })
}

fn main() {
    let Some(tag) = std::env::args().nth(1) else {
        eprintln!("usage: release_notes TAG   (for example v0.2.0)");
        exit(2);
    };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let cargo_toml = read(&root.join("Cargo.toml"));
    let changelog = read(&root.join("CHANGELOG.md"));
    match notes::release_notes(&tag, &cargo_toml, &changelog) {
        Ok(text) => print!("{text}"),
        Err(e) => {
            eprintln!("release_notes: {e}");
            exit(1);
        }
    }
}
