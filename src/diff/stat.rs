//! Patch preview statistics and byte-safe affected paths.
use super::model::DiffPath;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatParseErrorKind {
    InvalidMetadata,
    InvalidPath,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatParseError {
    pub kind: StatParseErrorKind,
    detail: &'static str,
}
impl StatParseError {
    fn new(kind: StatParseErrorKind, detail: &'static str) -> Self {
        Self { kind, detail }
    }
}
impl std::fmt::Display for StatParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.detail)
    }
}
impl std::error::Error for StatParseError {}

/// One bounded parsed `--stat` preview row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatchStatRow {
    pub path: String,
    /// Binary rows carry no line counts.
    pub binary: bool,
    pub changed: u64,
    pub added: u64,
    pub deleted: u64,
}

#[derive(Debug)]
pub struct NumstatEntry {
    pub binary: bool,
    pub paths: Vec<DiffPath>,
}

/// Parses byte-safe numstat paths and binary markers from `git apply --numstat -z`.
///
/// # Errors
/// Rejects records missing fields or containing invalid paths.
pub fn parse_numstat_entries(bytes: &[u8]) -> Result<Vec<NumstatEntry>, StatParseError> {
    let fields = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut entries = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let field = fields[index];
        index += 1;
        if field.is_empty() {
            continue;
        }
        let mut parts = field.splitn(3, |byte| *byte == b'\t');
        let added = parts.next().ok_or_else(invalid_numstat)?;
        let deleted = parts.next().ok_or_else(invalid_numstat)?;
        let path = parts.next().ok_or_else(invalid_numstat)?;
        let binary = added == b"-" && deleted == b"-";
        let paths = if path.is_empty() {
            let old = fields.get(index).copied().ok_or_else(invalid_numstat)?;
            let new = fields.get(index + 1).copied().ok_or_else(invalid_numstat)?;
            index += 2;
            vec![diff_path(old)?, diff_path(new)?]
        } else {
            vec![diff_path(path)?]
        };
        entries.push(NumstatEntry { binary, paths });
    }
    Ok(entries)
}

fn invalid_numstat() -> StatParseError {
    StatParseError::new(
        StatParseErrorKind::InvalidMetadata,
        "Git numstat output is malformed",
    )
}

fn diff_path(bytes: &[u8]) -> Result<DiffPath, StatParseError> {
    DiffPath::from_bytes(bytes.to_vec())
        .map_err(|_| StatParseError::new(StatParseErrorKind::InvalidPath, "patch path is invalid"))
}

/// Parses display-oriented `--stat` rows up to the caller's row limit.
/// The textual +/- graph counts are display widths, not exact line counts.
pub fn parse_stat(bytes: &[u8], max_rows: usize) -> (Vec<PatchStatRow>, bool) {
    let mut rows = Vec::new();
    let mut truncated = false;
    for line in String::from_utf8_lossy(bytes).lines() {
        let Some((path, counts)) = line.rsplit_once(" | ") else {
            continue;
        };
        if rows.len() == max_rows {
            truncated = true;
            break;
        }
        let counts = counts.trim();
        let binary = counts.starts_with("Bin ");
        let changed = counts
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        rows.push(PatchStatRow {
            path: path.trim().to_owned(),
            binary,
            changed,
            added: counts.bytes().filter(|byte| *byte == b'+').count() as u64,
            deleted: counts.bytes().filter(|byte| *byte == b'-').count() as u64,
        });
    }
    (rows, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numstat_keeps_binary_and_rename_paths_as_bytes() {
        let entries = parse_numstat_entries(b"-\t-\tbinary\0").unwrap();
        assert!(entries[0].binary);
        let entries = parse_numstat_entries(b"1\t2\t\0old/\xff\0new/\xfe\0").unwrap();
        assert_eq!(entries[0].paths[0].as_bytes(), b"old/\xff");
        assert_eq!(entries[0].paths[1].as_bytes(), b"new/\xfe");
        assert_eq!(
            parse_numstat_entries(b"1\t2\t\0old\0").unwrap_err().kind,
            StatParseErrorKind::InvalidPath
        );
    }

    #[test]
    fn stat_retains_display_graph_counts_and_reports_row_limit() {
        let (rows, truncated) = parse_stat(b" file | 12 +++--\n binary | Bin 0 -> 100 bytes\n", 1);
        assert!(truncated);
        assert_eq!(rows[0].changed, 12);
        assert_eq!((rows[0].added, rows[0].deleted), (3, 2));
        let (rows, truncated) = parse_stat(b" binary | Bin 0 -> 100 bytes\n", 1);
        assert!(!truncated);
        assert!(rows[0].binary);
    }
}
