//! Parsing for Git LFS pointers and tracked-file JSON output.

use crate::types::RepositoryRelativePath;
use std::collections::BTreeSet;

/// Invalid LFS file-list output or a consumer-selected count bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LfsFilesError {
    InvalidJson,
    TooManyItems,
    InvalidPath,
}

impl std::fmt::Display for LfsFilesError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidJson => "git-lfs status JSON is malformed",
            Self::TooManyItems => "LFS path count exceeds its bound",
            Self::InvalidPath => "git-lfs returned an unsafe path",
        })
    }
}

impl std::error::Error for LfsFilesError {}

/// Parses `git lfs ls-files --json` into sorted, deduplicated repository paths.
///
/// Missing or null `files` means an empty list. Additional metadata is ignored.
/// The count limit applies before deduplication; callers also bound input bytes.
///
/// # Errors
/// Rejects malformed JSON/schema, an excessive file count, and unsafe paths.
pub fn parse_files_json(
    raw: &[u8],
    maximum_items: usize,
) -> Result<Vec<RepositoryRelativePath>, LfsFilesError> {
    let value: serde_json::Value =
        serde_json::from_slice(raw).map_err(|_| LfsFilesError::InvalidJson)?;
    let object = value.as_object().ok_or(LfsFilesError::InvalidJson)?;
    let files = match object.get("files") {
        None | Some(serde_json::Value::Null) => return Ok(Vec::new()),
        Some(value) => value.as_array().ok_or(LfsFilesError::InvalidJson)?,
    };
    if files.len() > maximum_items {
        return Err(LfsFilesError::TooManyItems);
    }
    let mut unique = BTreeSet::new();
    for file in files {
        let name = file
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or(LfsFilesError::InvalidJson)?;
        if name.contains('\0') {
            return Err(LfsFilesError::InvalidPath);
        }
        unique.insert(RepositoryRelativePath::new(name).map_err(|_| LfsFilesError::InvalidPath)?);
    }
    Ok(unique.into_iter().collect())
}

/// Object identity and original content size from an LFS pointer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LfsPointer {
    pub oid_sha256: String,
    pub size: u64,
}

/// Invalid fields in a file that identifies itself as an LFS pointer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LfsPointerError {
    MissingObjectId,
    InvalidObjectId,
    InvalidSize,
    UnexpectedFields,
}

impl std::fmt::Display for LfsPointerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::MissingObjectId => "LFS pointer has no SHA-256 object ID",
            Self::InvalidObjectId => "LFS pointer object ID is invalid",
            Self::InvalidSize => "LFS pointer size is invalid",
            Self::UnexpectedFields => "LFS pointer has unexpected trailing fields",
        })
    }
}

impl std::error::Error for LfsPointerError {}

impl LfsPointer {
    /// Parses a strict three-field pointer; ordinary file content returns `None`.
    /// Extension fields are not accepted. Callers reading files should bound the read size.
    ///
    /// # Errors
    /// Returns the invalid field when a recognized pointer is malformed.
    pub fn parse(text: &str) -> Result<Option<Self>, LfsPointerError> {
        let mut lines = text.lines();
        if lines.next() != Some("version https://git-lfs.github.com/spec/v1") {
            return Ok(None);
        }
        let oid = lines
            .next()
            .and_then(|line| line.strip_prefix("oid sha256:"))
            .ok_or(LfsPointerError::MissingObjectId)?;
        if oid.len() != 64 || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(LfsPointerError::InvalidObjectId);
        }
        let size = lines
            .next()
            .and_then(|line| line.strip_prefix("size "))
            .and_then(|value| value.parse().ok())
            .ok_or(LfsPointerError::InvalidSize)?;
        if lines.next().is_some() {
            return Err(LfsPointerError::UnexpectedFields);
        }
        Ok(Some(Self {
            oid_sha256: oid.to_owned(),
            size,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_files_json, LfsFilesError, LfsPointer, LfsPointerError};

    #[test]
    fn file_list_deduplicates_after_applying_input_count_limit() {
        let raw =
            br#"{"files":[{"name":"z.bin","oid":"ignored"},{"name":"a.bin"},{"name":"z.bin"}]}"#;
        let paths = parse_files_json(raw, 3).unwrap();
        assert_eq!(
            paths
                .iter()
                .map(|path| path.as_path().to_str().unwrap())
                .collect::<Vec<_>>(),
            ["a.bin", "z.bin"]
        );
        assert_eq!(parse_files_json(raw, 2), Err(LfsFilesError::TooManyItems));
        assert!(parse_files_json(br#"{"files":null}"#, 0)
            .unwrap()
            .is_empty());
        assert!(parse_files_json(b"{}", 0).unwrap().is_empty());
    }

    #[test]
    fn file_list_rejects_invalid_schema_and_unsafe_paths() {
        for raw in [
            b"[]".as_slice(),
            br#"{"files":{}}"#,
            br#"{"files":[{}]}"#,
            br#"{"files":[{"name":1}]}"#,
            br#"{"files":[]"#,
        ] {
            assert_eq!(parse_files_json(raw, 10), Err(LfsFilesError::InvalidJson));
        }
        for name in ["../escape", "/absolute", "", "nul\0path"] {
            let raw = serde_json::json!({"files":[{"name":name}]}).to_string();
            assert_eq!(
                parse_files_json(raw.as_bytes(), 10),
                Err(LfsFilesError::InvalidPath)
            );
        }
    }

    #[test]
    fn parses_pointer_and_distinguishes_ordinary_content() {
        let oid = "1".repeat(64);
        let pointer = LfsPointer::parse(&format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 42\n"
        ))
        .unwrap()
        .unwrap();
        assert_eq!(pointer.size, 42);
        assert_eq!(pointer.oid_sha256, oid);
        assert!(LfsPointer::parse("ordinary file\n").unwrap().is_none());
    }

    #[test]
    fn rejects_malformed_recognized_pointers() {
        let header = "version https://git-lfs.github.com/spec/v1\n";
        assert_eq!(
            LfsPointer::parse(header),
            Err(LfsPointerError::MissingObjectId)
        );
        assert_eq!(
            LfsPointer::parse(&format!("{header}oid sha256:no\nsize 1\n")),
            Err(LfsPointerError::InvalidObjectId)
        );
        let prefix = format!("{header}oid sha256:{}\n", "a".repeat(64));
        for size in ["-1", "18446744073709551616", "no"] {
            assert_eq!(
                LfsPointer::parse(&format!("{prefix}size {size}\n")),
                Err(LfsPointerError::InvalidSize)
            );
        }
        assert_eq!(
            LfsPointer::parse(&format!("{prefix}size 42\nextra\n")),
            Err(LfsPointerError::UnexpectedFields)
        );
    }
}
