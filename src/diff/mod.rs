//! Byte-safe installed-Git diff parsing and partial patch construction.

mod contracts;
pub mod index;
pub mod intra_line;
pub mod model;
pub mod source;
pub mod stat;
#[cfg(feature = "syntax-highlighting")]
pub mod syntax;
pub use contracts::{ConstructedPatch, DiffError, DiffErrorKind, PatchConstructionRequest};

use crate::diff::model::{
    DiffDocument, DiffFallback, DiffFile, DiffFileId, DiffHunk, DiffLayout, DiffLine,
    DiffLineAnnotation, DiffLineId, DiffLineOrigin, DiffNumStat, DiffPath, DiffStatCount,
    FileChangeKind, FileStagingState, GitFileMode, HunkId, LineEnding, SyntaxAvailability,
};
use crate::types::{ObjectFormat, ObjectId};
use std::collections::BTreeMap;

pub const MAX_METADATA_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_METADATA_RECORDS: usize = 65_536;
pub const MAX_DIFF_PATH_BYTES: usize = 64 * 1024;
pub const MAX_COMBINED_PARENTS: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RawObjectId {
    Object(ObjectId),
    Null(ObjectFormat),
}

impl RawObjectId {
    const fn format(&self) -> ObjectFormat {
        match self {
            Self::Object(object) => object.format(),
            Self::Null(format) => *format,
        }
    }
}

/// Git raw status interpreted together with the old and new modes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RawFileChangeKind {
    Modified,
    ModeChanged,
    TypeChanged,
    Added,
    Deleted,
    Renamed { similarity: u8 },
    Copied { similarity: u8 },
    Conflict,
}

/// Scoreless per-parent status from Git's combined raw format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CombinedRawStatus {
    Modified,
    ModeChanged,
    TypeChanged,
    Added,
    Deleted,
    Renamed,
    Copied,
    Conflict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawDiffEntry {
    pub old_mode: GitFileMode,
    pub new_mode: GitFileMode,
    pub old_object: RawObjectId,
    pub new_object: RawObjectId,
    pub kind: FileChangeKind,
    pub raw_kind: RawFileChangeKind,
    pub old_path: DiffPath,
    pub new_path: DiffPath,
}

/// Display metadata from standard `--raw -z -c --no-abbrev` output.
///
/// Combined patch hunks have one source column per parent and are intentionally not represented by
/// [`DiffHunk`], whose stable coordinates describe a two-sided patch. Consumers can still display
/// the parent/result modes, object identities, statuses, and byte-safe path from this record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CombinedRawDiffEntry {
    pub parent_modes: Vec<GitFileMode>,
    pub result_mode: GitFileMode,
    pub parent_objects: Vec<RawObjectId>,
    pub result_object: RawObjectId,
    pub parent_statuses: Vec<CombinedRawStatus>,
    pub path: DiffPath,
}

/// Path-shaped `--numstat -z` record before status metadata supplies added/deleted semantics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawNumStatEntry {
    pub old_path: DiffPath,
    pub new_path: DiffPath,
    pub added: DiffStatCount,
    pub deleted: DiffStatCount,
}

/// Parses `git diff --raw -z` records without decoding paths as text.
///
/// # Errors
///
/// Returns a bounded metadata error for malformed records or incomplete NUL streams.
pub fn parse_raw_diff_z(input: &[u8]) -> Result<Vec<RawDiffEntry>, DiffError> {
    check_metadata_bytes(input)?;
    if exceeds_nul_bound(input, MAX_METADATA_RECORDS.saturating_mul(3)) {
        return Err(metadata_too_large(
            "raw stream exceeds the record-field bound",
        ));
    }
    if !input.is_empty() && !input.ends_with(&[0]) {
        return Err(malformed_metadata("raw stream has no final NUL terminator"));
    }
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let mut fields: Vec<&[u8]> = input.split(|byte| *byte == 0).collect();
    fields.pop();
    if fields.is_empty() || fields.iter().any(|field| field.is_empty()) {
        return Err(malformed_metadata("raw stream contains an empty NUL field"));
    }
    let mut cursor = 0;
    let mut entries = Vec::new();
    let mut object_format = None;
    while cursor < fields.len() && !fields[cursor].is_empty() {
        check_record_count(entries.len())?;
        let header = fields[cursor];
        cursor += 1;
        let mut parts = header.split(|byte| *byte == b' ');
        let old_mode = parts
            .next()
            .and_then(|part| part.strip_prefix(b":"))
            .ok_or_else(|| malformed_metadata("raw record has no old mode"))?;
        let new_mode = parts
            .next()
            .ok_or_else(|| malformed_metadata("raw record has no new mode"))?;
        let old_object = parts
            .next()
            .ok_or_else(|| malformed_metadata("raw record has no old object"))?;
        let new_object = parts
            .next()
            .ok_or_else(|| malformed_metadata("raw record has no new object"))?;
        let status = parts
            .next()
            .ok_or_else(|| malformed_metadata("raw record has no status"))?;
        if parts.next().is_some() {
            return Err(malformed_metadata("raw record has extra header fields"));
        }
        let old_object = parse_raw_object(old_object)?;
        let new_object = parse_raw_object(new_object)?;
        observe_object_format(&mut object_format, old_object.format())?;
        observe_object_format(&mut object_format, new_object.format())?;
        let old_mode = GitFileMode::from_octal_bytes(old_mode)
            .map_err(|_| malformed_metadata("raw record has an invalid old mode"))?;
        let new_mode = GitFileMode::from_octal_bytes(new_mode)
            .map_err(|_| malformed_metadata("raw record has an invalid new mode"))?;
        let (raw_kind, needs_two_paths) = parse_status(status, old_mode, new_mode)?;
        let kind = display_kind(&raw_kind);
        let first = fields
            .get(cursor)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| malformed_metadata("raw record has no path"))?;
        cursor += 1;
        let second = if needs_two_paths {
            let path = fields
                .get(cursor)
                .filter(|path| !path.is_empty())
                .ok_or_else(|| malformed_metadata("rename or copy record has no target path"))?;
            cursor += 1;
            *path
        } else {
            *first
        };
        let old_path = parse_metadata_path(first)?;
        let new_path = parse_metadata_path(second)?;
        entries.push(RawDiffEntry {
            old_mode,
            new_mode,
            old_object,
            new_object,
            kind,
            raw_kind,
            old_path,
            new_path,
        });
    }
    if cursor != fields.len() {
        return Err(malformed_metadata(
            "raw stream contains data after its terminator",
        ));
    }
    Ok(entries)
}

/// Parses `git diff --numstat -z` without decoding paths as text.
///
/// Ordinary records carry their path in the header field. Rename and copy records carry an empty
/// header path followed by old and new NUL-terminated paths. A binary record must use `-` for both
/// counts; mixed numeric/binary counts are rejected rather than assigned misleading totals.
///
/// # Errors
///
/// Returns a bounded metadata error for malformed counts, paths, or incomplete NUL streams.
pub fn parse_numstat_z(input: &[u8]) -> Result<Vec<RawNumStatEntry>, DiffError> {
    let fields = nul_fields(input, "numstat stream has no final NUL terminator")?;
    let mut cursor = 0;
    let mut entries = Vec::new();
    while cursor < fields.len() {
        check_record_count(entries.len())?;
        let header = fields[cursor];
        cursor += 1;
        let mut parts = header.splitn(3, |byte| *byte == b'\t');
        let added = parse_stat_count(
            parts
                .next()
                .ok_or_else(|| malformed_metadata("numstat record has no added count"))?,
        )?;
        let deleted = parse_stat_count(
            parts
                .next()
                .ok_or_else(|| malformed_metadata("numstat record has no deleted count"))?,
        )?;
        if matches!(added, DiffStatCount::Binary) != matches!(deleted, DiffStatCount::Binary) {
            return Err(malformed_metadata(
                "numstat binary marker must appear for both counts",
            ));
        }
        let path = parts
            .next()
            .ok_or_else(|| malformed_metadata("numstat record has no path field"))?;
        let (old_path, new_path) = if path.is_empty() {
            let old = fields
                .get(cursor)
                .filter(|path| !path.is_empty())
                .ok_or_else(|| malformed_metadata("numstat rename has no old path"))?;
            cursor += 1;
            let new = fields
                .get(cursor)
                .filter(|path| !path.is_empty())
                .ok_or_else(|| malformed_metadata("numstat rename has no new path"))?;
            cursor += 1;
            (parse_metadata_path(old)?, parse_metadata_path(new)?)
        } else {
            let path = parse_metadata_path(path)?;
            (path.clone(), path)
        };
        entries.push(RawNumStatEntry {
            old_path,
            new_path,
            added,
            deleted,
        });
    }
    Ok(entries)
}

/// Joins ordered raw and numstat streams captured from the same frozen diff request and options.
/// Repeated conflict-stage records retain their Git order through a per-file ordinal, so the pair
/// `(file, ordinal)` is unique without aggregating or losing line totals.
///
/// # Errors
///
/// Returns a bounded metadata error when record counts or byte paths disagree.
pub fn parse_numstat_z_with_raw_metadata(
    raw_input: &[u8],
    numstat_input: &[u8],
) -> Result<Vec<DiffNumStat>, DiffError> {
    let raw = parse_raw_diff_z(raw_input)?;
    let stats = parse_numstat_z(numstat_input)?;
    if raw.len() != stats.len() {
        return Err(malformed_metadata(
            "raw metadata and numstat contain different file counts",
        ));
    }
    let mut occurrences = BTreeMap::<DiffFileId, u32>::new();
    raw.iter()
        .zip(stats)
        .map(|(raw, stats)| {
            if raw.old_path != stats.old_path || raw.new_path != stats.new_path {
                return Err(malformed_metadata(
                    "raw metadata and numstat paths disagree",
                ));
            }
            let file = raw_file_id(raw);
            let ordinal = occurrences.entry(file.clone()).or_default();
            let current_ordinal = *ordinal;
            *ordinal = ordinal.saturating_add(1);
            Ok(DiffNumStat {
                file,
                ordinal: current_ordinal,
                kind: raw.kind.clone(),
                added: stats.added,
                deleted: stats.deleted,
            })
        })
        .collect()
}

fn raw_file_id(entry: &RawDiffEntry) -> DiffFileId {
    DiffFileId {
        old_path: (!matches!(entry.raw_kind, RawFileChangeKind::Added))
            .then(|| entry.old_path.clone()),
        new_path: (!matches!(entry.raw_kind, RawFileChangeKind::Deleted))
            .then(|| entry.new_path.clone()),
    }
}

/// Parses standard one-path combined raw metadata emitted by
/// `git diff --raw -z -c --no-abbrev`.
///
/// `--combined-all-paths` is a different record shape and must be requested through a future typed
/// API if the UI needs parent-specific path display. Combined hunk bodies (`@@@`) likewise remain
/// outside the two-sided [`DiffDocument`] contract.
///
/// # Errors
///
/// Returns a bounded metadata error for malformed parent arity, modes, object IDs, statuses, paths,
/// or incomplete NUL streams.
pub fn parse_combined_raw_diff_z(input: &[u8]) -> Result<Vec<CombinedRawDiffEntry>, DiffError> {
    let fields = nul_fields(input, "combined raw stream has no final NUL terminator")?;
    let mut cursor = 0;
    let mut entries = Vec::new();
    let mut object_format = None;
    while cursor < fields.len() {
        check_record_count(entries.len())?;
        let header = fields[cursor];
        cursor += 1;
        let parent_count = header.iter().take_while(|byte| **byte == b':').count();
        if parent_count < 2 {
            return Err(malformed_metadata(
                "combined raw record requires at least two parents",
            ));
        }
        if parent_count > MAX_COMBINED_PARENTS {
            return Err(metadata_too_large(
                "combined raw record exceeds the parent bound",
            ));
        }
        let mut parts = header[parent_count..].split(|byte| *byte == b' ');
        let mut modes = Vec::with_capacity(parent_count + 1);
        for _ in 0..=parent_count {
            modes.push(parse_metadata_mode(parts.next())?);
        }
        let result_mode = modes
            .pop()
            .ok_or_else(|| malformed_metadata("combined raw record lacks a result mode"))?;

        let mut objects = Vec::with_capacity(parent_count + 1);
        for _ in 0..=parent_count {
            let object = parts
                .next()
                .ok_or_else(|| malformed_metadata("combined raw record lacks an object ID"))?;
            let object = parse_raw_object(object)?;
            observe_object_format(&mut object_format, object.format())?;
            objects.push(object);
        }
        let result_object = objects
            .pop()
            .ok_or_else(|| malformed_metadata("combined raw record lacks a result object"))?;
        let statuses = parts
            .next()
            .ok_or_else(|| malformed_metadata("combined raw record lacks parent statuses"))?;
        if parts.next().is_some() || statuses.len() != parent_count {
            return Err(malformed_metadata(
                "combined raw record has invalid field or status arity",
            ));
        }
        let parent_statuses = statuses
            .iter()
            .zip(&modes)
            .map(|(status, parent_mode)| parse_combined_status(*status, *parent_mode, result_mode))
            .collect::<Result<Vec<_>, _>>()?;
        let path = fields
            .get(cursor)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| malformed_metadata("combined raw record has no path"))?;
        cursor += 1;
        entries.push(CombinedRawDiffEntry {
            parent_modes: modes,
            result_mode,
            parent_objects: objects,
            result_object,
            parent_statuses,
            path: parse_metadata_path(path)?,
        });
    }
    Ok(entries)
}

