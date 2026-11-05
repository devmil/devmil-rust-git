//! Exact source identity for the two sides of a parsed diff file.
//!
//! A diff shows fragments of two source versions. Enrichment that needs the
//! whole source (syntax state from the start of a file, for example) must read
//! exactly those versions, never a current path that may have changed since
//! the diff was produced. These helpers take the identity from metadata the
//! diff read already produced and prove a captured source against every line
//! the diff displays.

use crate::diff::model::{DiffFile, DiffLineOrigin, LineEnding};
use crate::types::ObjectId;
use std::collections::BTreeSet;

/// One side of a two-sided diff.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DiffSide {
    Old,
    New,
}

/// The blob identities a `--full-index` patch header names for each side.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatchObjectIds {
    /// `None` when the side does not exist (an addition's old side).
    pub old: Option<ObjectId>,
    /// `None` when the side does not exist (a deletion's new side).
    pub new: Option<ObjectId>,
}

impl PatchObjectIds {
    pub const fn side(&self, side: DiffSide) -> Option<&ObjectId> {
        match side {
            DiffSide::Old => self.old.as_ref(),
            DiffSide::New => self.new.as_ref(),
        }
    }
}

/// Reads the full object IDs of a two-sided patch's `index` header line.
///
/// Returns `None` when the header has no `index` line, abbreviates an ID,
/// names more than one parent (combined or conflict formats) or is otherwise
/// not an exact two-sided identity. An all-zero ID denotes a missing side.
/// A worktree side has no stored object, so callers identify it by content.
pub fn patch_object_ids(file: &DiffFile) -> Option<PatchObjectIds> {
    let mut found = None;
    for line in file.patch_header.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(rest) = line.strip_prefix(b"index ") else {
            continue;
        };
        if found.is_some() {
            return None;
        }
        let range = rest.split(|byte| *byte == b' ').next()?;
        let separator = range.windows(2).position(|pair| pair == b"..")?;
        let (old, new) = (&range[..separator], &range[separator + 2..]);
        found = Some(PatchObjectIds {
            old: header_object(old)?.into_object(),
            new: header_object(new)?.into_object(),
        });
    }
    found
}

/// One side of an `index` header line.
enum HeaderObject {
    Missing,
    Object(ObjectId),
}

impl HeaderObject {
    fn into_object(self) -> Option<ObjectId> {
        match self {
            Self::Missing => None,
            Self::Object(object) => Some(object),
        }
    }
}

fn header_object(bytes: &[u8]) -> Option<HeaderObject> {
    if bytes.contains(&b',') {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let object = ObjectId::parse(text).ok()?;
    Some(if bytes.iter().all(|byte| *byte == b'0') {
        HeaderObject::Missing
    } else {
        HeaderObject::Object(object)
    })
}

/// Why a captured source cannot stand for a diff side.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceMismatch {
    /// A changed line the diff shows differs from the source line it names.
    ChangedLine,
    /// The diff names a line the source does not have.
    MissingLine,
}

/// Verifies `source` against every line the diff displays for `side`.
///
/// Every changed line of the side (deletions of the old side, additions of
/// the new side) must equal its source line byte for byte, including its
/// terminator; otherwise the source is not the version the diff was produced
/// from. Context lines can legitimately differ from one side when whitespace
/// is ignored, so a mismatching context line is only left out of the result.
///
/// Returns the one-based line numbers of `side` whose displayed text equals
/// the source exactly.
///
/// # Errors
/// Returns [`SourceMismatch`] when the source is not the side's version.
pub fn verify_displayed_lines(
    file: &DiffFile,
    side: DiffSide,
    source: &[u8],
) -> Result<BTreeSet<u32>, SourceMismatch> {
    let lines = SourceLines::new(source);
    let mut verified = BTreeSet::new();
    for line in file.hunks.iter().flat_map(|hunk| &hunk.lines) {
        let (number, changed) = match (side, line.origin) {
            (DiffSide::Old, DiffLineOrigin::Deletion) => (line.id.old_line, true),
            (DiffSide::New, DiffLineOrigin::Addition) => (line.id.new_line, true),
            (DiffSide::Old, DiffLineOrigin::Context) => (line.id.old_line, false),
            (DiffSide::New, DiffLineOrigin::Context) => (line.id.new_line, false),
            _ => continue,
        };
        let Some(number) = number else {
            continue;
        };
        let Some((content, ending)) = lines.line(number) else {
            if changed {
                return Err(SourceMismatch::MissingLine);
            }
            continue;
        };
        if content == line.source.as_slice() && ending == line.ending {
            verified.insert(number);
        } else if changed {
            return Err(SourceMismatch::ChangedLine);
        }
    }
    Ok(verified)
}

