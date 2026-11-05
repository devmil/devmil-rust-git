//! Byte-preserving installed Git path history parsing.
use super::ParseError;
use crate::diff::model::DiffPath;
use crate::types::ObjectId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathHistoryEntry {
    pub commit: ObjectId,
    pub author: Vec<u8>,
    pub author_time_unix: i64,
    pub summary: Vec<u8>,
    pub path: DiffPath,
    pub old_path: Option<DiffPath>,
}

const RECORD_SEPARATOR: u8 = 0x1e;
/// Parses record-separated log output with NUL-framed metadata and name-status fields.
///
/// # Errors
/// Rejects malformed commit identities, timestamps, and paths.
pub fn parse_path_history(
    bytes: &[u8],
    requested_path: &DiffPath,
) -> Result<Vec<PathHistoryEntry>, ParseError> {
    let mut entries = Vec::new();
    for record in bytes.split(|byte| *byte == RECORD_SEPARATOR).skip(1) {
        if record.is_empty() {
            continue;
        }
        let mut fields = record.splitn(5, |byte| *byte == 0);
        let commit = parse_object(fields.next().ok_or_else(parse_error)?)?;
        let author = fields.next().ok_or_else(parse_error)?.to_vec();
        let author_time_unix = parse_i64(fields.next().ok_or_else(parse_error)?)?;
        let summary = fields.next().ok_or_else(parse_error)?.to_vec();
        let changes = fields.next().unwrap_or_default();
        let (path, old_path) = parse_first_change(changes, requested_path)?;
        entries.push(PathHistoryEntry {
            commit,
            author,
            author_time_unix,
            summary,
            path,
            old_path,
        });
    }
    Ok(entries)
}

fn parse_first_change(
    bytes: &[u8],
    requested_path: &DiffPath,
) -> Result<(DiffPath, Option<DiffPath>), ParseError> {
    let bytes = bytes
        .iter()
        .position(|byte| !matches!(byte, 0 | b'\n'))
        .map_or(&[][..], |start| &bytes[start..]);
    let mut fields = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let Some(status) = fields.next() else {
        return Ok((requested_path.clone(), None));
    };
    if matches!(status.first(), Some(b'R' | b'C')) {
        let old_path = parse_path(fields.next().ok_or_else(parse_error)?)?;
        let path = parse_path(fields.next().ok_or_else(parse_error)?)?;
        Ok((path, Some(old_path)))
    } else {
        let path = fields
            .next()
            .map_or_else(|| Ok(requested_path.clone()), parse_path)?;
        Ok((path, None))
    }
}

fn parse_object(bytes: &[u8]) -> Result<ObjectId, ParseError> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|value| ObjectId::parse(value).ok())
        .ok_or_else(parse_error)
}

fn parse_i64(bytes: &[u8]) -> Result<i64, ParseError> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or_else(parse_error)
}

fn parse_path(bytes: &[u8]) -> Result<DiffPath, ParseError> {
    DiffPath::from_bytes(bytes.to_vec()).map_err(|_| ParseError("history path is invalid"))
}

fn parse_error() -> ParseError {
    ParseError("path history output is malformed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_history_preserves_old_and_new_byte_paths() {
        let bytes = format!(
            "\x1e{}\0Author\010\0Subject\0\nR100\0old.txt\0new.txt\0",
            "1".repeat(40)
        )
        .into_bytes();
        let path = DiffPath::from_bytes(b"new.txt".to_vec()).unwrap();
        let entries = parse_path_history(&bytes, &path).unwrap();
        assert_eq!(entries[0].path, path);
        assert_eq!(entries[0].old_path.as_ref().unwrap().as_bytes(), b"old.txt");
        assert_eq!(entries[0].author_time_unix, 10);
    }
}