fn nul_fields<'a>(
    input: &'a [u8],
    missing_terminator: &'static str,
) -> Result<Vec<&'a [u8]>, DiffError> {
    check_metadata_bytes(input)?;
    if input.is_empty() {
        return Ok(Vec::new());
    }
    if !input.ends_with(&[0]) {
        return Err(malformed_metadata(missing_terminator));
    }
    if exceeds_nul_bound(input, MAX_METADATA_RECORDS.saturating_mul(3)) {
        return Err(metadata_too_large(
            "metadata stream exceeds the record-field bound",
        ));
    }
    let mut fields = input.split(|byte| *byte == 0).collect::<Vec<_>>();
    fields.pop();
    if fields.iter().any(|field| field.is_empty()) {
        // Numstat rename headers intentionally end in a tab, not a NUL, so their header field is
        // nonempty. Empty NUL fields never carry information in the supported stream shapes.
        return Err(malformed_metadata(
            "metadata stream contains an empty NUL field",
        ));
    }
    Ok(fields)
}

fn parse_metadata_path(value: &[u8]) -> Result<DiffPath, DiffError> {
    if value.len() > MAX_DIFF_PATH_BYTES {
        return Err(metadata_too_large(
            "metadata path exceeds the supported bound",
        ));
    }
    DiffPath::from_bytes(value.to_vec())
        .map_err(|_| malformed_metadata("metadata record contains an invalid path"))
}

fn parse_stat_count(value: &[u8]) -> Result<DiffStatCount, DiffError> {
    if value == b"-" {
        return Ok(DiffStatCount::Binary);
    }
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(malformed_metadata("numstat count is not a decimal integer"));
    }
    std::str::from_utf8(value)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(DiffStatCount::Lines)
        .ok_or_else(|| malformed_metadata("numstat count exceeds the supported range"))
}

fn parse_metadata_mode(value: Option<&[u8]>) -> Result<GitFileMode, DiffError> {
    value
        .ok_or_else(|| malformed_metadata("combined raw record lacks a mode"))
        .and_then(|value| {
            GitFileMode::from_octal_bytes(value)
                .map_err(|_| malformed_metadata("combined raw record has an invalid mode"))
        })
}

fn mode_change_kind(old_mode: GitFileMode, new_mode: GitFileMode) -> RawFileChangeKind {
    let old_type = old_mode.value() & 0o170_000;
    let new_type = new_mode.value() & 0o170_000;
    if old_type != 0 && new_type != 0 && old_type != new_type {
        RawFileChangeKind::TypeChanged
    } else if old_mode != new_mode {
        RawFileChangeKind::ModeChanged
    } else {
        RawFileChangeKind::Modified
    }
}

fn parse_combined_status(
    status: u8,
    parent_mode: GitFileMode,
    result_mode: GitFileMode,
) -> Result<CombinedRawStatus, DiffError> {
    match status {
        b'M' => {
            let parent_type = parent_mode.value() & 0o170_000;
            let result_type = result_mode.value() & 0o170_000;
            Ok(
                if parent_type != 0 && result_type != 0 && parent_type != result_type {
                    CombinedRawStatus::TypeChanged
                } else if parent_mode != result_mode {
                    CombinedRawStatus::ModeChanged
                } else {
                    CombinedRawStatus::Modified
                },
            )
        }
        b'A' => Ok(CombinedRawStatus::Added),
        b'D' => Ok(CombinedRawStatus::Deleted),
        b'T' => Ok(CombinedRawStatus::TypeChanged),
        b'R' => Ok(CombinedRawStatus::Renamed),
        b'C' => Ok(CombinedRawStatus::Copied),
        b'U' => Ok(CombinedRawStatus::Conflict),
        _ => Err(malformed_metadata(
            "combined raw record has an unsupported parent status",
        )),
    }
}

fn check_metadata_bytes(input: &[u8]) -> Result<(), DiffError> {
    if input.len() > MAX_METADATA_BYTES {
        return Err(metadata_too_large("metadata stream exceeds the byte bound"));
    }
    Ok(())
}

fn exceeds_nul_bound(input: &[u8], bound: usize) -> bool {
    let mut count = 0_usize;
    for byte in input {
        if *byte == 0 {
            count += 1;
            if count > bound {
                return true;
            }
        }
    }
    false
}

fn check_record_count(current: usize) -> Result<(), DiffError> {
    if current >= MAX_METADATA_RECORDS {
        return Err(metadata_too_large(
            "metadata stream exceeds the record bound",
        ));
    }
    Ok(())
}

fn observe_object_format(
    expected: &mut Option<ObjectFormat>,
    observed: ObjectFormat,
) -> Result<(), DiffError> {
    match expected {
        Some(expected) if *expected != observed => Err(malformed_metadata(
            "metadata stream mixes Git object formats",
        )),
        Some(_) => Ok(()),
        None => {
            *expected = Some(observed);
            Ok(())
        }
    }
}

fn parse_raw_object(value: &[u8]) -> Result<RawObjectId, DiffError> {
    let format = match value.len() {
        40 => ObjectFormat::Sha1,
        64 => ObjectFormat::Sha256,
        _ => {
            return Err(malformed_metadata(
                "raw record has an invalid full object ID",
            ));
        }
    };
    if !value.iter().all(u8::is_ascii_hexdigit) {
        return Err(malformed_metadata(
            "raw record has an invalid full object ID",
        ));
    }
    if value.iter().all(|byte| *byte == b'0') {
        return Ok(RawObjectId::Null(format));
    }
    let value =
        std::str::from_utf8(value).map_err(|_| malformed_metadata("raw object ID is not ASCII"))?;
    ObjectId::parse(value)
        .map(RawObjectId::Object)
        .map_err(|_| malformed_metadata("raw record has an invalid full object ID"))
}

fn parse_status(
    status: &[u8],
    old_mode: GitFileMode,
    new_mode: GitFileMode,
) -> Result<(RawFileChangeKind, bool), DiffError> {
    let Some(code) = status.first() else {
        return Err(malformed_metadata("raw record has an empty status"));
    };
    let score = || -> Result<u8, DiffError> {
        let value = std::str::from_utf8(&status[1..])
            .ok()
            .and_then(|value| value.parse::<u8>().ok())
            .filter(|value| *value <= 100)
            .ok_or_else(|| malformed_metadata("rename or copy score is invalid"))?;
        Ok(value)
    };
    match code {
        b'M' if status.len() == 1 => Ok((mode_change_kind(old_mode, new_mode), false)),
        b'M' => {
            score()?;
            Ok((mode_change_kind(old_mode, new_mode), false))
        }
        b'A' if status.len() == 1 => Ok((RawFileChangeKind::Added, false)),
        b'D' if status.len() == 1 => Ok((RawFileChangeKind::Deleted, false)),
        b'T' if status.len() == 1 => Ok((RawFileChangeKind::TypeChanged, false)),
        b'U' if status.len() == 1 => Ok((RawFileChangeKind::Conflict, false)),
        b'R' => Ok((
            RawFileChangeKind::Renamed {
                similarity: score()?,
            },
            true,
        )),
        b'C' => Ok((
            RawFileChangeKind::Copied {
                similarity: score()?,
            },
            true,
        )),
        _ => Err(malformed_metadata("raw record has an unsupported status")),
    }
}

fn display_kind(kind: &RawFileChangeKind) -> FileChangeKind {
    match kind {
        RawFileChangeKind::Modified => FileChangeKind::Modified,
        RawFileChangeKind::ModeChanged => FileChangeKind::ModeChanged,
        RawFileChangeKind::TypeChanged => FileChangeKind::TypeChanged,
        RawFileChangeKind::Added => FileChangeKind::Added,
        RawFileChangeKind::Deleted => FileChangeKind::Deleted,
        RawFileChangeKind::Renamed { similarity } => FileChangeKind::Renamed {
            similarity: *similarity,
        },
        RawFileChangeKind::Copied { similarity } => FileChangeKind::Copied {
            similarity: *similarity,
        },
        RawFileChangeKind::Conflict => FileChangeKind::Conflict,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PatchLimits {
    pub max_bytes: usize,
    pub max_lines: usize,
    pub preview_line_limit: u32,
}

impl Default for PatchLimits {
    fn default() -> Self {
        Self {
            max_bytes: 2 * 1024 * 1024,
            max_lines: 50_000,
            preview_line_limit: 400,
        }
    }
}

#[derive(Clone, Debug)]
struct SourceLine {
    content: Vec<u8>,
    ending: LineEnding,
    complete: Vec<u8>,
}

/// Parses explicit unified patch bytes. It never treats Git paths or line content as UTF-8.
///
/// # Errors
///
/// Returns a bounded patch error for malformed headers, paths, modes, ranges, or hunks.
pub fn parse_unified_patch(input: &[u8], limits: PatchLimits) -> Result<DiffDocument, DiffError> {
    parse_unified_patch_inner(input, limits, None)
}

/// Parses a patch together with its `git diff --raw -z` metadata. The NUL-safe metadata owns path
/// and rename identity, so quoted, whitespace-containing, and non-UTF-8 patch headers stay opaque.
///
/// # Errors
///
/// Returns a bounded metadata or patch error when the two streams are malformed or disagree on
/// their number of file sections.
pub fn parse_unified_patch_with_raw_metadata(
    raw_input: &[u8],
    patch_input: &[u8],
    limits: PatchLimits,
) -> Result<DiffDocument, DiffError> {
    let entries = parse_raw_diff_z(raw_input)?;
    parse_unified_patch_inner(patch_input, limits, Some(&entries))
}

/// Joins a whitespace-filtered patch with byte-safe raw identities. Git retains
/// raw records for content changes whose patch disappears under whitespace filtering.
///
/// # Errors
/// Returns a bounded metadata or patch error when the streams are malformed or disagree.
pub fn parse_unified_patch_with_filtered_raw_metadata(
    raw_input: &[u8],
    patch_input: &[u8],
    limits: PatchLimits,
) -> Result<DiffDocument, DiffError> {
    let headers = patch_section_ranges(patch_input)
        .into_iter()
        .map(|(start, end)| {
            patch_input[start..end]
                .split(|byte| *byte == b'\n')
                .next()
                .unwrap_or_default()
        })
        .collect::<std::collections::BTreeSet<_>>();
    let entries = parse_raw_diff_z(raw_input)?
        .into_iter()
        .filter(|entry| {
            !matches!(entry.raw_kind, RawFileChangeKind::Modified)
                || entry.old_mode != entry.new_mode
                || [false, true].into_iter().any(|quote_high| {
                    let mut header = b"diff --git ".to_vec();
                    header.extend(quoted_patch_path(
                        b"a/",
                        entry.old_path.as_bytes(),
                        quote_high,
                    ));
                    header.push(b' ');
                    header.extend(quoted_patch_path(
                        b"b/",
                        entry.new_path.as_bytes(),
                        quote_high,
                    ));
                    headers.contains(header.as_slice())
                })
        })
        .collect::<Vec<_>>();
    parse_unified_patch_inner(patch_input, limits, Some(&entries))
}

fn quoted_patch_path(prefix: &[u8], path: &[u8], quote_high: bool) -> Vec<u8> {
    let needs_quotes = path.iter().any(|byte| {
        *byte < 32 || *byte == 127 || matches!(*byte, b'"' | b'\\') || (quote_high && *byte >= 128)
    });
    let mut output = Vec::new();
    if needs_quotes {
        output.push(b'"');
    }
    output.extend(prefix);
    for byte in path {
        match *byte {
            b'"' | b'\\' => output.extend([b'\\', *byte]),
            7 => output.extend(b"\\a"),
            8 => output.extend(b"\\b"),
            9 => output.extend(b"\\t"),
            10 => output.extend(b"\\n"),
            11 => output.extend(b"\\v"),
            12 => output.extend(b"\\f"),
            13 => output.extend(b"\\r"),
            value if value < 32 || value == 127 || (quote_high && value >= 128) => {
                output.extend([
                    b'\\',
                    b'0' + (value >> 6),
                    b'0' + ((value >> 3) & 7),
                    b'0' + (value & 7),
                ]);
            }
            value => output.push(value),
        }
    }
    if needs_quotes {
        output.push(b'"');
    }
    output
}

fn parse_unified_patch_inner(
    input: &[u8],
    limits: PatchLimits,
    raw_entries: Option<&[RawDiffEntry]>,
) -> Result<DiffDocument, DiffError> {
    let sections = patch_section_ranges(input);
    if sections.is_empty() && !input.is_empty() {
        return Err(malformed_patch("patch has no diff --git header"));
    }
    let mut files = Vec::new();
    if let Some(entries) = raw_entries {
        let mut section_cursor = 0;
        for (entry_index, entry) in entries.iter().enumerate() {
            let remaining_sections = sections.len().saturating_sub(section_cursor);
            let remaining_entries = entries.len().saturating_sub(entry_index);
            let split_type_change = matches!(entry.raw_kind, RawFileChangeKind::TypeChanged)
                && remaining_sections > remaining_entries;
            if split_type_change {
                let first = sections
                    .get(section_cursor)
                    .ok_or_else(|| malformed_metadata("type change lacks its delete section"))?;
                let second = sections
                    .get(section_cursor + 1)
                    .ok_or_else(|| malformed_metadata("type change lacks its add section"))?;
                let deleted = parse_patch_section(input, *first, limits, None)?;
                let added = parse_patch_section(input, *second, limits, None)?;
                files.push(merge_type_change_sections(entry, deleted, added)?);
                section_cursor += 2;
            } else {
                let section = sections
                    .get(section_cursor)
                    .ok_or_else(|| malformed_metadata("raw metadata has no patch section"))?;
                files.push(parse_patch_section(input, *section, limits, Some(entry))?);
                section_cursor += 1;
            }
        }
        if section_cursor != sections.len() {
            return Err(malformed_metadata(
                "raw metadata and patch contain different file counts",
            ));
        }
    } else {
        for section in sections {
            files.push(parse_patch_section(input, section, limits, None)?);
        }
    }
    Ok(DiffDocument {
        layout: DiffLayout::Unified,
        files,
    })
}

fn parse_patch_section(
    input: &[u8],
    (start, end): (usize, usize),
    limits: PatchLimits,
    raw_entry: Option<&RawDiffEntry>,
) -> Result<DiffFile, DiffError> {
    let section = &input[start..end];
    let total_lines = source_line_count(section);
    let too_large = section.len() > limits.max_bytes || total_lines > limits.max_lines;
    let retained = if too_large {
        bounded_patch_prefix(section, limits.preview_line_limit)
    } else {
        section
    };
    let lines = split_source_lines(retained);
    parse_file_section(
        &lines,
        u64::try_from(section.len()).unwrap_or(u64::MAX),
        u64::try_from(total_lines).unwrap_or(u64::MAX),
        too_large,
        limits.preview_line_limit,
        raw_entry,
    )
}

fn merge_type_change_sections(
    raw: &RawDiffEntry,
    deleted: DiffFile,
    added: DiffFile,
) -> Result<DiffFile, DiffError> {
    if deleted.kind != FileChangeKind::Deleted
        || added.kind != FileChangeKind::Added
        || deleted.id.old_path.as_ref() != Some(&raw.old_path)
        || added.id.new_path.as_ref() != Some(&raw.new_path)
    {
        return Err(malformed_metadata(
            "type change patch is not one delete section followed by one add section",
        ));
    }
    let id = raw_file_id(raw);
    let mut hunks = deleted.hunks;
    hunks.extend(added.hunks);
    let fallback = deleted.fallback.or(added.fallback);
    if fallback.is_some() {
        hunks.clear();
    }
    for (ordinal, hunk) in hunks.iter_mut().enumerate() {
        hunk.id.file = id.clone();
        hunk.id.ordinal = u32::try_from(ordinal).unwrap_or(u32::MAX);
        for line in &mut hunk.lines {
            line.id.hunk = hunk.id.clone();
        }
    }
    let mut patch_header = deleted.patch_header;
    patch_header.extend(added.patch_header);
    Ok(DiffFile {
        id,
        kind: FileChangeKind::TypeChanged,
        old_mode: (raw.old_mode.value() != 0).then_some(raw.old_mode),
        new_mode: (raw.new_mode.value() != 0).then_some(raw.new_mode),
        syntax: syntax_for_path(Some(&raw.new_path)),
        staging: FileStagingState::NotApplicable,
        hunks,
        fallback,
        patch_header,
    })
}

fn source_line_count(input: &[u8]) -> usize {
    input
        .iter()
        .fold(0_usize, |count, byte| count + usize::from(*byte == b'\n'))
        + usize::from(!input.is_empty() && !input.ends_with(b"\n"))
}

fn patch_section_ranges(input: &[u8]) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in input.split_inclusive(|byte| *byte == b'\n') {
        if line.starts_with(b"diff --git ") {
            starts.push(offset);
        }
        offset += line.len();
    }
    starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            (
                *start,
                starts.get(index + 1).copied().unwrap_or(input.len()),
            )
        })
        .collect()
}

