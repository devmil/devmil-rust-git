//! Byte-preserving parsers for installed Git history plumbing.

use crate::types::{ObjectId, RefId};
use std::fmt;

pub mod blame;
pub mod cat_file;
pub mod commit;
pub mod merge_tree;
pub mod path_history;
pub mod reflog;
pub mod search;
pub mod summary;
pub mod tree;

/// A malformed byte-oriented Git history record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParseError(pub &'static str);

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for ParseError {}

pub const MAX_REV_LIST_RECORD_BYTES: usize = 16 * 1024;
pub const MAX_REF_FIELD_BYTES: usize = 4 * 1024;
pub const MAX_METADATA_FIELD_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevListRecord {
    pub id: ObjectId,
    pub parents: Vec<ObjectId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefRecord {
    pub target: ObjectId,
    pub reference: RefId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataRecord {
    pub id: ObjectId,
    pub author: Vec<u8>,
    pub author_time: i64,
    pub subject: Vec<u8>,
}

/// NUL-framed `for-each-ref` format consumed by [`RefParser`].
pub const FOR_EACH_REF_FORMAT: &str = "%(objectname)%00%(refname)%00";

/// `tformat` for one `git log -z --no-walk --stdin` batch, consumed by [`MetadataParser`].
/// `-z` supplies the fourth NUL terminator without inserting newlines.
pub const BATCH_METADATA_FORMAT: &str = "%H%x00%an%x00%at%x00%s";

/// `git log -z --format=<SUBJECT_LOG_FORMAT>` emits `<oid>\0<subject>` per commit, with NUL
/// between commits. Parse it with [`parse_subject_log`].
pub const SUBJECT_LOG_FORMAT: &str = "%H%x00%s";

/// Parses [`SUBJECT_LOG_FORMAT`] output into commits and subjects in output order. Empty
/// subjects are kept: every field is positional, so an empty field never shifts the pairing.
///
/// # Errors
/// Rejects an odd field count or a field that is not a full object ID.
pub fn parse_subject_log(bytes: &[u8]) -> Result<Vec<(ObjectId, Vec<u8>)>, ParseError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let mut fields: Vec<&[u8]> = bytes.split(|byte| *byte == 0).collect();
    // `-z` terminates the last record with NUL when Git uses `tformat`.
    if !fields.len().is_multiple_of(2) && fields.last().is_some_and(|field| field.is_empty()) {
        fields.pop();
    }
    if !fields.len().is_multiple_of(2) {
        return Err(ParseError("Git returned an incomplete subject log record"));
    }
    fields
        .chunks_exact(2)
        .map(|pair| {
            let id = std::str::from_utf8(pair[0])
                .ok()
                .and_then(|text| ObjectId::parse(text.trim()).ok())
                .ok_or(ParseError("Git returned an invalid subject log identity"))?;
            Ok((id, pair[1].to_vec()))
        })
        .collect()
}

pub fn rev_list_arguments() -> Vec<&'static str> {
    vec![
        "rev-list",
        "--parents",
        "--topo-order",
        "--missing=print",
        "--all",
    ]
}

pub fn ref_arguments() -> Vec<String> {
    vec![
        "for-each-ref".to_owned(),
        format!("--format={FOR_EACH_REF_FORMAT}"),
    ]
}

pub fn metadata_arguments() -> Vec<String> {
    vec![
        "log".to_owned(),
        "-z".to_owned(),
        "--no-walk=unsorted".to_owned(),
        "--stdin".to_owned(),
        format!("--format=tformat:{BATCH_METADATA_FORMAT}"),
    ]
}

