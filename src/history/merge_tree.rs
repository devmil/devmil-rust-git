//! Tree identity and conflict paths from `git merge-tree -z --name-only`.
use super::ParseError;
use crate::types::{ObjectId, RepositoryRelativePath};

/// Reads the first newline-delimited full object ID.
///
/// # Errors
/// Rejects missing, malformed, or non-UTF-8 identities.
pub fn parse_first_oid(bytes: &[u8]) -> Result<ObjectId, ParseError> {
    let line = bytes
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    let text = std::str::from_utf8(line)
        .map(str::trim)
        .map_err(|_| malformed_oid())?;
    ObjectId::parse(text).map_err(|_| malformed_oid())
}

/// Parses conflict paths, retaining at most `maximum + 1` to indicate overflow.
/// The caller owns command exit-status interpretation and truncation policy.
///
/// # Errors
/// Rejects malformed result tree IDs and paths unsafe or unrepresentable on this platform.
pub fn parse_conflict_paths(
    bytes: &[u8],
    maximum: usize,
) -> Result<Vec<RepositoryRelativePath>, ParseError> {
    let mut fields = bytes.split(|byte| *byte == 0);
    parse_first_oid(fields.next().unwrap_or_default())?;
    let mut paths = Vec::new();
    for field in fields {
        if field.is_empty() {
            break;
        }
        paths.push(
            RepositoryRelativePath::new(native_path(field)?)
                .map_err(|_| ParseError("merge-tree returned an unsafe conflict path"))?,
        );
        if paths.len() > maximum {
            break;
        }
    }
    Ok(paths)
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn native_path(bytes: &[u8]) -> Result<std::path::PathBuf, ParseError> {
    use std::os::unix::ffi::OsStringExt;
    Ok(std::ffi::OsString::from_vec(bytes.to_vec()).into())
}

#[cfg(not(unix))]
fn native_path(bytes: &[u8]) -> Result<std::path::PathBuf, ParseError> {
    String::from_utf8(bytes.to_vec())
        .map(std::path::PathBuf::from)
        .map_err(|_| {
            ParseError(
                "merge-tree returned a conflict path that is not representable on this platform",
            )
        })
}

fn malformed_oid() -> ParseError {
    ParseError("Git returned a malformed object identity during conflict prediction")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_conflict_paths_and_exposes_overflow_without_reading_trailing_messages() {
        let bytes = format!("{}\0one\0two\0three\0\0message", "1".repeat(40));
        let paths = parse_conflict_paths(bytes.as_bytes(), 1).unwrap();
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].as_path(), std::path::Path::new("one"));
        assert!(
            parse_conflict_paths(format!("{}\0../escape\0", "1".repeat(40)).as_bytes(), 10)
                .is_err()
        );
    }
}