fn bounded_patch_prefix(input: &[u8], preview_line_limit: u32) -> &[u8] {
    const MAX_RETAINED_BYTES: usize = 256 * 1024;
    const HEADER_LINE_ALLOWANCE: usize = 128;
    let max_lines = usize::try_from(preview_line_limit)
        .unwrap_or(usize::MAX)
        .saturating_add(HEADER_LINE_ALLOWANCE);
    let byte_limit = input.len().min(MAX_RETAINED_BYTES);
    let mut line_count = 0;
    let mut end = byte_limit;
    for (index, byte) in input[..byte_limit].iter().enumerate() {
        if *byte == b'\n' {
            line_count += 1;
            if line_count >= max_lines {
                end = index + 1;
                break;
            }
        }
    }
    &input[..end]
}

#[allow(clippy::too_many_lines)]
fn parse_file_section(
    lines: &[SourceLine],
    total_bytes: u64,
    total_lines: u64,
    too_large: bool,
    preview_line_limit: u32,
    raw_entry: Option<&RawDiffEntry>,
) -> Result<DiffFile, DiffError> {
    let (old_path, new_path, raw_kind) = if let Some(entry) = raw_entry {
        let file = raw_file_id(entry);
        (file.old_path, file.new_path, Some(entry.kind.clone()))
    } else {
        let (diff_old_path, diff_new_path) = diff_git_paths(lines)?;
        let old_path = if has_header_value(lines, b"--- ", b"/dev/null") {
            None
        } else {
            header_path(lines, b"--- ", b"a/")?.or(diff_old_path)
        };
        let new_path = if has_header_value(lines, b"+++ ", b"/dev/null") {
            None
        } else {
            header_path(lines, b"+++ ", b"b/")?.or(diff_new_path)
        };
        let rename_from = metadata_path(lines, b"rename from ")?;
        let rename_to = metadata_path(lines, b"rename to ")?;
        let renamed = rename_from.is_some() || rename_to.is_some();
        (
            rename_from.or(old_path),
            rename_to.or(new_path),
            renamed.then_some(FileChangeKind::Renamed {
                similarity: percent_metadata(lines, b"similarity index ")?.unwrap_or(100),
            }),
        )
    };
    if old_path.is_none() && new_path.is_none() {
        return Err(malformed_patch("file section has no usable path"));
    }
    let id = DiffFileId {
        old_path: old_path.clone(),
        new_path: new_path.clone(),
    };
    let old_mode = raw_entry
        .and_then(|entry| (entry.old_mode.value() != 0).then_some(entry.old_mode))
        .or(mode_metadata(lines, b"old mode ")?)
        .or(mode_metadata(lines, b"deleted file mode ")?);
    let new_mode = raw_entry
        .and_then(|entry| (entry.new_mode.value() != 0).then_some(entry.new_mode))
        .or(mode_metadata(lines, b"new mode ")?)
        .or(mode_metadata(lines, b"new file mode ")?);
    let binary = lines.iter().any(|line| {
        line.content.starts_with(b"Binary files ") || line.content == b"GIT binary patch"
    });
    let submodule = lines.iter().any(|line| {
        line.content
            .get(1..)
            .is_some_and(|line| line.starts_with(b"Subproject commit "))
    });
    let conflict = lines.iter().any(|line| {
        line.content
            .get(1..)
            .is_some_and(|line| line.starts_with(b"<<<<<<< ") || line.starts_with(b">>>>>>> "))
    });
    let image = new_path
        .as_ref()
        .or(old_path.as_ref())
        .is_some_and(is_image_path);
    let kind = if image {
        FileChangeKind::Image
    } else if binary {
        FileChangeKind::Binary
    } else if old_mode == Some(GitFileMode::GITLINK)
        || new_mode == Some(GitFileMode::GITLINK)
        || submodule
    {
        FileChangeKind::Submodule
    } else if conflict {
        FileChangeKind::Conflict
    } else if let Some(kind) = raw_kind {
        kind
    } else if old_path.is_none() {
        FileChangeKind::Added
    } else if new_path.is_none() {
        FileChangeKind::Deleted
    } else if old_mode.is_some() || new_mode.is_some() {
        FileChangeKind::ModeChanged
    } else {
        FileChangeKind::Modified
    };
    let first_hunk = lines
        .iter()
        .position(|line| line.content.starts_with(b"@@ "));
    let patch_header = lines[..first_hunk.unwrap_or(lines.len())]
        .iter()
        .flat_map(|line| line.complete.iter().copied())
        .collect();
    let fallback = if image {
        Some(DiffFallback::ImageDeferred)
    } else if binary {
        Some(DiffFallback::Binary { byte_count: None })
    } else if too_large {
        Some(DiffFallback::TooLarge {
            byte_count: total_bytes,
            line_count: total_lines,
            preview_line_limit,
        })
    } else {
        None
    };
    let hunks = if fallback.is_some() {
        Vec::new()
    } else {
        parse_hunks(lines, &id)?
    };
    Ok(DiffFile {
        id,
        kind,
        old_mode,
        new_mode,
        syntax: syntax_for_path(new_path.as_ref().or(old_path.as_ref())),
        staging: FileStagingState::NotApplicable,
        hunks,
        fallback,
        patch_header,
    })
}

fn header_path(
    lines: &[SourceLine],
    prefix: &[u8],
    side_prefix: &[u8],
) -> Result<Option<DiffPath>, DiffError> {
    let Some(value) = lines
        .iter()
        .find_map(|line| line.content.strip_prefix(prefix))
    else {
        return Ok(None);
    };
    let value = value.split(|byte| *byte == b'\t').next().unwrap_or(value);
    if value == b"/dev/null" {
        return Ok(None);
    }
    let value = value.strip_prefix(side_prefix).unwrap_or(value);
    DiffPath::from_bytes(value.to_vec())
        .map(Some)
        .map_err(|_| malformed_patch("patch header contains an invalid path"))
}

fn has_header_value(lines: &[SourceLine], prefix: &[u8], expected: &[u8]) -> bool {
    lines.iter().any(|line| {
        line.content
            .strip_prefix(prefix)
            .and_then(|value| value.split(|byte| *byte == b'\t').next())
            == Some(expected)
    })
}

fn diff_git_paths(lines: &[SourceLine]) -> Result<(Option<DiffPath>, Option<DiffPath>), DiffError> {
    let header = lines
        .first()
        .and_then(|line| line.content.strip_prefix(b"diff --git "))
        .ok_or_else(|| malformed_patch("file section has no diff --git header"))?;
    let mut fields = header.split(|byte| *byte == b' ');
    let old = fields
        .next()
        .and_then(|path| path.strip_prefix(b"a/"))
        .ok_or_else(|| malformed_patch("diff --git header has no old path"))?;
    let new = fields
        .next()
        .and_then(|path| path.strip_prefix(b"b/"))
        .ok_or_else(|| malformed_patch("diff --git header has no new path"))?;
    if fields.next().is_some() {
        return Err(malformed_patch(
            "quoted or space-containing diff paths require raw NUL metadata",
        ));
    }
    let old = DiffPath::from_bytes(old.to_vec())
        .map_err(|_| malformed_patch("diff --git header contains an invalid old path"))?;
    let new = DiffPath::from_bytes(new.to_vec())
        .map_err(|_| malformed_patch("diff --git header contains an invalid new path"))?;
    Ok((Some(old), Some(new)))
}

fn metadata_path(lines: &[SourceLine], prefix: &[u8]) -> Result<Option<DiffPath>, DiffError> {
    let Some(value) = lines
        .iter()
        .find_map(|line| line.content.strip_prefix(prefix))
    else {
        return Ok(None);
    };
    DiffPath::from_bytes(value.to_vec())
        .map(Some)
        .map_err(|_| malformed_patch("patch metadata contains an invalid path"))
}

fn is_image_path(path: &DiffPath) -> bool {
    let bytes = path.as_bytes();
    [
        b".png".as_slice(),
        b".apng",
        b".jpg",
        b".jpeg",
        b".jfif",
        b".gif",
        b".webp",
        b".svg",
        b".bmp",
        b".ico",
        b".tif",
        b".tiff",
        b".qoi",
    ]
    .iter()
    .any(|extension| {
        bytes.len() >= extension.len()
            && bytes[bytes.len() - extension.len()..].eq_ignore_ascii_case(extension)
    })
}

fn syntax_for_path(path: Option<&DiffPath>) -> SyntaxAvailability {
    let Some(path) = path else {
        return SyntaxAvailability::Unavailable;
    };
    let bytes = path.as_bytes();
    for (extension, language) in [
        (b".rs".as_slice(), b"rust".as_slice()),
        (b".qml", b"qml"),
        (b".js", b"javascript"),
        (b".py", b"python"),
        (b".toml", b"toml"),
        (b".json", b"json"),
    ] {
        if bytes.ends_with(extension) {
            return SyntaxAvailability::Available(language.to_vec());
        }
    }
    SyntaxAvailability::Unavailable
}

