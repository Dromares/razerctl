//! Release versions: `0.3.0`, `v0.2.91-test.2`. Ordered by semver 2.0.0 precedence, so a pre-release sorts before its release
//! (`0.3.0-rc.1 < 0.3.0`) and `v0.2.90-test.1 < v0.2.91-test.2 < v0.3.0`. Build metadata (`+…`) is dropped.

use std::cmp::Ordering;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Ident {
    Num(u64),
    Alpha(String),
}

impl Ord for Ident {
    fn cmp(&self, o: &Self) -> Ordering {
        match (self, o) {
            (Ident::Num(a), Ident::Num(b)) => a.cmp(b),
            (Ident::Num(_), Ident::Alpha(_)) => Ordering::Less,
            (Ident::Alpha(_), Ident::Num(_)) => Ordering::Greater,
            (Ident::Alpha(a), Ident::Alpha(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for Ident {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    core: [u64; 3],
    pre: Vec<Ident>,
}

impl Version {
    /// `1.2.3`, `v1.2.3`, `1.2.3-rc.1`, `1.2.3-rc.1+build`. `None` for anything else (two-part versions, empty identifiers,
    /// leading zeros in a number).
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let s = s.split('+').next()?;
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let mut nums = core.split('.').map(number);
        let version = Version {
            core: [nums.next()??, nums.next()??, nums.next()??],
            pre: match pre {
                Some(p) => p.split('.').map(ident).collect::<Option<_>>()?,
                None => Vec::new(),
            },
        };
        nums.next().is_none().then_some(version)
    }

    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

fn number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0'))
    {
        return None;
    }
    s.parse().ok()
}

fn ident(s: &str) -> Option<Ident> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return None;
    }
    if s.bytes().all(|b| b.is_ascii_digit()) {
        number(s).map(Ident::Num)
    } else {
        Some(Ident::Alpha(s.to_string()))
    }
}

impl Ord for Version {
    fn cmp(&self, o: &Self) -> Ordering {
        self.core.cmp(&o.core).then_with(|| {
            // no pre-release identifiers outranks any pre-release of the same core
            match (self.pre.is_empty(), o.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&o.pre),
            }
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.core[0], self.core[1], self.core[2])?;
        for (i, id) in self.pre.iter().enumerate() {
            f.write_str(if i == 0 { "-" } else { "." })?;
            match id {
                Ident::Num(n) => write!(f, "{n}")?,
                Ident::Alpha(a) => f.write_str(a)?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("{s} should parse"))
    }

    #[test]
    fn the_test_release_tags_sort_in_release_order() {
        assert!(v("v0.2.90-test.1") < v("v0.2.91-test.2"));
        assert!(v("v0.2.91-test.2") < v("v0.3.0"));
        assert!(v("v0.2.0") < v("v0.2.90-test.1"));
        assert!(v("0.3.0-rc.1") < v("0.3.0"), "a pre-release is older than its release");
        assert!(v("0.3.0-rc.1") > v("0.2.99"));
    }

    #[test]
    fn numbers_compare_as_numbers_and_prerelease_parts_by_semver() {
        assert!(v("0.10.0") > v("0.9.0"));
        assert!(v("1.0.0-test.2") < v("1.0.0-test.10"));
        assert!(v("1.0.0-1") < v("1.0.0-alpha"), "numeric identifiers rank below alphanumeric");
        assert!(v("1.0.0-alpha") < v("1.0.0-alpha.1"), "a longer identifier list ranks higher");
        assert!(v("1.0.0-alpha") < v("1.0.0-beta"));
        assert_eq!(v("1.2.3+build.5"), v("v1.2.3"));
    }

    #[test]
    fn parses_only_whole_versions_and_prints_them_back() {
        for bad in [
            "", "v", "1", "1.2", "1.2.3.4", "1.2.x", "01.2.3", "1.2.3-", "1.2.3-a..b", "1.2.3-a_b", "latest", "-1.2.3",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
        assert_eq!(v("v0.2.91-test.2+abc").to_string(), "0.2.91-test.2");
        assert_eq!(v(" 0.3.0 ").to_string(), "0.3.0");
        assert!(v("0.3.0-rc.1").is_prerelease() && !v("0.3.0").is_prerelease());
        assert_eq!(Version::parse(env!("CARGO_PKG_VERSION")).map(|v| v.to_string()).as_deref(), Some(env!("CARGO_PKG_VERSION")));
    }
}
