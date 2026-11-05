//! Byte-safe diff, patch, and staging value types.

use std::collections::BTreeSet;
use std::fmt;

/// A Git path exactly as emitted by Git. Display conversion is always explicit.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DiffPath(Vec<u8>);

impl DiffPath {
    /// # Errors
    ///
    /// Returns [`DiffModelError::InvalidPath`] for empty, absolute, aliased, or NUL paths.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Result<Self, DiffModelError> {
        let bytes = bytes.into();
        if bytes.is_empty()
            || bytes.contains(&0)
            || bytes.starts_with(b"/")
            || bytes
                .split(|byte| *byte == b'/')
                .any(|component| component.is_empty() || component == b"." || component == b"..")
        {
            return Err(DiffModelError::InvalidPath);
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub(crate) fn allocated_bytes(&self) -> usize {
        self.0.capacity()
    }

    pub fn to_string_lossy(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }
}

/// A six-digit Git file mode.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GitFileMode(u32);

impl GitFileMode {
    pub const REGULAR: Self = Self(0o100_644);
    pub const EXECUTABLE: Self = Self(0o100_755);
    pub const SYMLINK: Self = Self(0o120_000);
    pub const GITLINK: Self = Self(0o160_000);

    /// # Errors
    ///
    /// Returns [`DiffModelError::InvalidMode`] unless `bytes` is six octal digits.
    pub fn from_octal_bytes(bytes: &[u8]) -> Result<Self, DiffModelError> {
        if bytes.len() != 6 || !bytes.iter().all(|byte| matches!(byte, b'0'..=b'7')) {
            return Err(DiffModelError::InvalidMode);
        }
        let mut value = 0_u32;
        for byte in bytes {
            value = value * 8 + u32::from(byte - b'0');
        }
        Ok(Self(value))
    }