fn mode_metadata(lines: &[SourceLine], prefix: &[u8]) -> Result<Option<GitFileMode>, DiffError> {
    let Some(value) = lines
        .iter()
        .find_map(|line| line.content.strip_prefix(prefix))
    else {
        return Ok(None);
    };
    GitFileMode::from_octal_bytes(value)
        .map(Some)
        .map_err(|_| malformed_patch("patch metadata contains an invalid mode"))
}

fn percent_metadata(lines: &[SourceLine], prefix: &[u8]) -> Result<Option<u8>, DiffError> {
    let Some(value) = lines
        .iter()
        .find_map(|line| line.content.strip_prefix(prefix))
    else {
        return Ok(None);
    };
    let value = value.strip_suffix(b"%").unwrap_or(value);
    std::str::from_utf8(value)
        .ok()
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|value| *value <= 100)
        .map(Some)
        .ok_or_else(|| malformed_patch("patch metadata contains an invalid percentage"))
}

fn parse_hunks(lines: &[SourceLine], file: &DiffFileId) -> Result<Vec<DiffHunk>, DiffError> {
    let mut hunks = Vec::new();
    let mut cursor = 0;
    while cursor < lines.len() {
        if !lines[cursor].content.starts_with(b"@@ ") {
            cursor += 1;
            continue;
        }
        let (old_start, old_count, new_start, new_count, section) =
            parse_hunk_header(&lines[cursor].content)?;
        let id = HunkId {
            file: file.clone(),
            old_start,
            new_start,
            ordinal: u32::try_from(hunks.len()).unwrap_or(u32::MAX),
        };
        cursor += 1;
        let mut old_line = old_start;
        let mut new_line = new_start;
        let mut parsed_lines: Vec<DiffLine> = Vec::new();
        while cursor < lines.len()
            && !lines[cursor].content.starts_with(b"@@ ")
            && !lines[cursor].content.starts_with(b"diff --git ")
        {
            let source = &lines[cursor];
            if source.content == b"\\ No newline at end of file" {
                let previous = parsed_lines
                    .last_mut()
                    .ok_or_else(|| malformed_patch("no-newline marker has no preceding line"))?;
                previous.no_newline_marker = true;
                cursor += 1;
                continue;
            }
            let Some(prefix) = source.content.first() else {
                return Err(malformed_patch(
                    "empty line inside a hunk has no diff prefix",
                ));
            };
            let (origin, old_number, new_number) = match prefix {
                b' ' => {
                    let result = (DiffLineOrigin::Context, Some(old_line), Some(new_line));
                    old_line += 1;
                    new_line += 1;
                    result
                }
                b'-' => {
                    let result = (DiffLineOrigin::Deletion, Some(old_line), None);
                    old_line += 1;
                    result
                }
                b'+' => {
                    let result = (DiffLineOrigin::Addition, None, Some(new_line));
                    new_line += 1;
                    result
                }
                _ => return Err(malformed_patch("hunk line has an invalid prefix")),
            };
            parsed_lines.push(DiffLine {
                id: DiffLineId {
                    hunk: id.clone(),
                    old_line: old_number,
                    new_line: new_number,
                    occurrence: 0,
                },
                origin,
                source: source.content[1..].to_vec(),
                ending: source.ending,
                no_newline_marker: false,
                staged: None,
                intra_line: Vec::new(),
                annotation: conflict_annotation(&source.content[1..]),
            });
            cursor += 1;
        }
        let actual_old = parsed_lines
            .iter()
            .filter(|line| line.origin != DiffLineOrigin::Addition)
            .count();
        let actual_new = parsed_lines
            .iter()
            .filter(|line| line.origin != DiffLineOrigin::Deletion)
            .count();
        if actual_old != old_count as usize || actual_new != new_count as usize {
            return Err(malformed_patch("hunk line counts do not match its header"));
        }
        hunks.push(DiffHunk {
            id,
            old_count,
            new_count,
            section,
            lines: parsed_lines,
        });
    }
    Ok(hunks)
}

/// Annotates a diff line with the shared marker rule ([`crate::merge::markers`]) at Git's
/// default marker size. A bare `<<<<<<<`/`>>>>>>>` marker has an empty label.
fn conflict_annotation(content: &[u8]) -> DiffLineAnnotation {
    use crate::merge::markers::{classify_line, MarkerKind, DEFAULT_MARKER_SIZE};
    match classify_line(content, DEFAULT_MARKER_SIZE) {
        Some((MarkerKind::Current, label)) => DiffLineAnnotation::ConflictBegin {
            label: content[label].to_vec(),
        },
        Some((MarkerKind::Separator, _)) => DiffLineAnnotation::ConflictSeparator,
        Some((MarkerKind::Incoming, label)) => DiffLineAnnotation::ConflictEnd {
            label: content[label].to_vec(),
        },
        Some((MarkerKind::Ancestor, _)) | None => DiffLineAnnotation::None,
    }
}

/// One unified-diff hunk header: `@@ -<old start>[,<count>] +<new start>[,<count>] @@[ <section>]`.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct HunkHeader {
    pub old_start: u32,
    /// Lines of the old side; `1` when the header omits the count.
    pub old_count: u32,
    pub new_start: u32,
    /// Lines of the new side; `1` when the header omits the count.
    pub new_count: u32,
    /// The text after the closing `@@` (Git's function context), including its leading space.
    pub section: Vec<u8>,
}

/// Parses one hunk header line without its line terminator, with the rules the patch parser
/// uses. Useful for patches a hosting provider returns per file.
///
/// # Errors
/// Returns a malformed-patch [`DiffError`] for a line that is not a hunk header.
pub fn parse_unified_hunk_header(line: &[u8]) -> Result<HunkHeader, DiffError> {
    let (old_start, old_count, new_start, new_count, section) = parse_hunk_header(line)?;
    Ok(HunkHeader {
        old_start,
        old_count,
        new_start,
        new_count,
        section,
    })
}

fn parse_hunk_header(input: &[u8]) -> Result<(u32, u32, u32, u32, Vec<u8>), DiffError> {
    let tail = input
        .strip_prefix(b"@@ -")
        .ok_or_else(|| malformed_patch("hunk header has no old range"))?;
    let separator = tail
        .iter()
        .position(|byte| *byte == b' ')
        .ok_or_else(|| malformed_patch("hunk header has no new range"))?;
    let old = &tail[..separator];
    let tail = tail[separator + 1..]
        .strip_prefix(b"+")
        .ok_or_else(|| malformed_patch("hunk header has no new range prefix"))?;
    let close = tail
        .windows(3)
        .position(|window| window == b" @@")
        .ok_or_else(|| malformed_patch("hunk header has no closing marker"))?;
    let new = &tail[..close];
    let section = tail[close + 3..].to_vec();
    let (old_start, old_count) = parse_range(old)?;
    let (new_start, new_count) = parse_range(new)?;
    Ok((old_start, old_count, new_start, new_count, section))
}

fn parse_range(value: &[u8]) -> Result<(u32, u32), DiffError> {
    let mut parts = value.splitn(2, |byte| *byte == b',');
    let start = parse_u32(parts.next().unwrap_or_default())?;
    let count = parts.next().map_or(Ok(1), parse_u32)?;
    Ok((start, count))
}

fn parse_u32(value: &[u8]) -> Result<u32, DiffError> {
    std::str::from_utf8(value)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| malformed_patch("hunk range is not a decimal integer"))
}

fn split_source_lines(input: &[u8]) -> Vec<SourceLine> {
    let mut result = Vec::new();
    let mut start = 0;
    for (index, byte) in input.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let has_cr = index > start && input[index - 1] == b'\r';
        let content_end = if has_cr { index - 1 } else { index };
        result.push(SourceLine {
            content: input[start..content_end].to_vec(),
            ending: if has_cr {
                LineEnding::CrLf
            } else {
                LineEnding::Lf
            },
            complete: input[start..=index].to_vec(),
        });
        start = index + 1;
    }
    if start < input.len() {
        result.push(SourceLine {
            content: input[start..].to_vec(),
            ending: LineEnding::None,
            complete: input[start..].to_vec(),
        });
    }
    result
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BytePatchBuilder;

impl BytePatchBuilder {
    /// Builds a byte-preserving patch for the selected changes.
    ///
    /// # Errors
    /// Returns a bounded error if the selection or file cannot form a text patch.
    pub fn construct_patch(
        &self,
        request: &PatchConstructionRequest,
    ) -> Result<ConstructedPatch, DiffError> {
        request
            .selection
            .validate_for_file(&request.file)
            .map_err(|error| DiffError::new(DiffErrorKind::InvalidSelection, error.to_string()))?;
        if request.file.fallback.is_some()
            || matches!(request.file.kind, FileChangeKind::TypeChanged)
        {
            return Err(DiffError::new(
                DiffErrorKind::UnsupportedBinary,
                "selected file has no constructible text patch",
            ));
        }
        let mut output = request.file.patch_header.clone();
        let mut selected_change_count = 0;
        for hunk in &request.file.hunks {
            let selected: Vec<usize> = hunk
                .lines
                .iter()
                .enumerate()
                .filter_map(|(index, line)| request.selection.contains(&line.id).then_some(index))
                .collect();
            selected_change_count += selected.len();
            for (start, end) in
                selection_windows(&selected, hunk.lines.len(), request.context.value())
            {
                let (start, end) = expand_to_change_group(hunk, start, end);
                render_partial_hunk(
                    &mut output,
                    hunk,
                    start,
                    end,
                    &request.selection,
                    request.reverse,
                );
            }
        }
        if selected_change_count == 0 {
            return Err(DiffError::new(
                DiffErrorKind::InvalidSelection,
                "selection does not contain a line in this patch",
            ));
        }
        Ok(ConstructedPatch {
            bytes: output,
            selected_change_count,
        })
    }
}

fn selection_windows(selected: &[usize], line_count: usize, context: u16) -> Vec<(usize, usize)> {
    let context = usize::from(context);
    let mut windows: Vec<(usize, usize)> = Vec::new();
    for index in selected {
        let start = index.saturating_sub(context);
        let end = index
            .saturating_add(context)
            .saturating_add(1)
            .min(line_count);
        if let Some(last) = windows.last_mut().filter(|last| start <= last.1) {
            last.1 = last.1.max(end);
        } else {
            windows.push((start, end));
        }
    }
    windows
}

fn expand_to_change_group(hunk: &DiffHunk, mut start: usize, mut end: usize) -> (usize, usize) {
    while start > 0 && hunk.lines[start].is_change() && hunk.lines[start - 1].is_change() {
        start -= 1;
    }
    while end < hunk.lines.len() && hunk.lines[end - 1].is_change() && hunk.lines[end].is_change() {
        end += 1;
    }
    (start, end)
}

fn render_partial_hunk(
    output: &mut Vec<u8>,
    hunk: &DiffHunk,
    start: usize,
    end: usize,
    selection: &crate::diff::model::LineSelection,
    reverse: bool,
) {
    let mut old_cursor = hunk.id.old_start;
    let mut new_cursor = hunk.id.new_start;
    for line in &hunk.lines[..start] {
        if line.origin != DiffLineOrigin::Addition {
            old_cursor += 1;
        }
        if line.origin != DiffLineOrigin::Deletion {
            new_cursor += 1;
        }
    }
    let old_start = old_cursor;
    let new_start = new_cursor;
    let mut rendered = normalize_selected_lines(&hunk.lines[start..end], selection, reverse);
    let mut old_count = 0_u32;
    let mut new_count = 0_u32;

    for (origin, _) in &mut rendered {
        let effective = *origin;
        let effective = if reverse {
            match effective {
                DiffLineOrigin::Addition => DiffLineOrigin::Deletion,
                DiffLineOrigin::Deletion => DiffLineOrigin::Addition,
                DiffLineOrigin::Context => DiffLineOrigin::Context,
            }
        } else {
            effective
        };
        if effective != DiffLineOrigin::Addition {
            old_count += 1;
        }
        if effective != DiffLineOrigin::Deletion {
            new_count += 1;
        }
        *origin = effective;
    }
    let (header_old_start, header_old_count, header_new_start, header_new_count) = if reverse {
        (new_start, new_count, old_start, old_count)
    } else {
        (old_start, old_count, new_start, new_count)
    };
    output.extend_from_slice(b"@@ -");
    append_decimal(output, header_old_start);
    output.push(b',');
    append_decimal(output, header_old_count);
    output.extend_from_slice(b" +");
    append_decimal(output, header_new_start);
    output.push(b',');
    append_decimal(output, header_new_count);
    output.extend_from_slice(b" @@");
    output.extend_from_slice(&hunk.section);
    output.push(b'\n');
    for (origin, line) in rendered {
        output.push(match origin {
            DiffLineOrigin::Context => b' ',
            DiffLineOrigin::Addition => b'+',
            DiffLineOrigin::Deletion => b'-',
        });
        output.extend_from_slice(&line.source);
        match line.ending {
            LineEnding::Lf => output.push(b'\n'),
            LineEnding::CrLf => output.extend_from_slice(b"\r\n"),
            LineEnding::None => {}
        }
        if line.no_newline_marker {
            if line.ending == LineEnding::None {
                output.push(b'\n');
            }
            output.extend_from_slice(b"\\ No newline at end of file\n");
        }
    }
}