/// Builds one full object ID per line for a bounded `git log --no-walk --stdin` batch.
pub fn batch_metadata_requests(ids: &[ObjectId]) -> Vec<u8> {
    let mut requests = Vec::with_capacity(ids.len() * 65);
    for id in ids {
        requests.extend_from_slice(id.as_str().as_bytes());
        requests.push(b'\n');
    }
    requests
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GraphParseError {
    RecordTooLong {
        maximum: usize,
    },
    EmptyField,
    InvalidObjectId(Vec<u8>),
    InvalidRef(Vec<u8>),
    InvalidTimestamp(Vec<u8>),
    UnexpectedFieldCount {
        expected_multiple: usize,
        actual: usize,
    },
    UnexpectedRecordCount {
        expected: usize,
        actual: usize,
    },
    UnreferencedMissingObject,
    ParserPoisoned,
}

impl fmt::Display for GraphParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RecordTooLong { maximum } => {
                write!(formatter, "Git graph record exceeds {maximum} bytes")
            }
            Self::EmptyField => formatter.write_str("Git graph output contains an empty field"),
            Self::InvalidObjectId(value) => write!(
                formatter,
                "invalid object ID in Git graph output: {value:?}"
            ),
            Self::InvalidRef(value) => {
                write!(formatter, "invalid ref in Git graph output: {value:?}")
            }
            Self::InvalidTimestamp(value) => write!(
                formatter,
                "invalid timestamp in Git graph output: {value:?}"
            ),
            Self::UnexpectedFieldCount {
                expected_multiple,
                actual,
            } => write!(
                formatter,
                "Git graph output has {actual} trailing fields; expected groups of {expected_multiple}"
            ),
            Self::UnexpectedRecordCount { expected, actual } => write!(
                formatter,
                "Git graph output has {actual} records; expected {expected}"
            ),
            Self::UnreferencedMissingObject => {
                formatter.write_str("Git graph output names an unreferenced missing object")
            }
            Self::ParserPoisoned => {
                formatter.write_str("Git graph parser cannot continue after an earlier error")
            }
        }
    }
}

impl std::error::Error for GraphParseError {}

/// Parses `git rev-list --parents` without decoding the stream as UTF-8.
#[derive(Debug, Default)]
pub struct RevListParser {
    partial: Vec<u8>,
    poisoned: bool,
}

impl RevListParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn retained_partial_bytes(&self) -> usize {
        self.partial.len()
    }

    /// Appends a stream chunk and returns every complete record.
    ///
    /// # Errors
    ///
    /// Returns a typed parse error for an oversized or malformed record, or after
    /// a previous error poisoned the parser.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<RevListRecord>, GraphParseError> {
        if self.poisoned {
            return Err(GraphParseError::ParserPoisoned);
        }
        let mut records = Vec::new();
        let mut start = 0;
        for (index, byte) in chunk.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            self.append_bounded(&chunk[start..index], MAX_REV_LIST_RECORD_BYTES)?;
            if !self.partial.is_empty() {
                match parse_rev_list_record(&self.partial) {
                    Ok(record) => records.push(record),
                    Err(error) => {
                        self.partial.clear();
                        self.poisoned = true;
                        return Err(error);
                    }
                }
            }
            self.partial.clear();
            start = index + 1;
        }
        self.append_bounded(&chunk[start..], MAX_REV_LIST_RECORD_BYTES)?;
        Ok(records)
    }

    /// Completes parsing, accepting a final record without a newline.
    ///
    /// # Errors
    ///
    /// Returns a typed parse error when the retained record is malformed or the
    /// parser was poisoned.
    pub fn finish(self) -> Result<Vec<RevListRecord>, GraphParseError> {
        if self.poisoned {
            return Err(GraphParseError::ParserPoisoned);
        }
        if self.partial.is_empty() {
            Ok(Vec::new())
        } else {
            parse_rev_list_record(&self.partial).map(|record| vec![record])
        }
    }

    fn append_bounded(&mut self, bytes: &[u8], maximum: usize) -> Result<(), GraphParseError> {
        if self.partial.len().saturating_add(bytes.len()) > maximum {
            self.partial.clear();
            self.poisoned = true;
            return Err(GraphParseError::RecordTooLong { maximum });
        }
        self.partial.extend_from_slice(bytes);
        Ok(())
    }
}

/// Parses one parent-list record.
///
/// # Errors
/// Rejects malformed full object IDs and empty fields.
pub fn parse_rev_list_record(bytes: &[u8]) -> Result<RevListRecord, GraphParseError> {
    let mut fields = bytes.split(|byte| *byte == b' ');
    let id = parse_object_id(fields.next().unwrap_or_default())?;
    let parents = fields.map(parse_object_id).collect::<Result<Vec<_>, _>>()?;
    Ok(RevListRecord { id, parents })
}

