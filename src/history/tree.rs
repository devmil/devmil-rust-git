//! Range-history and NUL-framed ls-tree parsing.
use super::ParseError;
use crate::diff::model::DiffPath;
use crate::types::ObjectId;
const RECORD_SEPARATOR: u8 = 0x1e;
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RangeHistoryEntry {
    pub commit: ObjectId,
    pub author: Vec<u8>,
    pub author_time_unix: i64,
    pub summary: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitTreeEntryKind {
    Directory,
    File,
    Symlink,
    Submodule,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitTreeEntry {
    pub path: DiffPath,
    pub object: ObjectId,
    pub kind: CommitTreeEntryKind,
    pub mode: u32,
    pub size: Option<u64>,
}

/// Parses the bounded NUL-delimited range-history projection emitted by installed Git.
///
/// # Errors
///
/// Returns a typed parse error when a record is incomplete or contains an invalid identity or
/// timestamp.
pub fn parse_range_history(bytes: &[u8]) -> Result<Vec<RangeHistoryEntry>, ParseError> {
    let mut entries = Vec::new();
    for record in bytes.split(|byte| *byte == RECORD_SEPARATOR).skip(1) {
        let mut fields = record.splitn(5, |byte| *byte == 0);
        let commit = parse_object(fields.next().ok_or_else(parse_error)?)?;
        let author = fields.next().ok_or_else(parse_error)?.to_vec();
        let author_time_unix = parse_i64(fields.next().ok_or_else(parse_error)?)?;
        let summary = fields.next().ok_or_else(parse_error)?.to_vec();
        let _tail = fields.next().ok_or_else(parse_error)?;
        entries.push(RangeHistoryEntry {
            commit,
            author,
            author_time_unix,
            summary,
        });
    }
    Ok(entries)
}

/// Parses the bounded NUL-delimited tree projection emitted by installed Git.
///
/// # Errors
///
/// Returns a typed parse error when an entry has invalid metadata, identity, size, or path bytes.
pub fn parse_commit_tree(
    bytes: &[u8],
    directory: Option<&DiffPath>,
) -> Result<Vec<CommitTreeEntry>, ParseError> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|record| parse_commit_tree_entry(record, directory))
        .collect()
}

/// Parses a page of tree entries and reports whether more records remain.
///
/// # Errors
/// Rejects malformed records within the requested page.
pub fn parse_commit_tree_page(
    bytes: &[u8],
    directory: Option<&DiffPath>,
    offset: usize,
    limit: usize,
) -> Result<(Vec<CommitTreeEntry>, bool), ParseError> {
    let mut entries = Vec::with_capacity(limit.min(256));
    let mut records = bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .skip(offset);
    for record in records.by_ref().take(limit) {
        entries.push(parse_commit_tree_entry(record, directory)?);
    }
    Ok((entries, records.next().is_some()))
}

fn parse_commit_tree_entry(
    record: &[u8],
    directory: Option<&DiffPath>,
) -> Result<CommitTreeEntry, ParseError> {
    let tab = record
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(parse_error)?;
    let mut metadata = record[..tab]
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty());
    let mode = parse_mode(metadata.next().ok_or_else(parse_error)?)?;
    let object_type = metadata.next().ok_or_else(parse_error)?;
    let object = parse_object(metadata.next().ok_or_else(parse_error)?)?;
    let size_field = metadata.next().ok_or_else(parse_error)?;
    if metadata.next().is_some() {
        return Err(parse_error());
    }
    let relative = &record[tab + 1..];
    let mut full_path = directory.map_or_else(Vec::new, |parent| {
        let mut bytes = parent.as_bytes().to_vec();
        bytes.push(b'/');
        bytes
    });
    full_path.extend_from_slice(relative);
    let path = DiffPath::from_bytes(full_path).map_err(|_| parse_error())?;
    let kind = match (mode, object_type) {
        (0o040_000, b"tree") => CommitTreeEntryKind::Directory,
        (0o160_000, b"commit") => CommitTreeEntryKind::Submodule,
        (0o120_000, b"blob") => CommitTreeEntryKind::Symlink,
        (_, b"blob") => CommitTreeEntryKind::File,
        _ => return Err(parse_error()),
    };
    let size = if size_field == b"-" {
        None
    } else {
        Some(parse_u64(size_field)?)
    };
    Ok(CommitTreeEntry {
        path,
        object,
        kind,
        mode,
        size,
    })
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

fn parse_u64(bytes: &[u8]) -> Result<u64, ParseError> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or_else(parse_error)
}

fn parse_mode(bytes: &[u8]) -> Result<u32, ParseError> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|value| u32::from_str_radix(value, 8).ok())
        .ok_or_else(parse_error)
}

fn parse_error() -> ParseError {
    ParseError("installed Git returned malformed richer-history output")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_byte_safe_tree_entries() {
        let mut input = b"100644 blob 1111111111111111111111111111111111111111 3\tbad".to_vec();
        input.extend_from_slice(b"\xff.txt\0");
        let entries = parse_commit_tree(&input, None).unwrap();
        assert_eq!(entries[0].path.as_bytes(), b"bad\xff.txt");
        assert_eq!(entries[0].kind, CommitTreeEntryKind::File);
        assert_eq!(entries[0].size, Some(3));
    }

    #[test]
    fn prefixes_lazily_loaded_directory_children() {
        let input = b"040000 tree 1111111111111111111111111111111111111111 -\tnested\0";
        let directory = DiffPath::from_bytes(b"src".to_vec()).unwrap();
        let entries = parse_commit_tree(input, Some(&directory)).unwrap();
        assert_eq!(entries[0].path.as_bytes(), b"src/nested");
        assert_eq!(entries[0].kind, CommitTreeEntryKind::Directory);
    }
}
