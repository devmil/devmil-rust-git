//! Partial-patch requests and bounded diff errors.

use super::model::{ContextLines, DiffFile, LineSelection};
use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatchConstructionRequest {
    pub file: DiffFile,
    pub selection: LineSelection,
    pub context: ContextLines,
    pub reverse: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConstructedPatch {
    pub bytes: Vec<u8>,
    pub selected_change_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiffErrorKind {
    GitFailed,
    MalformedMetadata,
    MalformedPatch,
    UnsupportedBinary,
    TooLarge,
    InvalidSelection,
    Canceled,
}

/// Bounded error suitable for publication. Raw Git output remains in the activity contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffError {
    pub kind: DiffErrorKind,
    detail: String,
}

impl DiffError {
    pub const MAX_DETAIL_BYTES: usize = 512;

    pub fn new(kind: DiffErrorKind, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        if detail.len() > Self::MAX_DETAIL_BYTES {
            let mut boundary = Self::MAX_DETAIL_BYTES;
            while !detail.is_char_boundary(boundary) {
                boundary -= 1;
            }
            detail.truncate(boundary);
        }
        Self { kind, detail }
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for DiffError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for DiffError {}