/// Parses repeated `<object-id>\0<full-ref-name>\0` records.
#[derive(Debug, Default)]
pub struct RefParser {
    fields: NulFields,
}

impl RefParser {
    /// Appends a NUL-framed ref stream chunk.
    ///
    /// # Errors
    ///
    /// Returns a typed parse error for invalid framing, IDs, ref names, or bounds.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<RefRecord>, GraphParseError> {
        let complete = self.fields.push(chunk, MAX_REF_FIELD_BYTES, 2)?;
        match parse_ref_fields(&complete) {
            Ok(records) => Ok(records),
            Err(error) => {
                self.fields.poisoned = true;
                Err(error)
            }
        }
    }

    /// Completes the NUL-framed ref stream.
    ///
    /// # Errors
    ///
    /// Returns a typed parse error when an incomplete or invalid record remains.
    pub fn finish(self) -> Result<Vec<RefRecord>, GraphParseError> {
        let fields = self.fields.finish(2)?;
        parse_ref_fields(&fields)
    }

    pub fn retained_partial_bytes(&self) -> usize {
        self.fields.partial.len()
    }
}

fn parse_ref_fields(fields: &[Vec<u8>]) -> Result<Vec<RefRecord>, GraphParseError> {
    if !fields.len().is_multiple_of(2) {
        return Err(GraphParseError::UnexpectedFieldCount {
            expected_multiple: 2,
            actual: fields.len(),
        });
    }
    fields
        .chunks_exact(2)
        .map(|fields| {
            let target = parse_object_id(&fields[0])?;
            let reference = RefId::from_bytes(fields[1].clone())
                .map_err(|_| GraphParseError::InvalidRef(fields[1].clone()))?;
            Ok(RefRecord { target, reference })
        })
        .collect()
}

/// Parses NUL-framed cat-file metadata fields: id, author, Unix time, subject.
#[derive(Debug, Default)]
pub struct MetadataParser {
    fields: NulFields,
}

impl MetadataParser {
    /// Appends a NUL-framed metadata stream chunk.
    ///
    /// # Errors
    ///
    /// Returns a typed parse error for invalid framing, IDs, timestamps, or bounds.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<MetadataRecord>, GraphParseError> {
        let complete = self.fields.push(chunk, MAX_METADATA_FIELD_BYTES, 4)?;
        match parse_metadata_fields(&complete) {
            Ok(records) => Ok(records),
            Err(error) => {
                self.fields.poisoned = true;
                Err(error)
            }
        }
    }

    /// Completes the NUL-framed metadata stream.
    ///
    /// # Errors
    ///
    /// Returns a typed parse error when an incomplete or invalid record remains.
    pub fn finish(self) -> Result<Vec<MetadataRecord>, GraphParseError> {
        let fields = self.fields.finish(4)?;
        parse_metadata_fields(&fields)
    }

    pub fn retained_partial_bytes(&self) -> usize {
        self.fields.partial.len()
    }
}

fn parse_metadata_fields(fields: &[Vec<u8>]) -> Result<Vec<MetadataRecord>, GraphParseError> {
    if !fields.len().is_multiple_of(4) {
        return Err(GraphParseError::UnexpectedFieldCount {
            expected_multiple: 4,
            actual: fields.len(),
        });
    }
    fields
        .chunks_exact(4)
        .map(|fields| {
            let id = parse_object_id(&fields[0])?;
            let author_time = std::str::from_utf8(&fields[2])
                .ok()
                .and_then(|value| value.parse().ok())
                .ok_or_else(|| GraphParseError::InvalidTimestamp(fields[2].clone()))?;
            Ok(MetadataRecord {
                id,
                author: fields[1].clone(),
                author_time,
                subject: fields[3].clone(),
            })
        })
        .collect()
}

