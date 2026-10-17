//! The tag check and the changelog lookup behind `release_notes` (DAN-1668).

/// The `version = "…"` of the `[package]` table. `Cargo.toml` is read as text: the tree takes no TOML dependency for a dev-time tool.
pub fn package_version(cargo_toml: &str) -> Result<String, String> {
    let mut in_package = false;
    for line in cargo_toml.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
        } else if in_package {
            if let Some(rest) = line.strip_prefix("version") {
                let value = rest.trim_start().strip_prefix('=').map(str::trim);
                if let Some(v) = value.and_then(|v| v.strip_prefix('"')) {
                    return v
                        .split('"')
                        .next()
                        .map(String::from)
                        .ok_or_else(|| format!("unterminated version string: {line}"));
                }
            }
        }
    }
    Err("no version in the [package] table of Cargo.toml".into())
}

/// The version a `## ` changelog heading names: `## v0.2.0`, `## 0.2.0`, `## [0.2.0]` and `## v0.2.0 - 2026-10-17` all give `0.2.0`.
fn heading_version(heading: &str) -> &str {
    let title = heading.trim_start_matches('#').trim();
    let token = title.split_whitespace().next().unwrap_or("");
    let token = token.trim_matches(|c| c == '[' || c == ']');
    token.strip_prefix('v').unwrap_or(token)
}

/// The body of the `## v<version>` section: everything up to the next `## ` heading, without leading and trailing blank lines.
pub fn changelog_section(changelog: &str, version: &str) -> Result<String, String> {
    let mut body: Option<Vec<&str>> = None;
    for line in changelog.lines() {
        let is_h2 = line.starts_with("## ");
        match body.as_mut() {
            None if is_h2 && heading_version(line) == version => body = Some(Vec::new()),
            Some(_) if is_h2 => break,
            Some(lines) => lines.push(line),
            None => {}
        }
    }
    let lines = body.ok_or_else(|| format!("CHANGELOG.md has no section for v{version}"))?;
    let text = lines.join("\n");
    let text = text.trim();
    if text.is_empty() {
        return Err(format!("the CHANGELOG.md section for v{version} is empty"));
    }
    Ok(format!("{text}\n"))
}

/// The release notes for `tag`: fails unless the tag is `v` + the `Cargo.toml` version and `CHANGELOG.md` has that version's section.
pub fn release_notes(tag: &str, cargo_toml: &str, changelog: &str) -> Result<String, String> {
    let version = package_version(cargo_toml)?;
    if tag != format!("v{version}") {
        return Err(format!(
            "tag {tag} does not match Cargo.toml version {version} (expected v{version})"
        ));
    }
    changelog_section(changelog, &version)
}
