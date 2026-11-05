//! Byte-safe Git identities and native repository paths. No filesystem access.

use std::fmt;
use std::path::{Component, Path, PathBuf};

/// A native path that identifies a repository location.
///
/// The infrastructure layer must supply its chosen canonical representation. This type deliberately
/// performs no filesystem access, so equality and ordering compare the supplied native path exactly.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepositoryPath(PathBuf);

impl RepositoryPath {
    /// Creates a non-empty repository path without inspecting the filesystem.
    ///
    /// # Errors
    ///
    /// Returns [`GitValueError::EmptyRepositoryPath`] when the supplied path is empty.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, GitValueError> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err(GitValueError::EmptyRepositoryPath);
        }
        Ok(Self(path))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

/// A byte-safe path relative to a repository worktree.
///
/// The path cannot be empty, absolute, or contain `.` or `..` components. Rejecting lexical aliases
/// makes value equality suitable for durable file selection. [`PathBuf`] retains non-UTF-8 bytes on
/// Unix and the native wide-string representation on Windows.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepositoryRelativePath(PathBuf);

impl RepositoryRelativePath {
    /// Validates a repository-relative native path without inspecting the filesystem.
    ///
    /// # Errors
    ///
    /// Returns [`GitValueError::InvalidRepositoryRelativePath`] for an empty or non-relative path, or
    /// for a path containing a current-directory or parent-directory component.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, GitValueError> {
        let path = path.into();
        if path.as_os_str().is_empty()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::Prefix(_)
                        | Component::RootDir
                        | Component::CurDir
                        | Component::ParentDir
                )
            })
        {
            return Err(GitValueError::InvalidRepositoryRelativePath(path));
        }
        Ok(Self(path))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

/// The object format determines the exact length of a full Git object ID.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}

impl ObjectFormat {
    pub const fn hexadecimal_length(self) -> usize {
        match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
        }
    }
}

/// A complete Git object ID suitable for durable identity.
///
/// Abbreviated IDs are presentation values and are rejected here, even when Git could currently
/// resolve them unambiguously. Equality uses the normalized full value. Ordering is lexicographic by
/// that value, which is deterministic and carries no graph or chronology meaning.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ObjectId {
    value: String,
    format: ObjectFormat,
}

impl ObjectId {
    /// Parses a full SHA-1 or SHA-256 hexadecimal Git object ID.
    ///
    /// ASCII uppercase digits are accepted and normalized to lowercase. No repository lookup occurs.
    ///
    /// # Errors
    ///
    /// Returns [`GitValueError::InvalidObjectId`] for non-hexadecimal values and every length other than
    /// 40 or 64 characters.
    pub fn parse(value: impl Into<String>) -> Result<Self, GitValueError> {
        let value = value.into();
        let format = match value.len() {
            40 => ObjectFormat::Sha1,
            64 => ObjectFormat::Sha256,
            _ => return Err(GitValueError::InvalidObjectId(value)),
        };
        if !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(GitValueError::InvalidObjectId(value));
        }
        Ok(Self {
            value: value.to_ascii_lowercase(),
            format,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }

    pub const fn format(&self) -> ObjectFormat {
        self.format
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.value)
    }
}

/// A complete ref name below `refs/`, stored as Git bytes rather than display text. Equality and
/// ordering use those bytes exactly.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RefId(Vec<u8>);

impl RefId {
    /// Parses a complete Git ref name such as `refs/heads/main`.
    ///
    /// This applies the ref-name invariants that do not require invoking Git. Pseudo refs such as
    /// `HEAD` are intentionally excluded because they are state, not durable ref identities.
    ///
    /// # Errors
    ///
    /// Returns [`GitValueError::InvalidRefId`] when `value` is not a valid complete ref name.
    pub fn from_bytes(value: impl Into<Vec<u8>>) -> Result<Self, GitValueError> {
        let value = value.into();
        if !is_valid_ref_name(&value) {
            return Err(GitValueError::InvalidRefId(value));
        }
        Ok(Self(value))
    }

    /// Parses a UTF-8 complete ref name.
    ///
    /// # Errors
    ///
    /// Returns [`GitValueError::InvalidRefId`] under the same conditions as [`Self::from_bytes`].
    pub fn parse(value: impl Into<String>) -> Result<Self, GitValueError> {
        Self::from_bytes(value.into().into_bytes())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Display for RefId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", String::from_utf8_lossy(&self.0))
    }
}

