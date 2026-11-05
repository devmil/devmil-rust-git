//! Native-path parsing of `git submodule status` output.
use crate::types::{ObjectId, RepositoryRelativePath};
#[cfg(unix)]
use std::{ffi::OsString, path::PathBuf};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmoduleParseErrorKind {
    InvalidOutput,
    TooLarge,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmoduleParseError {
    kind: SubmoduleParseErrorKind,
    detail: String,
}
impl SubmoduleParseError {
    fn new(kind: SubmoduleParseErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
    pub const fn kind(&self) -> SubmoduleParseErrorKind {
        self.kind
    }
}
impl std::fmt::Display for SubmoduleParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}
impl std::error::Error for SubmoduleParseError {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmoduleCheckoutState {
    AtRecordedCommit,
    DifferentCommit,
    Uninitialized,
    Conflicted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmoduleStatus {
    pub path: RepositoryRelativePath,
    pub commit: ObjectId,
    pub checkout: SubmoduleCheckoutState,
}

/// Parses submodule checkout markers, full object IDs, and native byte paths.
///
/// # Errors
/// Rejects malformed or unsafe paths, invalid records, and more than `maximum` submodules.
pub fn parse_submodule_status(
    bytes: &[u8],
    maximum: usize,
) -> Result<Vec<SubmoduleStatus>, SubmoduleParseError> {
    let mut statuses = Vec::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if statuses.len() == maximum {
            return Err(parse_error(
                SubmoduleParseErrorKind::TooLarge,
                "submodule count exceeds its configured bound",
            ));
        }
        let (&marker, remainder) = line.split_first().ok_or_else(malformed)?;
        let checkout = match marker {
            b' ' => SubmoduleCheckoutState::AtRecordedCommit,
            b'+' => SubmoduleCheckoutState::DifferentCommit,
            b'-' => SubmoduleCheckoutState::Uninitialized,
            b'U' => SubmoduleCheckoutState::Conflicted,
            _ => return Err(malformed()),
        };
        let separator = remainder
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or_else(malformed)?;
        let commit = std::str::from_utf8(&remainder[..separator])
            .ok()
            .and_then(|value| ObjectId::parse(value).ok())
            .ok_or_else(malformed)?;
        let path = parse_status_path(&remainder[separator + 1..])?;
        statuses.push(SubmoduleStatus {
            path: repository_relative_path(path)?,
            commit,
            checkout,
        });
    }
    Ok(statuses)
}

fn parse_status_path(value: &[u8]) -> Result<Vec<u8>, SubmoduleParseError> {
    if value.first() == Some(&b'"') {
        let mut escaped = false;
        let closing = value
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(index, byte)| {
                if escaped {
                    escaped = false;
                    None
                } else if *byte == b'\\' {
                    escaped = true;
                    None
                } else if *byte == b'"' {
                    Some(index)
                } else {
                    None
                }
            })
            .ok_or_else(malformed)?;
        return decode_git_quoted(&value[1..closing]);
    }
    let path_end = value
        .windows(2)
        .rposition(|window| window == b" (")
        .filter(|_| value.ends_with(b")"))
        .unwrap_or(value.len());
    if path_end == 0 {
        return Err(malformed());
    }
    Ok(value[..path_end].to_vec())
}

fn decode_git_quoted(value: &[u8]) -> Result<Vec<u8>, SubmoduleParseError> {
    let mut output = Vec::with_capacity(value.len());
    let mut cursor = 0;
    while cursor < value.len() {
        if value[cursor] != b'\\' {
            output.push(value[cursor]);
            cursor += 1;
            continue;
        }
        cursor += 1;
        let escaped = *value.get(cursor).ok_or_else(malformed)?;
        cursor += 1;
        match escaped {
            b'\\' | b'"' => output.push(escaped),
            b'a' => output.push(0x07),
            b'b' => output.push(0x08),
            b't' => output.push(b'\t'),
            b'n' => output.push(b'\n'),
            b'v' => output.push(0x0b),
            b'f' => output.push(0x0c),
            b'r' => output.push(b'\r'),
            b'0'..=b'7' => {
                let mut octal = u16::from(escaped - b'0');
                for _ in 0..2 {
                    let Some(next @ b'0'..=b'7') = value.get(cursor).copied() else {
                        break;
                    };
                    octal = octal * 8 + u16::from(next - b'0');
                    cursor += 1;
                }
                output.push(u8::try_from(octal).map_err(|_| malformed())?);
            }
            _ => return Err(malformed()),
        }
    }
    Ok(output)
}

#[cfg(unix)]
fn repository_relative_path(bytes: Vec<u8>) -> Result<RepositoryRelativePath, SubmoduleParseError> {
    use std::os::unix::ffi::OsStringExt;
    RepositoryRelativePath::new(PathBuf::from(OsString::from_vec(bytes))).map_err(|_| malformed())
}

#[cfg(not(unix))]
fn repository_relative_path(bytes: Vec<u8>) -> Result<RepositoryRelativePath, SubmoduleParseError> {
    String::from_utf8(bytes)
        .ok()
        .and_then(|path| RepositoryRelativePath::new(path).ok())
        .ok_or_else(malformed)
}

fn malformed() -> SubmoduleParseError {
    parse_error(
        SubmoduleParseErrorKind::InvalidOutput,
        "Git submodule status output is malformed",
    )
}

fn parse_error(kind: SubmoduleParseErrorKind, detail: &str) -> SubmoduleParseError {
    SubmoduleParseError::new(kind, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_checkout_states_sha_formats_and_quoted_paths() {
        let sha1 = "0123456789012345678901234567890123456789";
        let sha256 = "0123456789012345678901234567890123456789012345678901234567890123";
        let bytes = format!(
            " {sha1} vendor/one (heads/main)\n+{sha1} vendor/two (v1.0)\n-{sha256} \"vendor/line\\nfeed\"\nU{sha1} vendor/conflict\n"
        );
        let statuses = parse_submodule_status(bytes.as_bytes(), 100).unwrap();
        assert_eq!(statuses.len(), 4);
        assert_eq!(
            statuses[0].checkout,
            SubmoduleCheckoutState::AtRecordedCommit
        );
        assert_eq!(
            statuses[1].checkout,
            SubmoduleCheckoutState::DifferentCommit
        );
        assert_eq!(statuses[2].checkout, SubmoduleCheckoutState::Uninitialized);
        assert_eq!(
            statuses[2].path.as_path().as_os_str().as_encoded_bytes(),
            b"vendor/line\nfeed"
        );
        assert_eq!(statuses[3].checkout, SubmoduleCheckoutState::Conflicted);
    }

    #[test]
    fn rejects_truncated_and_unknown_records() {
        assert_eq!(
            parse_submodule_status(b"?bad\n", 100).unwrap_err().kind(),
            SubmoduleParseErrorKind::InvalidOutput
        );
        assert_eq!(
            parse_submodule_status(b" 0123 missing-object\n", 100)
                .unwrap_err()
                .kind(),
            SubmoduleParseErrorKind::InvalidOutput
        );
    }
}
