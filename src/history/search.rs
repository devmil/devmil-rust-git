//! Git log search records and bisect-log markers.
use super::ParseError;
use crate::types::ObjectId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffSearchMatch {
    pub commit: ObjectId,
    pub summary: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BisectMark {
    Good,
    Bad,
    Skip,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BisectEntry {
    pub revision: ObjectId,
    pub mark: BisectMark,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BisectParseError {
    Malformed(ParseError),
    TooManyEntries,
}

impl std::fmt::Display for BisectParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(error) => std::fmt::Display::fmt(error, formatter),
            Self::TooManyEntries => {
                formatter.write_str("bisect timeline exceeds the supported entry bound")
            }
        }
    }
}
impl std::error::Error for BisectParseError {}

/// Parses repeated NUL-separated full object IDs and subjects.
///
/// # Errors
/// Rejects invalid object identities and records lacking a subject field.
pub fn parse_diff_search_matches(bytes: &[u8]) -> Result<Vec<DiffSearchMatch>, ParseError> {
    let fields = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut matches = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let oid = fields[index]
            .iter()
            .copied()
            .skip_while(u8::is_ascii_whitespace)
            .collect::<Vec<_>>();
        if oid.is_empty() {
            index += 1;
            continue;
        }
        let summary = fields.get(index + 1).ok_or(ParseError(
            "Git returned a diff-search row without a subject",
        ))?;
        matches.push(DiffSearchMatch {
            commit: parse_one_oid(&oid)?,
            summary: summary.to_vec(),
        });
        index += 2;
    }
    Ok(matches)
}

/// Parses comment records written by `git bisect log`.
///
/// # Errors
/// Rejects malformed marked entries, non-UTF-8 logs, and counts above `maximum`.
pub fn parse_bisect_log(
    bytes: &[u8],
    maximum: usize,
) -> Result<Vec<BisectEntry>, BisectParseError> {
    let malformed = |detail| BisectParseError::Malformed(ParseError(detail));
    let text =
        std::str::from_utf8(bytes).map_err(|_| malformed("Git returned a non-UTF-8 bisect log"))?;
    let mut timeline = Vec::new();
    for line in text.lines() {
        let Some((mark, rest)) = [
            (BisectMark::Good, "# good: ["),
            (BisectMark::Bad, "# bad: ["),
            (BisectMark::Skip, "# skip: ["),
        ]
        .into_iter()
        .find_map(|(mark, prefix)| line.strip_prefix(prefix).map(|rest| (mark, rest))) else {
            continue;
        };
        let oid = rest
            .split_once(']')
            .map(|(oid, _)| oid)
            .ok_or_else(|| malformed("Git returned a malformed bisect log entry"))?;
        timeline.push(BisectEntry {
            revision: ObjectId::parse(oid)
                .map_err(|_| malformed("Git returned a malformed bisect log identity"))?,
            mark,
        });
        if timeline.len() > maximum {
            return Err(BisectParseError::TooManyEntries);
        }
    }
    Ok(timeline)
}

/// Parses a full object ID surrounded by whitespace.
///
/// # Errors
/// Rejects non-UTF-8 or malformed full object IDs.
pub fn parse_one_oid(bytes: &[u8]) -> Result<ObjectId, ParseError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ParseError("Git returned a non-UTF-8 object identity"))?
        .trim();
    ObjectId::parse(text).map_err(|_| ParseError("Git returned an invalid object identity"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bisect_records_are_bounded_and_search_subjects_stay_byte_safe() {
        let id = "1".repeat(40);
        let log = format!("git bisect start\n# good: [{id}] base\n# skip: [{id}] flaky\n");
        let entries = parse_bisect_log(log.as_bytes(), 2).unwrap();
        assert_eq!(entries[0].mark, BisectMark::Good);
        assert_eq!(entries[1].mark, BisectMark::Skip);
        assert_eq!(
            parse_bisect_log(log.as_bytes(), 1),
            Err(BisectParseError::TooManyEntries)
        );
        let mut bytes = format!("{id}\0").into_bytes();
        bytes.extend_from_slice(b"subject\xff\0");
        assert_eq!(
            parse_diff_search_matches(&bytes).unwrap()[0].summary,
            b"subject\xff"
        );
    }
}