/// The lines of one source, split at LF with CRLF recognized, as a diff
/// parser splits them. Indexing is one-based.
#[derive(Clone, Debug)]
pub struct SourceLines<'a> {
    source: &'a [u8],
    starts: Vec<usize>,
}

impl<'a> SourceLines<'a> {
    pub fn new(source: &'a [u8]) -> Self {
        let mut starts = vec![0];
        starts.extend(
            source
                .iter()
                .enumerate()
                .filter(|(_, byte)| **byte == b'\n')
                .map(|(index, _)| index + 1),
        );
        if starts.last() == Some(&source.len()) {
            starts.pop();
        }
        Self { source, starts }
    }

    /// Number of lines, counting a final line without a terminator.
    pub fn len(&self) -> usize {
        self.starts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    /// Line `number` including its terminator.
    pub fn complete(&self, number: u32) -> Option<&'a [u8]> {
        let index = usize::try_from(number).ok()?.checked_sub(1)?;
        let start = *self.starts.get(index)?;
        let end = self
            .starts
            .get(index + 1)
            .copied()
            .unwrap_or(self.source.len());
        Some(&self.source[start..end])
    }

    /// Line `number` without its terminator, and the terminator.
    pub fn line(&self, number: u32) -> Option<(&'a [u8], LineEnding)> {
        let complete = self.complete(number)?;
        Some(if let Some(content) = complete.strip_suffix(b"\r\n") {
            (content, LineEnding::CrLf)
        } else if let Some(content) = complete.strip_suffix(b"\n") {
            (content, LineEnding::Lf)
        } else {
            (complete, LineEnding::None)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::model::{
        DiffFileId, DiffHunk, DiffLine, DiffLineAnnotation, DiffLineId, DiffPath, FileChangeKind,
        FileStagingState, HunkId, SyntaxAvailability,
    };

    const OLD: &str = "1111111111111111111111111111111111111111";
    const NEW: &str = "2222222222222222222222222222222222222222";

    fn file(
        header: &str,
        lines: Vec<(DiffLineOrigin, Option<u32>, Option<u32>, &str)>,
    ) -> DiffFile {
        let id = DiffFileId {
            old_path: Some(DiffPath::from_bytes(b"a.rs".to_vec()).unwrap()),
            new_path: Some(DiffPath::from_bytes(b"a.rs".to_vec()).unwrap()),
        };
        let hunk = HunkId {
            file: id.clone(),
            old_start: 1,
            new_start: 1,
            ordinal: 0,
        };
        DiffFile {
            id,
            kind: FileChangeKind::Modified,
            old_mode: None,
            new_mode: None,
            syntax: SyntaxAvailability::Unavailable,
            staging: FileStagingState::NotApplicable,
            hunks: vec![DiffHunk {
                id: hunk.clone(),
                old_count: 0,
                new_count: 0,
                section: Vec::new(),
                lines: lines
                    .into_iter()
                    .map(|(origin, old_line, new_line, text)| {
                        let (source, ending) = text.strip_suffix("\r\n").map_or_else(
                            || {
                                text.strip_suffix('\n')
                                    .map_or((text, LineEnding::None), |value| {
                                        (value, LineEnding::Lf)
                                    })
                            },
                            |value| (value, LineEnding::CrLf),
                        );
                        DiffLine {
                            id: DiffLineId {
                                hunk: hunk.clone(),
                                old_line,
                                new_line,
                                occurrence: 0,
                            },
                            origin,
                            source: source.as_bytes().to_vec(),
                            ending,
                            no_newline_marker: ending == LineEnding::None,
                            staged: None,
                            intra_line: Vec::new(),
                            annotation: DiffLineAnnotation::None,
                        }
                    })
                    .collect(),
            }],
            fallback: None,
            patch_header: header.as_bytes().to_vec(),
        }
    }

    #[test]
    fn full_index_header_names_both_sides_and_missing_sides() {
        let modified = file(
            &format!("diff --git a/a.rs b/a.rs\nindex {OLD}..{NEW} 100644\n"),
            Vec::new(),
        );
        let ids = patch_object_ids(&modified).unwrap();
        assert_eq!(ids.old.unwrap().as_str(), OLD);
        assert_eq!(ids.new.unwrap().as_str(), NEW);
        let added = file(
            &format!("new file mode 100644\nindex {}..{NEW}\n", "0".repeat(40)),
            Vec::new(),
        );
        let ids = patch_object_ids(&added).unwrap();
        assert!(ids.old.is_none());
        assert_eq!(ids.side(DiffSide::New).unwrap().as_str(), NEW);
    }

    #[test]
    fn abbreviated_combined_and_missing_headers_are_not_identities() {
        assert!(patch_object_ids(&file("index 1234567..89abcde 100644\n", Vec::new())).is_none());
        assert!(
            patch_object_ids(&file(&format!("index {OLD},{OLD}..{NEW}\n"), Vec::new())).is_none()
        );
        assert!(patch_object_ids(&file("diff --git a/a b/a\n", Vec::new())).is_none());
    }

    #[test]
    fn source_lines_follow_diff_line_splitting() {
        let lines = SourceLines::new(b"a\r\nb\nc");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines.line(1), Some((b"a".as_slice(), LineEnding::CrLf)));
        assert_eq!(lines.line(2), Some((b"b".as_slice(), LineEnding::Lf)));
        assert_eq!(lines.line(3), Some((b"c".as_slice(), LineEnding::None)));
        assert_eq!(lines.line(4), None);
        assert_eq!(lines.line(0), None);
        assert!(SourceLines::new(b"").is_empty());
        assert_eq!(SourceLines::new(b"x\n").len(), 1);
    }

    #[test]
    fn changed_lines_prove_the_side_and_context_lines_may_differ() {
        let diff = file(
            "",
            vec![
                (DiffLineOrigin::Context, Some(1), Some(1), "fn a() {\n"),
                (DiffLineOrigin::Deletion, Some(2), None, "    old();\n"),
                (DiffLineOrigin::Addition, None, Some(2), "    new();\n"),
                (DiffLineOrigin::Context, Some(3), Some(3), "}"),
            ],
        );
        assert_eq!(
            verify_displayed_lines(&diff, DiffSide::Old, b"fn a() {\n    old();\n}").unwrap(),
            BTreeSet::from([1, 2, 3])
        );
        // Whitespace-ignoring diffs may show context from the other side.
        assert_eq!(
            verify_displayed_lines(&diff, DiffSide::New, b"fn a()  {\n    new();\n}").unwrap(),
            BTreeSet::from([2, 3])
        );
        // An equal-size edit of a changed line is drift.
        assert_eq!(
            verify_displayed_lines(&diff, DiffSide::New, b"fn a() {\n    nex();\n}"),
            Err(SourceMismatch::ChangedLine)
        );
        // A terminator is part of the line.
        assert_eq!(
            verify_displayed_lines(&diff, DiffSide::Old, b"fn a() {\n    old();\r\n}"),
            Err(SourceMismatch::ChangedLine)
        );
        assert_eq!(
            verify_displayed_lines(&diff, DiffSide::New, b"fn a() {\n"),
            Err(SourceMismatch::MissingLine)
        );
    }
}
