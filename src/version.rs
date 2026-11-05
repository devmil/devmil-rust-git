//! Installed Git version parsing without an application-specific support floor.

/// A normalized installed Git version, ordered numerically.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GitVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl GitVersion {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parses `git --version` output, including Apple and Windows suffixes.
    /// Missing or nonnumeric patch components normalize to zero.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?.trim();
        let value = text
            .strip_prefix("git version ")?
            .split_whitespace()
            .next()?;
        let mut components = value.split('.');
        let major = components.next()?.parse().ok()?;
        let minor = components.next()?.parse().ok()?;
        let patch = components
            .next()
            .and_then(|value| value.split(|byte: char| !byte.is_ascii_digit()).next())
            .filter(|value| !value.is_empty())
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        Some(Self::new(major, minor, patch))
    }
}

#[cfg(test)]
mod tests {
    use super::GitVersion;

    #[test]
    fn parses_vendor_versions_and_optional_patch() {
        for input in [
            b"git version 2.39.3".as_slice(),
            b"git version 2.39.3.windows.1\n",
            b"git version 2.39.3 (Apple Git-146)",
        ] {
            assert_eq!(GitVersion::parse(input), Some(GitVersion::new(2, 39, 3)));
        }
        assert_eq!(
            GitVersion::parse(b"git version 2.39"),
            Some(GitVersion::new(2, 39, 0))
        );
        assert_eq!(
            GitVersion::parse(b"git version 2.39.3-rc0"),
            Some(GitVersion::new(2, 39, 3))
        );
    }

    #[test]
    fn rejects_invalid_prefix_major_minor_and_encoding() {
        for input in [
            b"2.39.3".as_slice(),
            b"git version x.39.3",
            b"git version 2.x.3",
            b"git version 2",
            b"git version \xff",
        ] {
            assert_eq!(GitVersion::parse(input), None);
        }
        assert!(GitVersion::new(2, 40, 0) > GitVersion::new(2, 39, 9));
    }
}