fn normalize_selected_lines<'a>(
    lines: &'a [DiffLine],
    selection: &crate::diff::model::LineSelection,
    reverse: bool,
) -> Vec<(DiffLineOrigin, &'a DiffLine)> {
    let mut rendered = Vec::new();
    let mut cursor = 0;
    while cursor < lines.len() {
        if lines[cursor].origin == DiffLineOrigin::Context {
            rendered.push((DiffLineOrigin::Context, &lines[cursor]));
            cursor += 1;
            continue;
        }

        let group_start = cursor;
        while cursor < lines.len() && lines[cursor].origin != DiffLineOrigin::Context {
            cursor += 1;
        }
        let group = &lines[group_start..cursor];
        let deletions = group
            .iter()
            .filter(|line| line.origin == DiffLineOrigin::Deletion)
            .collect::<Vec<_>>();
        let additions = group
            .iter()
            .filter(|line| line.origin == DiffLineOrigin::Addition)
            .collect::<Vec<_>>();
        for index in 0..deletions.len().max(additions.len()) {
            let deletion = deletions.get(index).copied();
            let addition = additions.get(index).copied();
            let deletion_selected = deletion.is_some_and(|line| selection.contains(&line.id));
            let addition_selected = addition.is_some_and(|line| selection.contains(&line.id));

            if reverse {
                if let Some(line) = addition {
                    rendered.push((
                        if addition_selected {
                            DiffLineOrigin::Addition
                        } else {
                            DiffLineOrigin::Context
                        },
                        line,
                    ));
                }
                if deletion_selected {
                    rendered.push((DiffLineOrigin::Deletion, deletion.unwrap()));
                }
            } else {
                if let Some(line) = deletion {
                    rendered.push((
                        if deletion_selected {
                            DiffLineOrigin::Deletion
                        } else {
                            DiffLineOrigin::Context
                        },
                        line,
                    ));
                }
                if addition_selected {
                    rendered.push((DiffLineOrigin::Addition, addition.unwrap()));
                }
            }
        }
    }
    rendered
}

fn append_decimal(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(value.to_string().as_bytes());
}

fn malformed_metadata(detail: &'static str) -> DiffError {
    DiffError::new(DiffErrorKind::MalformedMetadata, detail)
}

fn metadata_too_large(detail: &'static str) -> DiffError {
    DiffError::new(DiffErrorKind::TooLarge, detail)
}

fn malformed_patch(detail: &'static str) -> DiffError {
    DiffError::new(DiffErrorKind::MalformedPatch, detail)
}

pub enum CombinedDiff {
    Empty,
    RawOnly(Vec<u8>),
    Standard { raw: Vec<u8>, patch: Vec<u8> },
    Conflict(Vec<u8>),
}

