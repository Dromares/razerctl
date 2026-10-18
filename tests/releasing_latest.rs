//! Publishing a draft must make it the Latest release, and RELEASING.md must say how to check that it did (DAN-1698, D8).
//! On DAN-1694 a draft published without `--latest` left v0.2.0 as Latest, and the in-app updater (`/releases/latest`)
//! and AppImageUpdate (`gh-releases-zsync|...|latest|...`) both kept pointing at the old release.
//!
//! `release.yml` is deliberately not changed: it only creates a draft, and GitHub's REST docs say a draft cannot be
//! set as latest (`make_latest`: "Drafts and prereleases cannot be set as latest"), so the flag belongs on the publish step.

fn read(rel: &str) -> String {
    std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel))
        .unwrap_or_else(|e| panic!("{rel}: {e}"))
}

#[test]
fn publish_command_makes_the_release_latest() {
    let doc = read("RELEASING.md");
    let publish: Vec<&str> = doc
        .lines()
        .filter(|l| l.contains("gh release edit") && l.contains("--draft=false"))
        .collect();
    assert_eq!(
        publish.len(),
        1,
        "RELEASING.md should have exactly one publish command, found {publish:?}"
    );
    assert!(
        publish[0].contains("--latest"),
        "the publish command does not pass --latest: {}",
        publish[0]
    );
}

#[test]
fn publish_step_checks_the_anonymous_latest_endpoint() {
    let doc = read("RELEASING.md");
    let publish_at = doc
        .find("--draft=false")
        .expect("RELEASING.md has no publish command");
    let after = &doc[publish_at..];
    // the check must come before the next numbered section ("## 5."), i.e. be part of the publish step
    let step = after.split("\n## ").next().unwrap();
    assert!(
        step.contains("https://api.github.com/repos/Dromares/razerctl/releases/latest"),
        "the publish step does not check the anonymous /releases/latest endpoint:\n{step}"
    );
    assert!(
        step.contains("tag_name"),
        "the check should compare tag_name with the new tag:\n{step}"
    );
}