#[derive(Debug, Default)]
struct NulFields {
    partial: Vec<u8>,
    pending: Vec<Vec<u8>>,
    poisoned: bool,
}

impl NulFields {
    fn push(
        &mut self,
        chunk: &[u8],
        maximum: usize,
        arity: usize,
    ) -> Result<Vec<Vec<u8>>, GraphParseError> {
        if self.poisoned {
            return Err(GraphParseError::ParserPoisoned);
        }
        let mut start = 0;
        for (index, byte) in chunk.iter().enumerate() {
            if *byte != 0 {
                continue;
            }
            self.append_bounded(&chunk[start..index], maximum)?;
            if self.partial.is_empty() {
                self.poisoned = true;
                return Err(GraphParseError::EmptyField);
            }
            self.pending.push(std::mem::take(&mut self.partial));
            start = index + 1;
        }
        self.append_bounded(&chunk[start..], maximum)?;

        let complete_count = self.pending.len() / arity * arity;
        Ok(self.pending.drain(..complete_count).collect())
    }

    fn append_bounded(&mut self, bytes: &[u8], maximum: usize) -> Result<(), GraphParseError> {
        if self.partial.len().saturating_add(bytes.len()) > maximum {
            self.partial.clear();
            self.pending.clear();
            self.poisoned = true;
            return Err(GraphParseError::RecordTooLong { maximum });
        }
        self.partial.extend_from_slice(bytes);
        Ok(())
    }

    fn finish(mut self, expected_multiple: usize) -> Result<Vec<Vec<u8>>, GraphParseError> {
        if self.poisoned {
            return Err(GraphParseError::ParserPoisoned);
        }
        if self.partial == b"\n" || self.partial == b"\r\n" {
            self.partial.clear();
        }
        if !self.partial.is_empty() || !self.pending.len().is_multiple_of(expected_multiple) {
            return Err(GraphParseError::UnexpectedFieldCount {
                expected_multiple,
                actual: self.pending.len() + usize::from(!self.partial.is_empty()),
            });
        }
        Ok(self.pending)
    }
}