/// Splits Git raw-plus-patch output while retaining conflict and raw-only states.
///
/// # Errors
/// Rejects data without a valid output-mode separator.
pub fn split_combined_diff(input: &[u8]) -> Result<CombinedDiff, DiffError> {
    if input.is_empty() || input == b"\0" {
        return Ok(CombinedDiff::Empty);
    }
    let marker = b"\0\0diff --git ";
    let Some(split) = input
        .windows(marker.len())
        .position(|window| window == marker)
    else {
        if input.starts_with(b"::") {
            return Ok(CombinedDiff::Conflict(input.to_vec()));
        }
        if input.starts_with(b":") {
            return Ok(CombinedDiff::RawOnly(input.to_vec()));
        }
        return Err(DiffError::new(
            DiffErrorKind::MalformedMetadata,
            "combined Git diff has no metadata-patch separator",
        ));
    };
    // The first NUL terminates the final raw path. The second separates output modes.
    Ok(CombinedDiff::Standard {
        raw: input[..=split].to_vec(),
        patch: input[split + 2..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::model::LineSelection;
    use serde_json::Value;
    use std::collections::BTreeMap;

    const ZERO: &[u8] = b"0000000000000000000000000000000000000000";
    const ONE: &[u8] = b"1111111111111111111111111111111111111111";
    const TWO: &[u8] = b"2222222222222222222222222222222222222222";
    const THREE: &[u8] = b"3333333333333333333333333333333333333333";

    #[test]
    fn raw_z_parser_preserves_non_utf8_rename_paths() {
        let mut input = b":100644 100755 ".to_vec();
        input.extend_from_slice(ZERO);
        input.push(b' ');
        input.extend_from_slice(ONE);
        input.extend_from_slice(b" R92\0old/\xff.rs\0new/\xfe.rs\0");
        let entries = parse_raw_diff_z(&input).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].old_object, RawObjectId::Null(ObjectFormat::Sha1));
        assert_eq!(
            entries[0].new_object,
            RawObjectId::Object(ObjectId::parse(String::from_utf8_lossy(ONE)).unwrap())
        );
        assert_eq!(entries[0].old_path.as_bytes(), b"old/\xff.rs");
        assert_eq!(entries[0].new_path.as_bytes(), b"new/\xfe.rs");
        assert_eq!(entries[0].kind, FileChangeKind::Renamed { similarity: 92 });
        assert_eq!(
            entries[0].raw_kind,
            RawFileChangeKind::Renamed { similarity: 92 }
        );
        input.pop();
        assert_eq!(
            parse_raw_diff_z(&input).unwrap_err().kind,
            DiffErrorKind::MalformedMetadata
        );
    }

    #[test]
    fn raw_copy_metadata_drives_patch_identity_and_kind() {
        let mut raw = b":100644 100644 ".to_vec();
        raw.extend_from_slice(ONE);
        raw.push(b' ');
        raw.extend_from_slice(TWO);
        raw.extend_from_slice(b" C80\0old/source.rs\0new/copy.rs\0");
        let patch = b"diff --git a/old/source.rs b/new/copy.rs\nsimilarity index 80%\ncopy from old/source.rs\ncopy to new/copy.rs\n--- a/old/source.rs\n+++ b/new/copy.rs\n@@ -1 +1 @@\n-old\n+new\n";
        let document =
            parse_unified_patch_with_raw_metadata(&raw, patch, PatchLimits::default()).unwrap();
        assert_eq!(
            document.files[0].kind,
            FileChangeKind::Copied { similarity: 80 }
        );
        assert_eq!(
            document.files[0].id.old_path.as_ref().unwrap().as_bytes(),
            b"old/source.rs"
        );
        assert_eq!(
            document.files[0].id.new_path.as_ref().unwrap().as_bytes(),
            b"new/copy.rs"
        );
    }

    #[test]
    fn raw_objects_distinguish_sha1_sha256_and_null_ids() {
        assert!(parse_raw_diff_z(b"").unwrap().is_empty());
        for malformed in [b"\0".as_slice(), b"\0\0", b"junk\0\0"] {
            assert_eq!(
                parse_raw_diff_z(malformed).unwrap_err().kind,
                DiffErrorKind::MalformedMetadata
            );
        }
        let sha256 = "2".repeat(64);
        let null_sha256 = "0".repeat(64);
        let input = format!(":100644 100644 {null_sha256} {sha256} M\0a.txt\0");
        let entries = parse_raw_diff_z(input.as_bytes()).unwrap();
        assert_eq!(
            entries[0].old_object,
            RawObjectId::Null(ObjectFormat::Sha256)
        );
        assert_eq!(
            entries[0].new_object,
            RawObjectId::Object(ObjectId::parse(sha256).unwrap())
        );
        assert_eq!(
            parse_raw_diff_z(
                b":100644 100644 000000000000000000000000000000000000000 1111111111111111111111111111111111111111 M\0a\0"
            )
            .unwrap_err()
            .kind,
            DiffErrorKind::MalformedMetadata
        );

        let mixed = format!(
            ":100644 100644 {} {} M\0a.txt\0",
            "1".repeat(40),
            "2".repeat(64)
        );
        assert_eq!(
            parse_raw_diff_z(mixed.as_bytes()).unwrap_err().kind,
            DiffErrorKind::MalformedMetadata
        );
    }

    #[test]
    fn raw_status_uses_scores_and_modes_with_git_semantics() {
        let record = |old_mode: &[u8], new_mode: &[u8], status: &[u8]| {
            let mut input = Vec::new();
            input.push(b':');
            input.extend_from_slice(old_mode);
            input.push(b' ');
            input.extend_from_slice(new_mode);
            input.push(b' ');
            input.extend_from_slice(ONE);
            input.push(b' ');
            input.extend_from_slice(TWO);
            input.push(b' ');
            input.extend_from_slice(status);
            input.extend_from_slice(b"\0path\0");
            parse_raw_diff_z(&input).unwrap().remove(0)
        };

        let rewrite = record(b"100644", b"100644", b"M73");
        assert_eq!(rewrite.raw_kind, RawFileChangeKind::Modified);
        assert_eq!(rewrite.kind, FileChangeKind::Modified);

        let chmod = record(b"100644", b"100755", b"M");
        assert_eq!(chmod.raw_kind, RawFileChangeKind::ModeChanged);
        assert_eq!(chmod.kind, FileChangeKind::ModeChanged);

        let type_change = record(b"100644", b"120000", b"T");
        assert_eq!(type_change.raw_kind, RawFileChangeKind::TypeChanged);
        assert_eq!(type_change.kind, FileChangeKind::TypeChanged);

        for invalid in [b"M101".as_slice(), b"M-1", b"Mxx"] {
            let mut input = b":100644 100644 ".to_vec();
            input.extend_from_slice(ONE);
            input.push(b' ');
            input.extend_from_slice(TWO);
            input.push(b' ');
            input.extend_from_slice(invalid);
            input.extend_from_slice(b"\0path\0");
            assert_eq!(
                parse_raw_diff_z(&input).unwrap_err().kind,
                DiffErrorKind::MalformedMetadata
            );
        }
    }

    #[test]
    fn numstat_z_preserves_counts_binary_markers_and_byte_paths() {
        let mut input = b"12\t3\tsrc/with\ttab.rs\0-\t-\tassets/icon.png\0".to_vec();
        let first_record_end = input.len();
        input.extend_from_slice(b"4\t5\t\0old/space name-\xff.rs\0new/space name-\xfe.rs\0");

        let entries = parse_numstat_z(&input).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].added, DiffStatCount::Lines(12));
        assert_eq!(entries[0].deleted, DiffStatCount::Lines(3));
        assert_eq!(entries[0].old_path.as_bytes(), b"src/with\ttab.rs");
        assert_eq!(entries[1].added, DiffStatCount::Binary);
        assert_eq!(entries[1].deleted, DiffStatCount::Binary);
        assert_eq!(entries[2].old_path.as_bytes(), b"old/space name-\xff.rs");
        assert_eq!(entries[2].new_path.as_bytes(), b"new/space name-\xfe.rs");

        assert_eq!(
            parse_numstat_z(&input[..first_record_end]).unwrap().len(),
            2
        );
    }

    #[test]
    fn numstat_join_uses_raw_status_to_form_stable_file_identities() {
        let mut raw = Vec::new();
        for (old_mode, new_mode, status, path) in [
            (
                b"000000".as_slice(),
                b"100644".as_slice(),
                b"A".as_slice(),
                b"added".as_slice(),
            ),
            (
                b"100644".as_slice(),
                b"000000".as_slice(),
                b"D".as_slice(),
                b"deleted".as_slice(),
            ),
        ] {
            raw.push(b':');
            raw.extend_from_slice(old_mode);
            raw.push(b' ');
            raw.extend_from_slice(new_mode);
            raw.push(b' ');
            raw.extend_from_slice(ZERO);
            raw.push(b' ');
            raw.extend_from_slice(ONE);
            raw.push(b' ');
            raw.extend_from_slice(status);
            raw.push(0);
            raw.extend_from_slice(path);
            raw.push(0);
        }
        let stats =
            parse_numstat_z_with_raw_metadata(&raw, b"2\t0\tadded\x000\t3\tdeleted\0").unwrap();
        assert!(stats[0].file.old_path.is_none());
        assert_eq!(
            stats[0].file.new_path.as_ref().unwrap().as_bytes(),
            b"added"
        );
        assert_eq!(stats[0].added, DiffStatCount::Lines(2));
        assert_eq!(
            stats[1].file.old_path.as_ref().unwrap().as_bytes(),
            b"deleted"
        );
        assert!(stats[1].file.new_path.is_none());
        assert_eq!(stats[1].deleted, DiffStatCount::Lines(3));

        assert_eq!(
            parse_numstat_z_with_raw_metadata(&raw, b"2\t0\twrong\x000\t3\tdeleted\0")
                .unwrap_err()
                .kind,
            DiffErrorKind::MalformedMetadata
        );

        let mut conflict_raw = b":000000 000000 ".to_vec();
        conflict_raw.extend_from_slice(ZERO);
        conflict_raw.push(b' ');
        conflict_raw.extend_from_slice(ZERO);
        conflict_raw.extend_from_slice(b" U\0conflict.txt\0");
        let duplicate_conflicts = [conflict_raw.as_slice(), conflict_raw.as_slice()].concat();
        let conflicts = parse_numstat_z_with_raw_metadata(
            &duplicate_conflicts,
            b"0\t0\tconflict.txt\x000\t0\tconflict.txt\0",
        )
        .unwrap();
        assert_eq!(conflicts.len(), 2);
        assert_eq!(conflicts[0].ordinal, 0);
        assert_eq!(conflicts[1].ordinal, 1);
        assert_eq!(conflicts[0].file, conflicts[1].file);
        assert_eq!(conflicts[0].kind, FileChangeKind::Conflict);
    }

    #[test]
    fn combined_raw_z_exposes_parent_display_metadata_without_recasting_hunks() {
        let mut input = b"::100644 120000 100644 ".to_vec();
        input.extend_from_slice(ONE);
        input.push(b' ');
        input.extend_from_slice(TWO);
        input.push(b' ');
        input.extend_from_slice(THREE);
        input.extend_from_slice(b" RM\0src/merge-\xff.rs\0");

        let entries = parse_combined_raw_diff_z(&input).unwrap();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(
            entry.parent_modes,
            vec![GitFileMode::REGULAR, GitFileMode::SYMLINK]
        );
        assert_eq!(entry.result_mode, GitFileMode::REGULAR);
        assert_eq!(entry.parent_objects.len(), 2);
        assert_eq!(
            entry.result_object,
            RawObjectId::Object(ObjectId::parse(String::from_utf8_lossy(THREE)).unwrap())
        );
        assert_eq!(
            entry.parent_statuses,
            vec![CombinedRawStatus::Renamed, CombinedRawStatus::TypeChanged]
        );
        assert_eq!(entry.path.as_bytes(), b"src/merge-\xff.rs");

        let combined_patch = b"diff --cc src/merge.rs\nindex 1111111,2222222..3333333\n--- a/src/merge.rs\n+++ b/src/merge.rs\n@@@ -1,1 -1,1 +1,1 @@@\n--old\n -other\n++new\n";
        assert_eq!(
            parse_unified_patch(combined_patch, PatchLimits::default())
                .unwrap_err()
                .kind,
            DiffErrorKind::MalformedPatch
        );
    }

    #[test]
    fn metadata_record_boundaries_are_deterministic_and_malformed_inputs_fail_closed() {
        let normal = b"7\t8\tplain.txt\0";
        let rename = b"1\t2\t\0old/\xff\0new/\xfe\0";
        let mut numstat = [normal.as_slice(), rename.as_slice()].concat();
        for cut in 1..numstat.len() {
            let result = parse_numstat_z(&numstat[..cut]);
            if cut == normal.len() {
                assert_eq!(result.unwrap().len(), 1, "cut {cut}");
            } else {
                assert_eq!(
                    result.unwrap_err().kind,
                    DiffErrorKind::MalformedMetadata,
                    "cut {cut}"
                );
            }
        }
        assert_eq!(parse_numstat_z(&numstat).unwrap().len(), 2);

        for malformed in [
            b"x\t1\ta\0".as_slice(),
            b"1\t-\ta\0",
            b"1\t2\t\0old\0",
            b"1\t2\t/absolute\0",
            b"18446744073709551616\t0\ta\0",
            b"1\t2\ta\0\0",
        ] {
            assert_eq!(
                parse_numstat_z(malformed).unwrap_err().kind,
                DiffErrorKind::MalformedMetadata,
                "malformed numstat {malformed:?}"
            );
        }

        let mut combined = b"::100644 160000 100755 ".to_vec();
        combined.extend_from_slice(ONE);
        combined.push(b' ');
        combined.extend_from_slice(TWO);
        combined.push(b' ');
        combined.extend_from_slice(THREE);
        combined.extend_from_slice(b" MD\0vendor/qt-bridges\0");
        for cut in 1..combined.len() {
            assert_eq!(
                parse_combined_raw_diff_z(&combined[..cut])
                    .unwrap_err()
                    .kind,
                DiffErrorKind::MalformedMetadata,
                "combined cut {cut}"
            );
        }
        assert_eq!(parse_combined_raw_diff_z(&combined).unwrap().len(), 1);

        let bad_combined = [
            b":100644 100644 1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 M\0a\0".as_slice(),
            b"::100644 100644 100644 1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 3333333333333333333333333333333333333333 M\0a\0",
            b"::100644 100644 100644 1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 3333333333333333333333333333333333333333 MX\0a\0",
        ];
        for malformed in bad_combined {
            assert_eq!(
                parse_combined_raw_diff_z(malformed).unwrap_err().kind,
                DiffErrorKind::MalformedMetadata
            );
        }

        let mixed_combined = format!(
            "::100644 100644 100644 {} {} {} MM\0path\0",
            "1".repeat(40),
            "2".repeat(64),
            "3".repeat(40)
        );
        assert_eq!(
            parse_combined_raw_diff_z(mixed_combined.as_bytes())
                .unwrap_err()
                .kind,
            DiffErrorKind::MalformedMetadata
        );

        // Deterministic byte mutation exercises every separator and payload position. The parser
        // may still accept a mutation that forms another valid record, but must never panic or
        // publish a partially parsed prefix.
        for index in 0..numstat.len() {
            let original = numstat[index];
            numstat[index] = 0;
            let _ = parse_numstat_z(&numstat);
            numstat[index] = original;
        }
    }

    #[test]
    fn metadata_limits_accept_the_boundary_and_reject_the_next_unit() {
        assert!(check_metadata_bytes(&vec![0; MAX_METADATA_BYTES]).is_ok());
        assert_eq!(
            check_metadata_bytes(&vec![0; MAX_METADATA_BYTES + 1])
                .unwrap_err()
                .kind,
            DiffErrorKind::TooLarge
        );

        let exact_path = vec![b'p'; MAX_DIFF_PATH_BYTES];
        let mut exact_path_record = b"0\t0\t".to_vec();
        exact_path_record.extend_from_slice(&exact_path);
        exact_path_record.push(0);
        assert_eq!(
            parse_numstat_z(&exact_path_record).unwrap()[0]
                .new_path
                .as_bytes()
                .len(),
            MAX_DIFF_PATH_BYTES
        );
        let mut long_path_record = b"0\t0\t".to_vec();
        long_path_record.extend(std::iter::repeat_n(b'p', MAX_DIFF_PATH_BYTES + 1));
        long_path_record.push(0);
        assert_eq!(
            parse_numstat_z(&long_path_record).unwrap_err().kind,
            DiffErrorKind::TooLarge
        );

        let combined = combined_record(MAX_COMBINED_PARENTS);
        assert_eq!(
            parse_combined_raw_diff_z(&combined).unwrap()[0]
                .parent_modes
                .len(),
            MAX_COMBINED_PARENTS
        );
        assert_eq!(
            parse_combined_raw_diff_z(&combined_record(MAX_COMBINED_PARENTS + 1))
                .unwrap_err()
                .kind,
            DiffErrorKind::TooLarge
        );

        let mut records = Vec::new();
        for index in 0..MAX_METADATA_RECORDS {
            records.extend_from_slice(format!("0\t0\tf{index}\0").as_bytes());
        }
        assert_eq!(
            parse_numstat_z(&records).unwrap().len(),
            MAX_METADATA_RECORDS
        );
        records.extend_from_slice(b"0\t0\toverflow\0");
        assert_eq!(
            parse_numstat_z(&records).unwrap_err().kind,
            DiffErrorKind::TooLarge
        );
    }

    fn combined_record(parent_count: usize) -> Vec<u8> {
        let mut input = vec![b':'; parent_count];
        for _ in 0..=parent_count {
            input.extend_from_slice(b"100644 ");
        }
        for _ in 0..=parent_count {
            input.extend_from_slice(ONE);
            input.push(b' ');
        }
        input.extend(std::iter::repeat_n(b'M', parent_count));
        input.extend_from_slice(b"\0path\0");
        input
    }

    #[test]
    fn raw_metadata_supplies_byte_exact_paths_for_quoted_patch_headers() {
        let expected_path = b"dir/space\tand-\xff.rs";
        let mut raw = b":100644 100644 ".to_vec();
        raw.extend_from_slice(ZERO);
        raw.push(b' ');
        raw.extend_from_slice(ONE);
        raw.extend_from_slice(b" M\0");
        raw.extend_from_slice(expected_path);
        raw.push(0);
        let patch_bytes = b"diff --git \"a/dir/space\\tand-\\377.rs\" \"b/dir/space\\tand-\\377.rs\"\n--- \"a/dir/space\\tand-\\377.rs\"\n+++ \"b/dir/space\\tand-\\377.rs\"\n@@ -1 +1 @@\n-old\n+new\n";
        let document =
            parse_unified_patch_with_raw_metadata(&raw, patch_bytes, PatchLimits::default())
                .unwrap();
        let file = &document.files[0];
        assert_eq!(file.id.old_path.as_ref().unwrap().as_bytes(), expected_path);
        assert_eq!(file.id.new_path.as_ref().unwrap().as_bytes(), expected_path);
        assert_eq!(file.kind, FileChangeKind::Modified);
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].lines[0].source, b"old");
        assert_eq!(file.hunks[0].lines[1].source, b"new");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn installed_git_raw_and_patch_streams_join_for_non_utf8_whitespace_path() {
        use std::ffi::OsString;
        use std::fs;
        use std::os::unix::ffi::OsStringExt;
        use std::process::Command;
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "devmil-git-p01-diff-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let git = |arguments: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(arguments)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {arguments:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "devmil-git Fixture"]);
        git(&["config", "user.email", "fixture@invalid.example"]);
        let expected_git_path = b"space and\ttab-\xff.txt";
        let file_path = root.join(OsString::from_vec(expected_git_path.to_vec()));
        fs::write(&file_path, b"old\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "base"]);
        fs::write(&file_path, b"new\n").unwrap();

        let raw = git(&["diff", "--raw", "-z", "--no-abbrev"]);
        let numstat = git(&["diff", "--numstat", "-z"]);
        let stats = parse_numstat_z_with_raw_metadata(&raw, &numstat).unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].added, DiffStatCount::Lines(1));
        assert_eq!(stats[0].deleted, DiffStatCount::Lines(1));
        assert_eq!(
            stats[0].file.new_path.as_ref().unwrap().as_bytes(),
            expected_git_path
        );
        let patch_stream = git(&["diff", "--patch", "--binary", "--full-index", "--no-color"]);
        let document =
            parse_unified_patch_with_raw_metadata(&raw, &patch_stream, PatchLimits::default())
                .unwrap();
        assert_eq!(document.files.len(), 1);
        assert_eq!(
            document.files[0].id.old_path.as_ref().unwrap().as_bytes(),
            expected_git_path
        );
        assert_eq!(
            document.files[0].id.new_path.as_ref().unwrap().as_bytes(),
            expected_git_path
        );
        assert_eq!(document.files[0].hunks[0].lines[0].source, b"old");
        assert_eq!(document.files[0].hunks[0].lines[1].source, b"new");
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn installed_git_raw_statuses_cover_chmod_type_change_and_modification() {
        use std::fs;
        use std::os::unix::fs::{symlink, PermissionsExt};
        use std::process::Command;
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "devmil-git-p04-raw-status-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let git = |arguments: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(arguments)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {arguments:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "devmil-git Fixture"]);
        git(&["config", "user.email", "fixture@invalid.example"]);
        fs::write(root.join("chmod.txt"), b"mode\n").unwrap();
        fs::write(root.join("type.txt"), b"regular\n").unwrap();
        fs::write(root.join("rewrite.txt"), b"old\n".repeat(200)).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "base"]);

        let mut permissions = fs::metadata(root.join("chmod.txt")).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(root.join("chmod.txt"), permissions).unwrap();
        fs::remove_file(root.join("type.txt")).unwrap();
        symlink("target", root.join("type.txt")).unwrap();
        fs::write(root.join("rewrite.txt"), b"new\n".repeat(200)).unwrap();

        let raw = git(&["diff", "--raw", "-z", "--no-abbrev"]);
        let entries = parse_raw_diff_z(&raw).unwrap();
        let by_path = |path: &[u8]| {
            entries
                .iter()
                .find(|entry| entry.new_path.as_bytes() == path)
                .unwrap()
        };
        assert_eq!(
            by_path(b"chmod.txt").raw_kind,
            RawFileChangeKind::ModeChanged
        );
        assert_eq!(
            by_path(b"type.txt").raw_kind,
            RawFileChangeKind::TypeChanged
        );
        assert_eq!(
            by_path(b"rewrite.txt").raw_kind,
            RawFileChangeKind::Modified
        );
        let patch = git(&["diff", "--patch", "--binary", "--full-index", "--no-color"]);
        let document =
            parse_unified_patch_with_raw_metadata(&raw, &patch, PatchLimits::default()).unwrap();
        let type_file = document
            .files
            .iter()
            .find(|file| {
                file.id
                    .new_path
                    .as_ref()
                    .is_some_and(|path| path.as_bytes() == b"type.txt")
            })
            .unwrap();
        assert_eq!(type_file.kind, FileChangeKind::TypeChanged);
        assert_eq!(type_file.id.old_path, type_file.id.new_path);
        assert_eq!(type_file.hunks.len(), 2);
        assert_eq!(type_file.hunks[0].id.ordinal, 0);
        assert_eq!(type_file.hunks[1].id.ordinal, 1);
        assert_type_change_patch_is_not_constructible(type_file);
        fs::remove_dir_all(&root).unwrap();
    }

    fn assert_type_change_patch_is_not_constructible(file: &DiffFile) {
        let selected_line = file
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .find(|line| line.is_change())
            .unwrap()
            .id
            .clone();
        let construction = BytePatchBuilder.construct_patch(&PatchConstructionRequest {
            file: file.clone(),
            selection: crate::diff::model::LineSelection::new(vec![selected_line]),
            context: crate::diff::model::ContextLines::new(3).unwrap(),
            reverse: false,
        });
        assert_eq!(
            construction.unwrap_err().kind,
            DiffErrorKind::UnsupportedBinary
        );
    }

    #[test]
    fn installed_git_conflict_raw_and_numstat_join_without_losing_record_identity() {
        use std::fs;
        use std::process::Command;
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "devmil-git-p04-conflict-stat-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let run = |arguments: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(arguments)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .unwrap()
        };
        let git = |arguments: &[&str]| {
            let output = run(arguments);
            assert!(
                output.status.success(),
                "git {arguments:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "devmil-git Fixture"]);
        git(&["config", "user.email", "fixture@invalid.example"]);
        fs::write(root.join("conflict.txt"), b"base\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "base"]);
        git(&["checkout", "-q", "-b", "side"]);
        fs::write(root.join("conflict.txt"), b"side\n").unwrap();
        git(&["commit", "-qam", "side"]);
        git(&["checkout", "-q", "main"]);
        fs::write(root.join("conflict.txt"), b"main\n").unwrap();
        git(&["commit", "-qam", "main"]);
        assert!(!run(&["merge", "--no-edit", "side"]).status.success());

        let raw = git(&["diff", "--raw", "-z", "--no-abbrev"]);
        let numstat = git(&["diff", "--numstat", "-z"]);
        let stats = parse_numstat_z_with_raw_metadata(&raw, &numstat).unwrap();
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].kind, FileChangeKind::Conflict);
        assert_eq!(stats[1].kind, FileChangeKind::Modified);
        for (ordinal, entry) in stats.iter().enumerate() {
            assert_eq!(entry.ordinal, u32::try_from(ordinal).unwrap());
            assert_eq!(
                entry.file.new_path.as_ref().unwrap().as_bytes(),
                b"conflict.txt"
            );
        }
        git(&["merge", "--abort"]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn parser_preserves_crlf_content_and_no_newline_marker() {
        let patch = b"diff --git a/NOTICE b/NOTICE\n--- a/NOTICE\n+++ b/NOTICE\n@@ -1,1 +1,1 @@\n-old\r\n+new\r\n\\ No newline at end of file\n";
        let document = parse_unified_patch(patch, PatchLimits::default()).unwrap();
        let hunk = &document.files[0].hunks[0];
        assert_eq!(hunk.lines[0].ending, LineEnding::CrLf);
        assert_eq!(hunk.lines[1].ending, LineEnding::CrLf);
        assert!(hunk.lines[1].no_newline_marker);
    }

    #[test]
    fn arbitrary_selection_recalculates_and_splits_hunks() {
        let patch = b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,6 +1,6 @@ section\n a\n-old-b\n+new-b\n c\n d\n-old-e\n+new-e\n f\n";
        let document = parse_unified_patch(patch, PatchLimits::default()).unwrap();
        let file = document.files[0].clone();
        let selected = vec![
            file.hunks[0].lines[1].id.clone(),
            file.hunks[0].lines[5].id.clone(),
        ];
        let built = BytePatchBuilder
            .construct_patch(&PatchConstructionRequest {
                file,
                selection: crate::diff::model::LineSelection::new(selected),
                context: crate::diff::model::ContextLines::new(1).unwrap(),
                reverse: false,
            })
            .unwrap();
        let text = String::from_utf8(built.bytes).unwrap();
        assert_eq!(text.matches("@@ -").count(), 2);
        assert!(text.contains("-old-b"));
        assert!(!text.contains("+new-b"));
        assert!(text.contains("-old-e"));
        assert_eq!(built.selected_change_count, 2);
    }

    #[test]
    fn all_small_selection_and_context_combinations_produce_parseable_patches() {
        let patch = b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,6 +1,6 @@ section\n a\n-old-b\n+new-b\n c\n d\n-old-e\n+new-e\n f\n";
        let document = parse_unified_patch(patch, PatchLimits::default()).unwrap();
        let file = document.files[0].clone();
        let changes = [1_usize, 2, 5, 6];
        for mask in 1_u8..16 {
            for context in 0..=3 {
                let ids = changes
                    .iter()
                    .enumerate()
                    .filter(|(bit, _)| mask & (1 << bit) != 0)
                    .map(|(_, index)| file.hunks[0].lines[*index].id.clone())
                    .collect::<Vec<_>>();
                let built = BytePatchBuilder
                    .construct_patch(&PatchConstructionRequest {
                        file: file.clone(),
                        selection: crate::diff::model::LineSelection::new(ids),
                        context: crate::diff::model::ContextLines::new(context).unwrap(),
                        reverse: false,
                    })
                    .unwrap();
                parse_unified_patch(&built.bytes, PatchLimits::default())
                    .unwrap_or_else(|error| panic!("mask {mask}, context {context}: {error}"));
                assert_eq!(built.selected_change_count, mask.count_ones() as usize);
            }
        }
    }

    #[test]
    fn adjacent_replacement_keeps_unselected_old_line_after_selected_pair() {
        let patch = b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,4 +1,4 @@\n one\n-two\n-three\n+TWO\n+THREE\n four\n";
        let document = parse_unified_patch(patch, PatchLimits::default()).unwrap();
        let file = document.files[0].clone();
        let selected = file.hunks[0]
            .lines
            .iter()
            .filter(|line| line.source == b"two" || line.source == b"TWO")
            .map(|line| line.id.clone())
            .collect::<Vec<_>>();
        let built = BytePatchBuilder
            .construct_patch(&PatchConstructionRequest {
                file,
                selection: crate::diff::model::LineSelection::new(selected),
                context: crate::diff::model::ContextLines::new(1).unwrap(),
                reverse: false,
            })
            .unwrap();
        let text = String::from_utf8(built.bytes).unwrap();
        assert!(text.contains(" one\n-two\n+TWO\n three\n"), "{text}");
        assert!(!text.contains(" three\n+TWO\n"), "{text}");
    }

    #[test]
    fn reverse_selection_uses_target_context_and_replacement_order() {
        let patch = b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n-one\r\n+ONE\r\n two\r\n-three\r\n+THREE\r\n";
        let document = parse_unified_patch(patch, PatchLimits::default()).unwrap();
        let file = document.files[0].clone();
        let selected = file.hunks[0]
            .lines
            .iter()
            .filter(|line| line.source == b"one" || line.source == b"ONE")
            .map(|line| line.id.clone())
            .collect::<Vec<_>>();
        let built = BytePatchBuilder
            .construct_patch(&PatchConstructionRequest {
                file,
                selection: crate::diff::model::LineSelection::new(selected),
                context: crate::diff::model::ContextLines::new(3).unwrap(),
                reverse: true,
            })
            .unwrap();
        let text = String::from_utf8(built.bytes).unwrap();
        assert!(text.contains("-ONE\r\n+one\r\n"), "{text}");
        assert!(text.contains(" THREE\r\n"), "{text}");
        assert!(!text.contains(" three\r\n"), "{text}");
    }

    #[test]
    fn special_file_metadata_maps_to_typed_states() {
        let rename = b"diff --git a/old.rs b/new.rs\nsimilarity index 92%\nrename from old.rs\nrename to new.rs\n";
        let document = parse_unified_patch(rename, PatchLimits::default()).unwrap();
        assert_eq!(
            document.files[0].kind,
            FileChangeKind::Renamed { similarity: 92 }
        );

        let mode = b"diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n";
        let document = parse_unified_patch(mode, PatchLimits::default()).unwrap();
        assert_eq!(document.files[0].kind, FileChangeKind::ModeChanged);
        assert_eq!(document.files[0].new_mode, Some(GitFileMode::EXECUTABLE));

        let deleted = b"diff --git a/old.rs b/old.rs\ndeleted file mode 100644\n--- a/old.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n";
        let document = parse_unified_patch(deleted, PatchLimits::default()).unwrap();
        assert_eq!(document.files[0].kind, FileChangeKind::Deleted);
        assert!(document.files[0].id.new_path.is_none());

        let submodule = b"diff --git a/vendor/lib b/vendor/lib\nindex 1111111..2222222 160000\n--- a/vendor/lib\n+++ b/vendor/lib\n@@ -1 +1 @@\n-Subproject commit 1111111111111111111111111111111111111111\n+Subproject commit 2222222222222222222222222222222222222222\n";
        let document = parse_unified_patch(submodule, PatchLimits::default()).unwrap();
        assert_eq!(document.files[0].kind, FileChangeKind::Submodule);
        assert_eq!(document.files[0].hunks.len(), 1);

        let image =
            b"diff --git a/icon.png b/icon.png\nBinary files a/icon.png and b/icon.png differ\n";
        let document = parse_unified_patch(image, PatchLimits::default()).unwrap();
        assert_eq!(document.files[0].kind, FileChangeKind::Image);
        assert_eq!(
            document.files[0].fallback,
            Some(DiffFallback::ImageDeferred)
        );

        for path in ["favicon.ico", "Photo.JPG", "scan.tiff", "legacy.bmp"] {
            let patch = format!(
                "diff --git a/{path} b/{path}\nBinary files a/{path} and b/{path} differ\n"
            );
            let document = parse_unified_patch(patch.as_bytes(), PatchLimits::default()).unwrap();
            assert_eq!(document.files[0].kind, FileChangeKind::Image, "{path}");
        }
        let binary =
            b"diff --git a/app.icns b/app.icns\nBinary files a/app.icns and b/app.icns differ\n";
        let document = parse_unified_patch(binary, PatchLimits::default()).unwrap();
        assert_eq!(document.files[0].kind, FileChangeKind::Binary);

        let svg = b"diff --git a/icon.svg b/icon.svg\n--- a/icon.svg\n+++ b/icon.svg\n@@ -1 +1 @@\n-<svg width=\"1\"/>\n+<svg width=\"2\"/>\n";
        let document = parse_unified_patch(svg, PatchLimits::default()).unwrap();
        assert_eq!(document.files[0].kind, FileChangeKind::Image);
        assert_eq!(
            document.files[0].fallback,
            Some(DiffFallback::ImageDeferred)
        );

        let conflict = b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-<<<<<<< main\n+>>>>>>> topic\n";
        let document = parse_unified_patch(conflict, PatchLimits::default()).unwrap();
        assert_eq!(document.files[0].kind, FileChangeKind::Conflict);
        assert!(matches!(
            document.files[0].hunks[0].lines[0].annotation,
            DiffLineAnnotation::ConflictBegin { .. }
        ));
    }

    #[test]
    fn huge_and_binary_inputs_return_typed_fallbacks() {
        let huge = b"diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-a\n+b\n";
        let document = parse_unified_patch(
            huge,
            PatchLimits {
                max_bytes: 1,
                ..PatchLimits::default()
            },
        )
        .unwrap();
        assert!(matches!(
            document.files[0].fallback,
            Some(DiffFallback::TooLarge { .. })
        ));

        let binary = b"diff --git a/a.bin b/a.bin\nBinary files a/a.bin and b/a.bin differ\n";
        let document = parse_unified_patch(binary, PatchLimits::default()).unwrap();
        assert!(matches!(
            document.files[0].fallback,
            Some(DiffFallback::Binary { .. })
        ));

        let mut very_large =
            b"diff --git a/large b/large\n--- a/large\n+++ b/large\n@@ -1,1 +1,1 @@\n".to_vec();
        very_large.extend(std::iter::repeat_n(b'x', 2 * 1024 * 1024));
        very_large.push(b'\n');
        let retained = bounded_patch_prefix(&very_large, 400);
        assert!(retained.len() <= 256 * 1024);
        let document = parse_unified_patch(&very_large, PatchLimits::default()).unwrap();
        assert_eq!(
            document.files[0].patch_header,
            b"diff --git a/large b/large\n--- a/large\n+++ b/large\n"
        );
        assert!(matches!(
            document.files[0].fallback,
            Some(DiffFallback::TooLarge {
                byte_count,
                line_count: 5,
                preview_line_limit: 400,
            }) if byte_count == very_large.len() as u64
        ));
    }

    #[test]
    fn huge_first_file_does_not_hide_later_file_and_totals_are_per_file() {
        let mut patch =
            b"diff --git a/large b/large\n--- a/large\n+++ b/large\n@@ -1 +1 @@\n-old\n+".to_vec();
        patch.extend(std::iter::repeat_n(b'x', 1024 * 1024));
        patch.extend_from_slice(
            b"\ndiff --git a/later b/later\n--- a/later\n+++ b/later\n@@ -1 +1 @@\n-before\n+after\n",
        );
        let later_start = patch
            .windows(b"diff --git a/later".len())
            .position(|window| window == b"diff --git a/later")
            .unwrap();
        let mut raw = Vec::new();
        for path in [b"large".as_slice(), b"later"] {
            raw.extend_from_slice(b":100644 100644 ");
            raw.extend_from_slice(ONE);
            raw.push(b' ');
            raw.extend_from_slice(ZERO);
            raw.extend_from_slice(b" M\0");
            raw.extend_from_slice(path);
            raw.push(0);
        }
        let document = parse_unified_patch_with_raw_metadata(
            &raw,
            &patch,
            PatchLimits {
                max_bytes: 512,
                max_lines: 100,
                preview_line_limit: 20,
            },
        )
        .unwrap();
        assert_eq!(document.files.len(), 2);
        assert_eq!(
            document.files[0].id.new_path.as_ref().unwrap().as_bytes(),
            b"large"
        );
        assert!(matches!(
            document.files[0].fallback,
            Some(DiffFallback::TooLarge {
                byte_count,
                line_count: 6,
                preview_line_limit: 20,
            }) if byte_count == later_start as u64
        ));
        assert_eq!(
            document.files[1].id.new_path.as_ref().unwrap().as_bytes(),
            b"later"
        );
        assert!(document.files[1].fallback.is_none());
        assert_eq!(document.files[1].hunks.len(), 1);
        assert_eq!(document.files[1].hunks[0].lines[0].source, b"before");
        assert_eq!(document.files[1].hunks[0].lines[1].source, b"after");
    }

    #[test]
    fn every_canonical_design_fixture_imports_to_domain_semantics() {
        let root: Value =
            serde_json::from_str(include_str!("fixtures/diff-fixtures.json")).unwrap();
        assert_eq!(required_str(&root, "schema"), "qspline.diff-fixture.v2");
        assert_eq!(required_str(&root, "version"), "2.0.0");
        let geometry = root["geometry"].as_object().unwrap();
        for (key, expected) in [
            ("lineHeight", 20),
            ("prefixWidth", 12),
            ("gutterWidth", 40),
            ("minCodeAreaUnified", 280),
            ("minCodeAreaSxs", 220),
            ("hunkBarHeight", 22),
        ] {
            assert_eq!(geometry[key].as_u64(), Some(expected));
        }

        let imported = root["fixtures"]
            .as_array()
            .unwrap()
            .iter()
            .map(import_production_design_fixture)
            .collect::<BTreeMap<_, _>>();
        assert_eq!(imported.len(), 18);
        assert_design_text_hunk(&imported["unified-hunk"]);
        let selected_fixture = &imported["partial-line-range"];
        assert_eq!(selected_fixture.selection.iter().count(), 2);
        selected_fixture
            .selection
            .validate_for_file(&selected_fixture.document.files[0])
            .unwrap();
        let partial = &imported["partially-staged-file"].document.files[0];
        assert_eq!(partial.staging, FileStagingState::PartiallyStaged);
        assert_eq!(partial.hunks[0].lines[1].staged, Some(true));
        assert_eq!(partial.hunks[0].lines[2].staged, Some(false));
        let no_newline = &imported["nonewline"].document.files[0].hunks[0].lines[0];
        assert_eq!(no_newline.ending, LineEnding::None);
        assert!(no_newline.no_newline_marker);
        assert!(matches!(
            imported["binary"].document.files[0].fallback,
            Some(DiffFallback::Binary { .. })
        ));
        assert_eq!(
            imported["image"].document.layout,
            DiffLayout::ImageSideBySide
        );
        assert_eq!(
            imported["submodule"].document.files[0].kind,
            FileChangeKind::Submodule
        );
        assert_eq!(
            imported["mode-change"].document.files[0].new_mode,
            Some(GitFileMode::EXECUTABLE)
        );
        assert_eq!(
            imported["rename"].document.files[0].kind,
            FileChangeKind::Renamed { similarity: 92 }
        );
        assert!(imported["deleted-file"].document.files[0]
            .id
            .new_path
            .is_none());
        assert!(matches!(
            imported["huge-file"].document.files[0].fallback,
            Some(DiffFallback::TooLarge {
                byte_count: 1_200_000,
                line_count: 48_000,
                preview_line_limit: 400,
            })
        ));
        assert_eq!(
            imported["syntax-unavailable"].document.files[0].syntax,
            SyntaxAvailability::Unavailable
        );
        assert!(matches!(
            imported["conflict-markers"].document.files[0].hunks[0].lines[0].annotation,
            DiffLineAnnotation::ConflictBegin { .. }
        ));
        assert_eq!(
            imported["untracked"].document.files[0].kind,
            FileChangeKind::Untracked
        );
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct ImportedDesignFixture {
        document: DiffDocument,
        selection: LineSelection,
    }

    fn required_str<'a>(value: &'a Value, key: &str) -> &'a str {
        value[key]
            .as_str()
            .unwrap_or_else(|| panic!("{key} must be a string"))
    }

    fn import_production_design_fixture(value: &Value) -> (String, ImportedDesignFixture) {
        let id = required_str(value, "id").to_owned();
        let layout = match required_str(value, "layout") {
            "unified" => DiffLayout::Unified,
            "side-by-side" => DiffLayout::SideBySide,
            "image-sxs" => DiffLayout::ImageSideBySide,
            other => panic!("unknown design layout {other}"),
        };
        let path = DiffPath::from_bytes(required_str(value, "path").as_bytes().to_vec()).unwrap();
        for text_key in ["message", "note"] {
            if value.get(text_key).is_some() {
                assert!(!required_str(value, text_key).is_empty());
            }
        }
        let design_kind = value.get("kind").and_then(Value::as_str);
        let kind = match design_kind {
            Some("binary") => FileChangeKind::Binary,
            Some("image") => FileChangeKind::Image,
            Some("submodule") => FileChangeKind::Submodule,
            Some("mode") => FileChangeKind::ModeChanged,
            Some("rename") => FileChangeKind::Renamed {
                similarity: u8::try_from(value["similarity"].as_u64().unwrap()).unwrap(),
            },
            Some("deleted") => FileChangeKind::Deleted,
            Some("huge" | "decode-error") | None => FileChangeKind::Modified,
            Some("conflict") => FileChangeKind::Conflict,
            Some("untracked") => FileChangeKind::Untracked,
            Some(other) => panic!("unknown design file kind {other}"),
        };
        let old_path = match design_kind {
            Some("untracked") => None,
            Some("rename") => {
                Some(DiffPath::from_bytes(required_str(value, "from").as_bytes().to_vec()).unwrap())
            }
            _ => Some(path.clone()),
        };
        let new_path = (design_kind != Some("deleted")).then_some(path);
        let file_id = DiffFileId { old_path, new_path };
        let (hunks, selection) = import_design_hunks(value, &file_id);
        if let Some(selected_indexes) = value.get("selectedLines").and_then(Value::as_array) {
            assert_eq!(selection.iter().count(), selected_indexes.len());
            for index in selected_indexes {
                let source = &value["lines"][usize::try_from(index.as_u64().unwrap()).unwrap()];
                assert_eq!(source["selected"], true);
            }
        }
        let syntax = match (design_kind, value.get("language")) {
            (Some("decode-error"), _) | (_, Some(Value::Null)) => SyntaxAvailability::Unavailable,
            (_, Some(Value::String(language))) => {
                SyntaxAvailability::Available(language.as_bytes().to_vec())
            }
            _ => syntax_for_path(file_id.new_path.as_ref().or(file_id.old_path.as_ref())),
        };
        let fallback = match design_kind {
            Some("binary") => Some(DiffFallback::Binary {
                byte_count: Some(12 * 1024),
            }),
            Some("image") => Some(DiffFallback::ImageDeferred),
            Some("huge") => Some(DiffFallback::TooLarge {
                byte_count: 1_200_000,
                line_count: 48_000,
                preview_line_limit: 400,
            }),
            _ => None,
        };
        let (old_mode, new_mode) = match design_kind {
            Some("mode") => (Some(GitFileMode::REGULAR), Some(GitFileMode::EXECUTABLE)),
            Some("submodule") => (Some(GitFileMode::GITLINK), Some(GitFileMode::GITLINK)),
            _ => (None, None),
        };
        let staging = if value["fileState"] == "partial" {
            FileStagingState::PartiallyStaged
        } else {
            FileStagingState::NotApplicable
        };
        let file = DiffFile {
            id: file_id,
            kind,
            old_mode,
            new_mode,
            syntax,
            staging,
            hunks,
            fallback,
            patch_header: Vec::new(),
        };
        (
            id,
            ImportedDesignFixture {
                document: DiffDocument {
                    layout,
                    files: vec![file],
                },
                selection,
            },
        )
    }

    fn import_design_hunks(value: &Value, file: &DiffFileId) -> (Vec<DiffHunk>, LineSelection) {
        let Some(source_lines) = value.get("lines").and_then(Value::as_array) else {
            return (Vec::new(), LineSelection::new(Vec::new()));
        };
        let header = source_lines.iter().find(|line| line["kind"] == "hunk");
        let (old_start, old_count, new_start, new_count, section) = header
            .map_or((1, 0, 1, 0, Vec::new()), |line| {
                parse_hunk_header(required_str(line, "header").as_bytes()).unwrap()
            });
        let hunk_id = HunkId {
            file: file.clone(),
            old_start,
            new_start,
            ordinal: 0,
        };
        let mut lines = Vec::new();
        let mut selected = Vec::new();
        for (ordinal, source) in source_lines
            .iter()
            .filter(|line| line["kind"] != "hunk" && line["kind"] != "nonewline")
            .enumerate()
        {
            let line = import_design_line(source, &hunk_id, ordinal);
            if source["selected"] == true {
                selected.push(line.id.clone());
            }
            lines.push(line);
        }
        if source_lines.iter().any(|line| line["kind"] == "nonewline") {
            let last = lines.last_mut().unwrap();
            last.ending = LineEnding::None;
            last.no_newline_marker = true;
        }
        if lines.is_empty() {
            return (Vec::new(), LineSelection::new(selected));
        }
        let old_count = if header.is_some() {
            old_count
        } else {
            u32::try_from(
                lines
                    .iter()
                    .filter(|line| line.origin != DiffLineOrigin::Addition)
                    .count(),
            )
            .unwrap()
        };
        let new_count = if header.is_some() {
            new_count
        } else {
            u32::try_from(
                lines
                    .iter()
                    .filter(|line| line.origin != DiffLineOrigin::Deletion)
                    .count(),
            )
            .unwrap()
        };
        (
            vec![DiffHunk {
                id: hunk_id,
                old_count,
                new_count,
                section,
                lines,
            }],
            LineSelection::new(selected),
        )
    }

    fn import_design_line(source: &Value, hunk: &HunkId, ordinal: usize) -> DiffLine {
        let kind = required_str(source, "kind");
        let origin = match kind {
            "add" => DiffLineOrigin::Addition,
            "del" => DiffLineOrigin::Deletion,
            "ctx" | "conflict-begin" | "conflict-sep" | "conflict-end" => DiffLineOrigin::Context,
            other => panic!("unknown design line kind {other}"),
        };
        let annotation = match kind {
            "conflict-begin" => DiffLineAnnotation::ConflictBegin {
                label: required_str(source, "label").as_bytes().to_vec(),
            },
            "conflict-sep" => DiffLineAnnotation::ConflictSeparator,
            "conflict-end" => DiffLineAnnotation::ConflictEnd {
                label: required_str(source, "label").as_bytes().to_vec(),
            },
            _ => DiffLineAnnotation::None,
        };
        let old_line = design_line_number(source.get("oldNo"));
        let new_line = design_line_number(source.get("newNo"));
        let intra_line = source.get("intra").map_or_else(Vec::new, |value| {
            let values = value.as_array().unwrap();
            vec![crate::diff::model::ByteSpan::new(
                usize::try_from(values[0].as_u64().unwrap()).unwrap(),
                usize::try_from(values[1].as_u64().unwrap()).unwrap(),
            )
            .unwrap()]
        });
        DiffLine {
            id: DiffLineId {
                hunk: hunk.clone(),
                old_line,
                new_line,
                occurrence: u16::try_from(ordinal).unwrap(),
            },
            origin,
            source: required_str(source, "text").as_bytes().to_vec(),
            ending: LineEnding::Lf,
            no_newline_marker: false,
            staged: source.get("staged").and_then(Value::as_bool),
            intra_line,
            annotation,
        }
    }

    fn design_line_number(value: Option<&Value>) -> Option<u32> {
        value
            .and_then(Value::as_str)
            .filter(|number| !number.is_empty())
            .map(|number| number.parse().unwrap())
    }

    fn assert_design_text_hunk(fixture: &ImportedDesignFixture) {
        let file = &fixture.document.files[0];
        assert_eq!(file.kind, FileChangeKind::Modified);
        assert_eq!(file.hunks[0].id.old_start, 118);
        assert_eq!(file.hunks[0].old_count, 7);
        assert_eq!(file.hunks[0].new_count, 9);
        assert_eq!(file.hunks[0].section, b" fn paint_lane");
        assert_eq!(file.hunks[0].lines.len(), 6);
        assert_eq!(file.hunks[0].lines[0].id.old_line, Some(118));
        assert_eq!(file.hunks[0].lines[1].origin, DiffLineOrigin::Deletion);
        assert_eq!(file.hunks[0].lines[1].intra_line[0].start, 9);
        assert_eq!(file.hunks[0].lines[1].intra_line[0].end, 16);
    }
}
