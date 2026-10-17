//! The GitHub release the check finds, read out of `GET /repos/<owner>/<repo>/releases/latest`, and the `SHA256SUMS` file that
//! ships beside its assets (names fixed by the release workflow: `razerctl-<ver>-x86_64.AppImage`, `SHA256SUMS` in `sha256sum`
//! format).

use super::json::Json;
use super::version::Version;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub version: Version,
    pub name: String,
    pub page: String,
    pub notes: String,
    pub published: String,
    pub assets: Vec<Asset>,
}

pub const SUMS_NAME: &str = "SHA256SUMS";

impl Release {
    /// `None` when the body is not a release with a version tag, or is a draft (the API never returns those from `/latest`; a
    /// proxy or a mistake might).
    pub fn parse(body: &str) -> Option<Release> {
        let j = Json::parse(body)?;
        if j.get("draft").and_then(Json::as_bool) == Some(true) {
            return None;
        }
        let str_of = |k: &str| j.get(k).and_then(Json::as_str).unwrap_or("").to_string();
        let tag = str_of("tag_name");
        let assets = j
            .get("assets")
            .and_then(Json::as_array)
            .unwrap_or_default()
            .iter()
            .filter_map(|a| {
                Some(Asset {
                    name: a.get("name")?.as_str()?.to_string(),
                    size: a.get("size").and_then(Json::as_u64).unwrap_or(0),
                    url: a.get("browser_download_url")?.as_str()?.to_string(),
                })
            })
            .collect();
        Some(Release {
            version: Version::parse(&tag)?,
            name: str_of("name"),
            page: str_of("html_url"),
            notes: str_of("body"),
            published: str_of("published_at"),
            tag,
            assets,
        })
    }

    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }

    /// `razerctl-0.3.0-x86_64.AppImage`: the file name this version is published under.
    pub fn appimage_name(&self) -> String {
        format!("razerctl-{}-x86_64.AppImage", self.version)
    }

    pub fn appimage(&self) -> Option<&Asset> {
        self.asset(&self.appimage_name())
    }

    pub fn sums(&self) -> Option<&Asset> {
        self.asset(SUMS_NAME)
    }

    /// The release notes as the dialog shows them: the body with its line endings normalised and the edges trimmed.
    pub fn notes_text(&self) -> String {
        self.notes.replace("\r\n", "\n").trim().to_string()
    }
}

/// The hash `sums` (a `sha256sum` listing: `<64 hex>  <name>`, the name starting with `*` in binary mode) records for `name`.
/// The last matching line wins, as if the file were applied top to bottom.
pub fn sum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines()
        .filter_map(|l| {
            let (hash, file) = l.split_once(' ')?;
            let file = file.strip_prefix(' ').or_else(|| file.strip_prefix('*'))?;
            let ok = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
            (ok && file == name).then(|| hash.to_ascii_lowercase())
        })
        .last()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_hash_for_one_file_in_sha256sum_format() {
        let h1 = "a".repeat(64);
        let h2 = "B".repeat(64);
        let sums = format!(
            "{h1}  razerctl-0.3.0-x86_64.AppImage\n{h2} *70-razerctl.rules\nnot a line\n{} razerctl-0.3.0-x86_64.AppImage.zsync\n",
            "c".repeat(64)
        );
        assert_eq!(sum_for(&sums, "razerctl-0.3.0-x86_64.AppImage"), Some(h1));
        assert_eq!(sum_for(&sums, "70-razerctl.rules"), Some("b".repeat(64)));
        assert_eq!(sum_for(&sums, "razerctl-0.3.0-x86_64"), None, "whole names only");
        assert_eq!(sum_for(&sums, "missing"), None);
        assert_eq!(sum_for("abc  razerctl.AppImage\n", "razerctl.AppImage"), None, "a short hash is not a hash");
        assert_eq!(sum_for("", "x"), None);
    }

    #[test]
    fn a_draft_or_a_tag_that_is_not_a_version_is_not_a_release() {
        assert_eq!(Release::parse(r#"{"tag_name":"v0.3.0","draft":true}"#), None);
        assert_eq!(Release::parse(r#"{"tag_name":"nightly"}"#), None);
        assert_eq!(Release::parse(r#"{"name":"no tag"}"#), None);
        assert_eq!(Release::parse("<html>"), None);
        let r = Release::parse(r#"{"tag_name":"v0.3.0","draft":false,"body":"a\r\nb\r\n"}"#).unwrap();
        assert_eq!(r.notes_text(), "a\nb");
        assert!(r.assets.is_empty() && r.appimage().is_none() && r.sums().is_none());
    }
}
