//! Explicit, confirmation-bound recovery of a repository's Git index lock.
//!
//! A fingerprint detects changes between inspection and confirmation. It cannot
//! prove that a Git process has abandoned the lock. The caller must warn the user
//! to stop other Git operations before explicitly requesting removal.

use crate::filesystem::{open_common_git_capability, same_cap_metadata};
use crate::types::WorktreeId;
use cap_fs_ext::MetadataExt;
use std::fmt;
use std::path::{Path, PathBuf};

const LOCK_NAME: &str = "index.lock";

/// Opaque identity of the regular file shown in a removal confirmation.
#[derive(Clone, Debug)]
pub struct IndexLockSnapshot {
    git_dir: PathBuf,
    directory_identity: (u64, u64),
    file_identity: (u64, u64),
    metadata: cap_std::fs::Metadata,
}

impl IndexLockSnapshot {
    pub fn path(&self) -> PathBuf {
        self.git_dir.join(LOCK_NAME)
    }

    pub fn size_bytes(&self) -> u64 {
        self.metadata.len()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexLockError {
    Missing,
    Changed,
    UnsafeFile,
    Io,
}

impl fmt::Display for IndexLockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Missing => "The index lock no longer exists. Refresh repository status.",
            Self::Changed => {
                "The index lock or Git directory changed. Inspect it again before removal."
            }
            Self::UnsafeFile => "The index lock is not a regular file and cannot be removed here.",
            Self::Io => {
                "The index lock could not be inspected or removed. Check filesystem permissions."
            }
        })
    }
}

impl std::error::Error for IndexLockError {}

/// Inspect the per-worktree Git directory, including linked worktrees and custom
/// Git directory locations. No Git subprocess runs and no file is changed.
///
/// # Errors
/// Returns a typed error for unsafe files, unstable directories, or I/O failures.
pub fn inspect_index_lock(
    worktree: &WorktreeId,
) -> Result<Option<IndexLockSnapshot>, IndexLockError> {
    let git_dir = worktree.git_dir().as_path();
    let directory = open_directory(git_dir)?;
    let metadata = match directory.symlink_metadata(LOCK_NAME) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(IndexLockError::Io),
    };
    require_regular(&metadata)?;
    let directory_metadata = directory.dir_metadata().map_err(|_| IndexLockError::Io)?;
    Ok(Some(IndexLockSnapshot {
        git_dir: git_dir.to_path_buf(),
        directory_identity: identity(&directory_metadata),
        file_identity: identity(&metadata),
        metadata,
    }))
}

/// Remove only the inspected `index.lock` after explicit user confirmation.
/// The actual index is never opened, renamed, or deleted.
///
/// # Errors
/// Refuses changed/replaced files, symlinks, missing locks, or changed Git
/// directories. Concurrent Git activity must be stopped first: filesystems do
/// not provide an atomic compare-fingerprint-and-unlink operation.
pub fn remove_index_lock(snapshot: &IndexLockSnapshot) -> Result<(), IndexLockError> {
    let directory = open_directory(&snapshot.git_dir)?;
    let directory_metadata = directory.dir_metadata().map_err(|_| IndexLockError::Io)?;
    if identity(&directory_metadata) != snapshot.directory_identity {
        return Err(IndexLockError::Changed);
    }
    let current = directory.symlink_metadata(LOCK_NAME).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            IndexLockError::Missing
        } else {
            IndexLockError::Io
        }
    })?;
    require_regular(&current)?;
    if identity(&current) != snapshot.file_identity
        || !same_cap_metadata(&current, &snapshot.metadata)
    {
        return Err(IndexLockError::Changed);
    }
    directory.remove_file(LOCK_NAME).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            IndexLockError::Missing
        } else {
            IndexLockError::Io
        }
    })
}

fn open_directory(path: &Path) -> Result<cap_std::fs::Dir, IndexLockError> {
    open_common_git_capability(path, || {}).map_err(|_| IndexLockError::Io)
}

fn require_regular(metadata: &cap_std::fs::Metadata) -> Result<(), IndexLockError> {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(IndexLockError::UnsafeFile);
    }
    Ok(())
}