/// Parses an object ID with an optional record-separating newline prefix.
///
/// # Errors
/// Rejects malformed full object IDs and empty fields.
pub fn parse_object_id(bytes: &[u8]) -> Result<ObjectId, GraphParseError> {
    let bytes = bytes
        .strip_prefix(b"\r\n")
        .or_else(|| bytes.strip_prefix(b"\n"))
        .unwrap_or(bytes);
    if bytes.is_empty() {
        return Err(GraphParseError::EmptyField);
    }
    let value =
        std::str::from_utf8(bytes).map_err(|_| GraphParseError::InvalidObjectId(bytes.to_vec()))?;
    ObjectId::parse(value).map_err(|_| GraphParseError::InvalidObjectId(bytes.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: &str = "0000000000000000000000000000000000000001";
    const B: &str = "0000000000000000000000000000000000000002";
    const C: &str = "0000000000000000000000000000000000000003";
    #[test]
    fn rev_list_is_identical_for_every_chunk_boundary() {
        let bytes = format!("{A} {B} {C}\n{B}\n").into_bytes();
        let expected = parse_rev_list_in_chunks(&bytes, bytes.len());
        for chunk_size in 1..=bytes.len() {
            assert_eq!(parse_rev_list_in_chunks(&bytes, chunk_size), expected);
        }
        assert_eq!(
            expected[0].parents,
            vec![ObjectId::parse(B).unwrap(), ObjectId::parse(C).unwrap()]
        );
    }

    fn parse_rev_list_in_chunks(bytes: &[u8], chunk_size: usize) -> Vec<RevListRecord> {
        let mut parser = RevListParser::new();
        let mut records = Vec::new();
        for chunk in bytes.chunks(chunk_size) {
            records.extend(parser.push(chunk).unwrap());
            assert!(parser.retained_partial_bytes() <= MAX_REV_LIST_RECORD_BYTES);
        }
        records.extend(parser.finish().unwrap());
        records
    }

    #[test]
    fn rev_list_accepts_a_final_record_without_newline() {
        let mut parser = RevListParser::new();
        assert!(parser.push(A.as_bytes()).unwrap().is_empty());
        assert_eq!(parser.finish().unwrap().len(), 1);
    }

    #[test]
    fn malformed_and_oversized_rev_list_records_are_typed() {
        let mut invalid = RevListParser::new();
        assert!(matches!(
            invalid.push(b"not-an-object\n"),
            Err(GraphParseError::InvalidObjectId(_))
        ));
        assert_eq!(
            invalid.push(A.as_bytes()),
            Err(GraphParseError::ParserPoisoned)
        );

        let mut oversized = RevListParser::new();
        assert_eq!(
            oversized.push(&vec![b'a'; MAX_REV_LIST_RECORD_BYTES + 1]),
            Err(GraphParseError::RecordTooLong {
                maximum: MAX_REV_LIST_RECORD_BYTES
            })
        );
        assert_eq!(oversized.retained_partial_bytes(), 0);
        assert_eq!(
            oversized.push(b"anything"),
            Err(GraphParseError::ParserPoisoned)
        );
    }

    #[test]
    fn refs_preserve_non_utf8_names_across_chunks() {
        let mut bytes = A.as_bytes().to_vec();
        bytes.extend_from_slice(b"\0refs/heads/non-utf8-\xff\0");
        let mut parser = RefParser::default();
        let mut records = Vec::new();
        for chunk in bytes.chunks(3) {
            records.extend(parser.push(chunk).unwrap());
            assert!(parser.retained_partial_bytes() <= MAX_REF_FIELD_BYTES);
        }
        records.extend(parser.finish().unwrap());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].reference.as_bytes(), b"refs/heads/non-utf8-\xff");
    }

    #[test]
    fn refs_ignore_for_each_ref_record_newlines() {
        let bytes = format!("{A}\0refs/heads/main\0\n{B}\0refs/tags/v1\0\n").into_bytes();
        let mut parser = RefParser::default();
        let mut records = Vec::new();
        for chunk in bytes.chunks(7) {
            records.extend(parser.push(chunk).unwrap());
        }
        records.extend(parser.finish().unwrap());
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].target, ObjectId::parse(B).unwrap());
        assert_eq!(records[1].reference.as_bytes(), b"refs/tags/v1");
    }

    #[test]
    fn metadata_parser_retains_incomplete_groups_and_raw_display_bytes() {
        let bytes = [A.as_bytes(), b"A\xffuthor", b"1704067200", b"subject\xff"]
            .into_iter()
            .flat_map(|field| field.iter().copied().chain([0]))
            .collect::<Vec<_>>();
        let mut parser = MetadataParser::default();
        let split = bytes.len() / 2;
        assert!(parser.push(&bytes[..split]).unwrap().is_empty());
        let records = parser.push(&bytes[split..]).unwrap();
        assert_eq!(records[0].author, b"A\xffuthor");
        assert_eq!(records[0].subject, b"subject\xff");
        assert!(parser.finish().unwrap().is_empty());
    }

    #[test]
    fn batch_metadata_requests_use_full_ids_and_one_command_per_line() {
        let ids = [ObjectId::parse(A).unwrap(), ObjectId::parse(B).unwrap()];
        assert_eq!(
            batch_metadata_requests(&ids),
            format!("{A}\n{B}\n").into_bytes()
        );
        assert!(FOR_EACH_REF_FORMAT.contains("%00"));
        assert!(BATCH_METADATA_FORMAT.ends_with("%s"));
        assert!(rev_list_arguments().contains(&"--parents"));
        assert!(!ref_arguments().iter().any(|argument| argument == "-z"));
        assert!(metadata_arguments()
            .iter()
            .any(|argument| argument == "--no-walk=unsorted"));
    }

    #[test]
    fn incomplete_nul_records_are_rejected_at_finish() {
        let mut refs = RefParser::default();
        assert!(refs.push(format!("{A}\0").as_bytes()).unwrap().is_empty());
        assert_eq!(
            refs.finish(),
            Err(GraphParseError::UnexpectedFieldCount {
                expected_multiple: 2,
                actual: 1
            })
        );
    }
}