fn is_valid_ref_name(value: &[u8]) -> bool {
    if !value.starts_with(b"refs/") || value.ends_with(b"/") || value.ends_with(b".") {
        return false;
    }
    if value
        .iter()
        .any(|byte| *byte < b' ' || *byte == 0x7f || b" ~^:?*[\\".contains(byte))
        || value
            .windows(2)
            .any(|pair| pair == b".." || pair == b"@{" || pair == b"//")
    {
        return false;
    }
    value.split(|byte| *byte == b'/').all(|component| {
        !component.is_empty() && !component.starts_with(b".") && !component.ends_with(b".lock")
    })
}

/// Identity shared by the main worktree and every linked worktree of one Git repository.
///
/// The common Git directory must be supplied in the infrastructure layer's canonical native path
/// representation.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepositoryId {
    common_git_dir: RepositoryPath,
}

impl RepositoryId {
    pub fn new(common_git_dir: RepositoryPath) -> Self {
        Self { common_git_dir }
    }

    pub fn common_git_dir(&self) -> &RepositoryPath {
        &self.common_git_dir
    }
}

/// Durable identity for one worktree within a repository.
///
/// Linked worktrees share a [`RepositoryId`] but differ by worktree root or per-worktree Git
/// directory. Neither path is discovered or canonicalized by this constructor.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorktreeId {
    repository: RepositoryId,
    worktree_root: RepositoryPath,
    git_dir: RepositoryPath,
}

impl WorktreeId {
    pub fn new(
        repository: RepositoryId,
        worktree_root: RepositoryPath,
        git_dir: RepositoryPath,
    ) -> Self {
        Self {
            repository,
            worktree_root,
            git_dir,
        }
    }

    pub fn repository(&self) -> &RepositoryId {
        &self.repository
    }

    pub fn worktree_root(&self) -> &RepositoryPath {
        &self.worktree_root
    }

    pub fn git_dir(&self) -> &RepositoryPath {
        &self.git_dir
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitValueError {
    EmptyRepositoryPath,
    InvalidRepositoryRelativePath(PathBuf),
    InvalidObjectId(String),
    InvalidRefId(Vec<u8>),
}

impl fmt::Display for GitValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRepositoryPath => formatter.write_str("repository path cannot be empty"),
            Self::InvalidRepositoryRelativePath(path) => write!(
                formatter,
                "invalid repository-relative path: {}",
                path.display()
            ),
            Self::InvalidObjectId(value) => write!(formatter, "invalid Git object ID: {value}"),
            Self::InvalidRefId(value) => write!(formatter, "invalid Git ref ID: {value:?}"),
        }
    }
}
impl std::error::Error for GitValueError {}

/// Current meaning of `HEAD`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum HeadState {
    Attached { branch: RefId, target: ObjectId },
    Detached { target: ObjectId },
    Unborn { branch: RefId },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_ids_normalize_full_hashes_and_reject_abbreviated_or_nonhex_values() {
        for (length, format) in [(40, ObjectFormat::Sha1), (64, ObjectFormat::Sha256)] {
            let object = ObjectId::parse("A".repeat(length)).unwrap();
            assert_eq!(object.format(), format);
            assert_eq!(object.as_str(), "a".repeat(length));
        }
        for value in ["a".repeat(7), "a".repeat(41), "z".repeat(40)] {
            assert!(ObjectId::parse(value).is_err());
        }
    }

    #[test]
    fn refs_preserve_raw_bytes_and_reject_git_forbidden_names() {
        let reference = RefId::from_bytes(b"refs/heads/name-\xff".to_vec()).unwrap();
        assert_eq!(reference.as_bytes(), b"refs/heads/name-\xff");
        for value in [
            "HEAD",
            "refs/heads/a..b",
            "refs/heads/name.lock",
            "refs/heads/a b",
            "refs/heads/a@{b",
        ] {
            assert!(
                RefId::from_bytes(value.as_bytes().to_vec()).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn repository_identity_is_shared_while_worktree_identity_is_distinct() {
        let repository = RepositoryId::new(RepositoryPath::new("common.git").unwrap());
        let first = WorktreeId::new(
            repository.clone(),
            RepositoryPath::new("first").unwrap(),
            RepositoryPath::new("common.git").unwrap(),
        );
        let second = WorktreeId::new(
            repository,
            RepositoryPath::new("second").unwrap(),
            RepositoryPath::new("common.git/worktrees/second").unwrap(),
        );
        assert_eq!(first.repository(), second.repository());
        assert_ne!(first, second);
        for value in ["", "../escape", "nested/../escape", "./name", "/absolute"] {
            assert!(RepositoryRelativePath::new(value).is_err(), "{value}");
        }
    }
}