fn identity(metadata: &cap_std::fs::Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RepositoryId, RepositoryPath};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        worktree: WorktreeId,
    }

    impl Fixture {
        fn new(linked: bool) -> Self {
            let root = std::env::temp_dir().join(format!(
                "devmil-git-index-lock-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            let common = root.join("custom-git-dir");
            let git = if linked {
                common.join("worktrees/linked")
            } else {
                common.clone()
            };
            fs::create_dir_all(&git).unwrap();
            let worktree_root = root.join("worktree");
            fs::create_dir_all(&worktree_root).unwrap();
            let worktree = WorktreeId::new(
                RepositoryId::new(RepositoryPath::new(common).unwrap()),
                RepositoryPath::new(worktree_root).unwrap(),
                RepositoryPath::new(git).unwrap(),
            );
            Self { root, worktree }
        }

        fn lock(&self) -> PathBuf {
            self.worktree.git_dir().as_path().join(LOCK_NAME)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn unchanged_lock_is_removed_and_index_retained_in_custom_and_linked_git_dirs() {
        for linked in [false, true] {
            let fixture = Fixture::new(linked);
            let index = fixture.worktree.git_dir().as_path().join("index");
            fs::write(&index, b"index must remain").unwrap();
            fs::write(fixture.lock(), b"stale lock").unwrap();
            let snapshot = inspect_index_lock(&fixture.worktree).unwrap().unwrap();
            assert_eq!(snapshot.path(), fixture.lock());
            assert_eq!(snapshot.size_bytes(), 10);
            assert!(
                fixture.lock().exists(),
                "inspection must not remove the lock"
            );
            remove_index_lock(&snapshot).unwrap();
            assert!(!fixture.lock().exists());
            assert_eq!(fs::read(index).unwrap(), b"index must remain");
        }
    }

    #[test]
    fn missing_lock_is_an_empty_inspection_and_a_typed_removal_error() {
        let fixture = Fixture::new(false);
        assert!(inspect_index_lock(&fixture.worktree).unwrap().is_none());
        fs::write(fixture.lock(), b"lock").unwrap();
        let snapshot = inspect_index_lock(&fixture.worktree).unwrap().unwrap();
        fs::remove_file(fixture.lock()).unwrap();
        assert_eq!(remove_index_lock(&snapshot), Err(IndexLockError::Missing));
    }

    #[test]
    fn changed_and_replaced_locks_are_preserved() {
        for replace in [false, true] {
            let fixture = Fixture::new(false);
            fs::write(fixture.lock(), b"first").unwrap();
            let snapshot = inspect_index_lock(&fixture.worktree).unwrap().unwrap();
            if replace {
                // Keep the former inode allocated, avoiding filesystem inode reuse.
                fs::rename(fixture.lock(), fixture.root.join("old-lock")).unwrap();
            }
            fs::write(
                fixture.lock(),
                if replace {
                    b"first".as_slice()
                } else {
                    b"changed lock"
                },
            )
            .unwrap();
            assert_eq!(remove_index_lock(&snapshot), Err(IndexLockError::Changed));
            assert!(fixture.lock().exists());
        }
    }

    #[test]
    fn timestamp_changes_and_replaced_git_directories_invalidate_confirmation() {
        let fixture = Fixture::new(false);
        fs::write(fixture.lock(), b"same-size").unwrap();
        let snapshot = inspect_index_lock(&fixture.worktree).unwrap().unwrap();
        fs::File::options()
            .write(true)
            .open(fixture.lock())
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();
        assert_eq!(remove_index_lock(&snapshot), Err(IndexLockError::Changed));
        let snapshot = inspect_index_lock(&fixture.worktree).unwrap().unwrap();
        let git_dir = fixture.worktree.git_dir().as_path();
        fs::rename(git_dir, fixture.root.join("old-git-dir")).unwrap();
        fs::create_dir(git_dir).unwrap();
        fs::write(fixture.lock(), b"replacement").unwrap();
        assert_eq!(remove_index_lock(&snapshot), Err(IndexLockError::Changed));
        assert_eq!(fs::read(fixture.lock()).unwrap(), b"replacement");
    }

    #[test]
    fn nonregular_lock_is_never_removed() {
        let fixture = Fixture::new(false);
        fs::create_dir(fixture.lock()).unwrap();
        assert!(matches!(
            inspect_index_lock(&fixture.worktree),
            Err(IndexLockError::UnsafeFile)
        ));
        assert!(fixture.lock().is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_inspection_and_replacement_are_rejected_without_touching_target() {
        let fixture = Fixture::new(false);
        let index = fixture.worktree.git_dir().as_path().join("index");
        fs::write(&index, b"index must remain").unwrap();
        fs::write(fixture.lock(), b"first").unwrap();
        let snapshot = inspect_index_lock(&fixture.worktree).unwrap().unwrap();
        fs::remove_file(fixture.lock()).unwrap();
        std::os::unix::fs::symlink(&index, fixture.lock()).unwrap();
        assert!(matches!(
            inspect_index_lock(&fixture.worktree),
            Err(IndexLockError::UnsafeFile)
        ));
        assert_eq!(
            remove_index_lock(&snapshot),
            Err(IndexLockError::UnsafeFile)
        );
        assert_eq!(fs::read(index).unwrap(), b"index must remain");
        assert!(fs::symlink_metadata(fixture.lock())
            .unwrap()
            .file_type()
            .is_symlink());
    }
}