    pub const fn value(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DiffLayout {
    Unified,
    SideBySide,
    ImageSideBySide,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum FileChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed { similarity: u8 },
    Copied { similarity: u8 },
    ModeChanged,
    TypeChanged,
    Binary,
    Image,
    Submodule,
    Conflict,
    Untracked,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum SyntaxAvailability {
    Available(Vec<u8>),
    Unavailable,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum DiffFallback {
    Binary {
        byte_count: Option<u64>,
    },
    TooLarge {
        byte_count: u64,
        line_count: u64,
        preview_line_limit: u32,
    },
    ImageDeferred,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FileStagingState {
    Unstaged,
    Staged,
    PartiallyStaged,
    NotApplicable,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum LineEnding {
    Lf,
    CrLf,
    None,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DiffLineOrigin {
    Context,
    Addition,
    Deletion,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiffLineAnnotation {
    None,
    ConflictBegin { label: Vec<u8> },
    ConflictSeparator,
    ConflictEnd { label: Vec<u8> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteSpan {
    pub start: usize,
    pub end: usize,
}

impl ByteSpan {
    /// # Errors
    ///
    /// Returns [`DiffModelError::InvalidByteSpan`] for empty or reversed spans.
    pub fn new(start: usize, end: usize) -> Result<Self, DiffModelError> {
        if start >= end {
            return Err(DiffModelError::InvalidByteSpan);
        }
        Ok(Self { start, end })
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DiffFileId {
    pub old_path: Option<DiffPath>,
    pub new_path: Option<DiffPath>,
}

/// One side of Git's `--numstat` result. Binary files deliberately do not invent line counts.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DiffStatCount {
    Lines(u64),
    Binary,
}

/// Byte-safe line totals associated with the same stable file identity as patch data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffNumStat {
    pub file: DiffFileId,
    /// Zero-based occurrence among records with the same stable file identity in one frozen
    /// request. Conflict-stage duplicates therefore remain distinct without changing `DiffFileId`.
    pub ordinal: u32,
    pub kind: FileChangeKind,
    pub added: DiffStatCount,
    pub deleted: DiffStatCount,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HunkId {
    pub file: DiffFileId,
    pub old_start: u32,
    pub new_start: u32,
    pub ordinal: u32,
}

/// Stable line identity based on source coordinates, not a rendered row index.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DiffLineId {
    pub hunk: HunkId,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub occurrence: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffLine {
    pub id: DiffLineId,
    pub origin: DiffLineOrigin,
    /// Content without the diff prefix or line terminator.
    pub source: Vec<u8>,
    pub ending: LineEnding,
    pub no_newline_marker: bool,
    pub staged: Option<bool>,
    pub intra_line: Vec<ByteSpan>,
    pub annotation: DiffLineAnnotation,
}

impl DiffLine {
    pub fn display_text_lossy(&self) -> String {
        String::from_utf8_lossy(&self.source).into_owned()
    }

    pub const fn is_change(&self) -> bool {
        matches!(
            self.origin,
            DiffLineOrigin::Addition | DiffLineOrigin::Deletion
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffHunk {
    pub id: HunkId,
    pub old_count: u32,
    pub new_count: u32,
    /// Function context following the second `@@`, including its leading separator.
    pub section: Vec<u8>,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffFile {
    pub id: DiffFileId,
    pub kind: FileChangeKind,
    pub old_mode: Option<GitFileMode>,
    pub new_mode: Option<GitFileMode>,
    pub syntax: SyntaxAvailability,
    pub staging: FileStagingState,
    pub hunks: Vec<DiffHunk>,
    pub fallback: Option<DiffFallback>,
    /// Headers before the first hunk, retained for byte-exact patch construction.
    pub patch_header: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffDocument {
    pub layout: DiffLayout,
    pub files: Vec<DiffFile>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineSelection {
    ids: BTreeSet<DiffLineId>,
}

impl LineSelection {
    pub fn new(ids: impl IntoIterator<Item = DiffLineId>) -> Self {
        Self {
            ids: ids.into_iter().collect(),
        }
    }

    pub fn contains(&self, id: &DiffLineId) -> bool {
        self.ids.contains(id)
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &DiffLineId> {
        self.ids.iter()
    }

    /// # Errors
    ///
    /// Returns a typed error for empty, foreign, missing, or context-line identities.
    pub fn validate_for_file(&self, file: &DiffFile) -> Result<(), DiffModelError> {
        if self.is_empty() {
            return Err(DiffModelError::EmptySelection);
        }
        for selected in &self.ids {
            if selected.hunk.file != file.id {
                return Err(DiffModelError::SelectionFromAnotherFile);
            }
            let line = file
                .hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .find(|line| line.id == *selected)
                .ok_or(DiffModelError::UnknownLineIdentity)?;
            if !line.is_change() {
                return Err(DiffModelError::ContextLineSelected);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextLines(u16);

impl ContextLines {
    pub const MAX: u16 = 10_000;
    pub const DEFAULT: Self = Self(3);

    /// # Errors
    ///
    /// Returns [`DiffModelError::ContextTooLarge`] above [`Self::MAX`].
    pub fn new(value: u16) -> Result<Self, DiffModelError> {
        if value > Self::MAX {
            return Err(DiffModelError::ContextTooLarge);
        }
        Ok(Self(value))
    }

    pub const fn value(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiffModelError {
    InvalidPath,
    InvalidMode,
    EmptySelection,
    SelectionFromAnotherFile,
    UnknownLineIdentity,
    ContextLineSelected,
    ContextTooLarge,
    InvalidByteSpan,
}

impl fmt::Display for DiffModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPath => "diff path is not a valid repository-relative byte path",
            Self::InvalidMode => "Git mode is not six octal digits",
            Self::EmptySelection => "line selection is empty",
            Self::SelectionFromAnotherFile => "line selection belongs to another file",
            Self::UnknownLineIdentity => "line selection contains an unknown identity",
            Self::ContextLineSelected => "line selection contains a context line",
            Self::ContextTooLarge => "requested context exceeds the supported bound",
            Self::InvalidByteSpan => "intra-line byte span is empty or reversed",
        })
    }
}

impl std::error::Error for DiffModelError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_preserves_non_utf8_and_lossy_conversion_is_explicit() {
        let path = DiffPath::from_bytes(b"src/\xff.rs".to_vec()).unwrap();
        assert_eq!(path.as_bytes(), b"src/\xff.rs");
        assert_eq!(path.to_string_lossy(), "src/�.rs");
    }

    #[test]
    fn mode_requires_exact_octal_bytes() {
        assert_eq!(
            GitFileMode::from_octal_bytes(b"100755").unwrap(),
            GitFileMode::EXECUTABLE
        );
        assert_eq!(
            GitFileMode::from_octal_bytes(b"10075"),
            Err(DiffModelError::InvalidMode)
        );
        assert_eq!(
            GitFileMode::from_octal_bytes(b"10085x"),
            Err(DiffModelError::InvalidMode)
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffWhitespacePolicy {
    Preserve,
    IgnoreAll,
    IgnoreAmount,
    IgnoreEndOfLine,
}
