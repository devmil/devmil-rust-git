//! Parsing for unmerged index stages.
use super::model::{DiffPath, GitFileMode};
use crate::types::ObjectId;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexStage {
    pub mode: GitFileMode,
    pub object: ObjectId,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IndexParseError(&'static str);
impl std::fmt::Display for IndexParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for IndexParseError {}

/// Parses byte-safe `ls-files --unmerged -z` stage entries.
///
/// # Errors
/// Rejects malformed paths, modes, objects, stage numbers, and duplicate stages.
pub fn parse_unmerged_index(
    bytes: &[u8],
) -> Result<BTreeMap<DiffPath, [Option<IndexStage>; 3]>, IndexParseError> {
    let mut paths = BTreeMap::<DiffPath, [Option<IndexStage>; 3]>::new();
    for record in bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or(IndexParseError("unmerged index record has no path"))?;
        let mut fields = record[..tab].split(|byte| *byte == b' ');
        let mode = fields
            .next()
            .ok_or(IndexParseError("unmerged index mode is missing"))?;
        let object = fields
            .next()
            .ok_or(IndexParseError("unmerged index object is missing"))?;
        let stage = fields
            .next()
            .ok_or(IndexParseError("unmerged index stage is missing"))?;
        if fields.next().is_some() || !matches!(stage, b"1" | b"2" | b"3") {
            return Err(IndexParseError("unmerged index header is invalid"));
        }
        let path = DiffPath::from_bytes(record[tab + 1..].to_vec())
            .map_err(|_| IndexParseError("unmerged index path is invalid"))?;
        let object = std::str::from_utf8(object)
            .ok()
            .and_then(|object| ObjectId::parse(object).ok())
            .ok_or(IndexParseError("unmerged index object is invalid"))?;
        let mode = GitFileMode::from_octal_bytes(mode)
            .map_err(|_| IndexParseError("unmerged index mode is invalid"))?;
        let slot = usize::from(stage[0] - b'1');
        let stages = paths.entry(path).or_default();
        if stages[slot].replace(IndexStage { mode, object }).is_some() {
            return Err(IndexParseError("unmerged index contains a duplicate stage"));
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_byte_safe_unmerged_index_stages() {
        let oid1 = "1111111111111111111111111111111111111111";
        let oid2 = "2222222222222222222222222222222222222222";
        let bytes = format!("100644 {oid1} 1\tpath.rs\0100755 {oid2} 2\tpath.rs\0");
        let paths = parse_unmerged_index(bytes.as_bytes()).unwrap();
        let stages = paths.values().next().unwrap();
        assert_eq!(stages[0].as_ref().unwrap().mode, GitFileMode::REGULAR);
        assert_eq!(stages[1].as_ref().unwrap().mode, GitFileMode::EXECUTABLE);
        assert!(stages[2].is_none());
    }

    #[test]
    fn rejects_duplicate_or_invalid_stages() {
        let oid = "1111111111111111111111111111111111111111";
        let duplicate = format!("100644 {oid} 2\ta\0100644 {oid} 2\ta\0");
        assert!(parse_unmerged_index(duplicate.as_bytes()).is_err());
        let invalid = format!("100644 {oid} 4\ta\0");
        assert!(parse_unmerged_index(invalid.as_bytes()).is_err());
    }
}
