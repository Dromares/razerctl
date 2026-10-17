//! The by-hand tarball in RELEASING.md must hold the same top-level files as the one `release.yml` builds (DAN-1674: the
//! fallback `cp` once left out `packaging/razerctl.png`, so the two tarballs differed).

use std::collections::BTreeSet;

fn read(rel: &str) -> String {
    std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel))
        .unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// Source files `release.yml` installs directly into the tarball's top-level directory.
fn workflow_files() -> BTreeSet<String> {
    let yml = read(".github/workflows/release.yml");
    let files: BTreeSet<String> = yml
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("install ") && l.ends_with("\"target/tarball/$DIR/\""))
        .flat_map(|l| l.split_whitespace().skip(1))
        .filter(|t| !t.starts_with('-') && !t.starts_with('"'))
        .map(String::from)
        .collect();
    assert!(
        !files.is_empty(),
        "no top-level `install ... target/tarball/$DIR/` lines found in release.yml"
    );
    files
}

/// Source files the "Without hosted Actions" section of RELEASING.md copies into `out/$D/`.
fn by_hand_files() -> BTreeSet<String> {
    let doc = read("RELEASING.md");
    let line = doc
        .lines()
        .find(|l| l.starts_with("cp ") && l.ends_with(" out/$D/"))
        .expect("RELEASING.md has no `cp ... out/$D/` line for the tarball's top-level files");
    let mut tokens: Vec<&str> = line.split_whitespace().skip(1).collect();
    tokens.pop(); // the destination
    tokens.into_iter().map(String::from).collect()
}

#[test]
fn by_hand_tarball_holds_the_same_top_level_files_as_the_workflow() {
    assert_eq!(
        by_hand_files(),
        workflow_files(),
        "RELEASING.md `cp` line and release.yml `install` lines have drifted"
    );
}
