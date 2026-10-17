//! The release tag check and the changelog lookup (DAN-1668): a tag must be `v` + the `Cargo.toml` version, and the release notes
//! are that version's `CHANGELOG.md` section.

#[path = "../examples/release_notes/notes.rs"]
mod notes;

const CARGO: &str = r#"[package]
name = "razerctl"
version = "0.2.0"
edition = "2021"

[dependencies]
libc = "0.2"
"#;

const CHANGELOG: &str = "# Changelog

## v0.2.1

Fixes.

## v0.2.0

First tagged release.

### Heading inside the section

- one
- two

## v0.1.0

Not this one.
";

#[test]
fn matching_tag_prints_that_versions_section() {
    let out = notes::release_notes("v0.2.0", CARGO, CHANGELOG).unwrap();
    assert_eq!(
        out,
        "First tagged release.\n\n### Heading inside the section\n\n- one\n- two\n"
    );
}

#[test]
fn mismatching_tag_is_refused() {
    for tag in ["v0.2.1", "0.2.0", "v0.2.0-rc.1", "v0.2", "V0.2.0", ""] {
        let err = notes::release_notes(tag, CARGO, CHANGELOG).unwrap_err();
        assert!(
            err.contains("does not match Cargo.toml version 0.2.0"),
            "{tag:?}: {err}"
        );
    }
}

#[test]
fn missing_section_is_refused() {
    let cargo = CARGO.replace("0.2.0", "0.3.0");
    let err = notes::release_notes("v0.3.0", &cargo, CHANGELOG).unwrap_err();
    assert_eq!(err, "CHANGELOG.md has no section for v0.3.0");
}

#[test]
fn empty_section_is_refused() {
    let changelog = "## v0.2.0\n\n\n## v0.1.0\n\ntext\n";
    let err = notes::release_notes("v0.2.0", CARGO, changelog).unwrap_err();
    assert!(err.contains("is empty"), "{err}");
}

#[test]
fn pre_release_tag_matches_its_own_section_only() {
    let cargo = CARGO.replace("0.2.0", "0.2.90-test.1");
    let changelog =
        "## v0.2.90-test.1\n\nTest build.\n\n## v0.2.90\n\nWrong.\n\n## v0.2.9\n\nAlso wrong.\n";
    assert_eq!(
        notes::release_notes("v0.2.90-test.1", &cargo, changelog).unwrap(),
        "Test build.\n"
    );
    // The final version must not borrow the pre-release's section (or the other way round).
    assert!(notes::release_notes("v0.2.90", &cargo, changelog).is_err());
    let cargo = CARGO.replace("0.2.0", "0.2.9");
    assert_eq!(
        notes::release_notes("v0.2.9", &cargo, changelog).unwrap(),
        "Also wrong.\n"
    );
}

#[test]
fn heading_styles_and_the_last_section() {
    let changelog = "## [0.2.0] - 2026-10-17\n\nBracketed.\n";
    assert_eq!(
        notes::release_notes("v0.2.0", CARGO, changelog).unwrap(),
        "Bracketed.\n"
    );
}

#[test]
fn version_comes_from_the_package_table_only() {
    let cargo = "[dependencies]\nfoo = { version = \"9.9.9\" }\nversion = \"8.8.8\"\n\n[package]\nname = \"x\"\nversion = \"1.2.3\"\n\n[profile.release]\nversion = \"7\"\n";
    assert_eq!(notes::package_version(cargo).unwrap(), "1.2.3");
    assert!(notes::package_version("[dependencies]\nversion = \"1\"\n").is_err());
}

#[test]
fn the_real_manifest_and_changelog_agree() {
    // The crate's own version must have a changelog section, or the next tag push would fail at the first step.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let cargo = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let changelog = std::fs::read_to_string(root.join("CHANGELOG.md")).unwrap();
    let version = notes::package_version(&cargo).unwrap();
    notes::release_notes(&format!("v{version}"), &cargo, &changelog).unwrap();
}
