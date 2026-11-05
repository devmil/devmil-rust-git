//! Discovery and confirmation-bound removal of Git lock files (`*.lock`).
//!
//! Git creates `<file>.lock` exclusively before it rewrites the index, a ref,
//! `packed-refs`, `config` or `HEAD`, and renames or deletes it when done. A
//! second Git process that finds the lock fails with "Unable to create
//! '…lock': File exists". A crashed process leaves the lock behind.
//!
//! Inspection reads metadata only; no Git subprocess runs and no file changes.
//! A fingerprint detects changes between inspection and confirmation. It cannot
//! prove that a Git process has abandoned the lock: callers must warn the user
//! to stop other Git operations before explicitly requesting removal.

use crate::filesystem::{open_common_git_capability, same_cap_metadata};
use crate::types::WorktreeId;
use cap_fs_ext::MetadataExt;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const LOCK_SUFFIX: &str = ".lock";

/// Directories of the common Git directory whose nested files Git locks.
const NESTED_LOCK_ROOTS: [&str; 2] = ["refs", "logs"];

/// Bounds one inspection so a huge loose-ref tree stays cheap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GitLockLimits {
    /// Directory entries visited before the walk stops.
    pub max_entries: usize,
    /// Lock files reported before the walk stops.
    pub max_locks: usize,
}

impl Default for GitLockLimits {
    fn default() -> Self {
        Self {
            max_entries: 200_000,
            max_locks: 64,
        }
    }
}

/// Which Git directory a lock lives in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitLockScope {
    /// The per-worktree Git directory (`index.lock`, `HEAD.lock`).
    Worktree,
    /// The common Git directory shared by all worktrees (refs, `packed-refs`, `config`).
    Common,
}

/// Opaque identity of one lock file shown in a removal confirmation.
#[derive(Clone, Debug)]
pub struct GitLockSnapshot {
    scope: GitLockScope,
    base: PathBuf,
    relative: PathBuf,
    directory_identity: (u64, u64),
    file_identity: (u64, u64),
    metadata: cap_std::fs::Metadata,
}

impl GitLockSnapshot {
    #[must_use]
    pub const fn scope(&self) -> GitLockScope {
        self.scope
    }

    /// The lock's path relative to its Git directory, such as
    /// `refs/remotes/origin/main.lock`.
    #[must_use]
    pub fn relative_path(&self) -> &Path {
        &self.relative
    }

    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.base.join(&self.relative)
    }

    #[must_use]
    pub fn size_bytes(&self) -> u64 {
        self.metadata.len()
    }

    /// When the lock was last written; Git writes it once when it takes the lock.
    #[must_use]
    pub fn modified(&self) -> Option<SystemTime> {
        self.metadata
            .modified()
            .ok()
            .map(cap_std::time::SystemTime::into_std)
    }
}

/// The lock files one inspection found.
#[derive(Clone, Debug, Default)]
pub struct GitLockInspection {
    pub locks: Vec<GitLockSnapshot>,
    /// A limit stopped the walk; more locks may exist.
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitLockError {
    Missing,
    Changed,
    UnsafeFile,
    Io,
}

impl fmt::Display for GitLockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Missing => "The lock file no longer exists.",
            Self::Changed => {
                "The lock file or its Git directory changed. Inspect it again before removal."
            }
            Self::UnsafeFile => "The lock file is not a regular file and cannot be removed here.",
            Self::Io => {
                "The lock file could not be inspected or removed. Check filesystem permissions."
            }
        })
    }
}

impl std::error::Error for GitLockError {}

/// Whether Git failed because another process held one of its lock files.
/// Matches Git's lockfile diagnostic ("Unable to create '….lock': File exists")
/// and ref-lock failures. The patterns assume `LC_ALL=C`.
#[must_use]
pub fn is_lock_conflict(stderr: &[u8]) -> bool {
    crate::diagnosis::shows(crate::diagnosis::DiagnosisKind::LockConflict, stderr)
}

/// Finds the regular `*.lock` files Git may have left in the worktree's Git
/// directory and in the common Git directory's `refs` and `logs` trees,
/// including linked worktrees and custom Git directory locations.
///
/// # Errors
/// Returns a typed error when a Git directory cannot be opened safely.
pub fn inspect_git_locks(
    worktree: &WorktreeId,
    limits: GitLockLimits,
) -> Result<GitLockInspection, GitLockError> {
    let worktree_dir = worktree.git_dir().as_path();
    let common_dir = worktree.repository().common_git_dir().as_path();
    let mut walk = Walk {
        limits,
        visited: 0,
        inspection: GitLockInspection::default(),
    };
    let separate = worktree_dir != common_dir;
    if separate {
        walk.directory(GitLockScope::Worktree, worktree_dir, false)?;
    }
    walk.directory(GitLockScope::Common, common_dir, true)?;
    let mut inspection = walk.inspection;
    inspection
        .locks
        .sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(inspection)
}

/// Removes only the inspected lock file after explicit user confirmation. The
/// file the lock protects is never opened, renamed or deleted.
///
/// # Errors
/// Refuses changed or replaced files, symlinks, missing locks and changed Git
/// directories. Concurrent Git activity must be stopped first: filesystems do
/// not provide an atomic compare-fingerprint-and-unlink operation.
pub fn remove_git_lock(snapshot: &GitLockSnapshot) -> Result<(), GitLockError> {
    if !is_lock_name(&snapshot.relative) {
        return Err(GitLockError::UnsafeFile);
    }
    let directory = open_directory(&snapshot.base)?;
    let directory_metadata = directory.dir_metadata().map_err(|_| GitLockError::Io)?;
    if identity(&directory_metadata) != snapshot.directory_identity {
        return Err(GitLockError::Changed);
    }
    let current = directory
        .symlink_metadata(&snapshot.relative)
        .map_err(|error| not_found_as_missing(&error))?;
    require_regular(&current)?;
    if identity(&current) != snapshot.file_identity
        || !same_cap_metadata(&current, &snapshot.metadata)
    {
        return Err(GitLockError::Changed);
    }
    directory
        .remove_file(&snapshot.relative)
        .map_err(|error| not_found_as_missing(&error))
}

struct Walk {
    limits: GitLockLimits,
    visited: usize,
    inspection: GitLockInspection,
}

impl Walk {
    fn directory(
        &mut self,
        scope: GitLockScope,
        base: &Path,
        nested: bool,
    ) -> Result<(), GitLockError> {
        let directory = open_directory(base)?;
        let directory_identity = identity(&directory.dir_metadata().map_err(|_| GitLockError::Io)?);
        let root = Root {
            scope,
            base,
            directory_identity,
        };
        self.entries(&root, &directory, Path::new(""), nested)
    }

    fn entries(
        &mut self,
        root: &Root<'_>,
        directory: &cap_std::fs::Dir,
        relative: &Path,
        nested: bool,
    ) -> Result<(), GitLockError> {
        let Ok(entries) = directory.entries() else {
            // A directory removed or made unreadable during the walk holds no
            // lock this inspection can offer.
            return if relative.as_os_str().is_empty() {
                Err(GitLockError::Io)
            } else {
                Ok(())
            };
        };
        for entry in entries {
            if self.full() {
                return Ok(());
            }
            self.visited += 1;
            let Ok(entry) = entry else { continue };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            let path = relative.join(&name);
            if file_type.is_dir() {
                let descend = nested
                    && (!relative.as_os_str().is_empty()
                        || NESTED_LOCK_ROOTS.iter().any(|root| name == *root));
                if descend {
                    if let Ok(child) = entry.open_dir() {
                        self.entries(root, &child, &path, nested)?;
                    }
                }
            } else if file_type.is_file() && is_lock_name(&path) {
                let Ok(metadata) = directory.symlink_metadata(&name) else {
                    continue;
                };
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    continue;
                }
                self.inspection.locks.push(GitLockSnapshot {
                    scope: root.scope,
                    base: root.base.to_path_buf(),
                    relative: path,
                    directory_identity: root.directory_identity,
                    file_identity: identity(&metadata),
                    metadata,
                });
            }
        }
        Ok(())
    }

    fn full(&mut self) -> bool {
        if self.visited >= self.limits.max_entries
            || self.inspection.locks.len() >= self.limits.max_locks
        {
            self.inspection.truncated = true;
            return true;
        }
        false
    }
}

struct Root<'a> {
    scope: GitLockScope,
    base: &'a Path,
    directory_identity: (u64, u64),
}

fn is_lock_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.len() > LOCK_SUFFIX.len() && name.ends_with(LOCK_SUFFIX))
        && path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn open_directory(path: &Path) -> Result<cap_std::fs::Dir, GitLockError> {
    open_common_git_capability(path, || {}).map_err(|_| GitLockError::Io)
}

fn not_found_as_missing(error: &std::io::Error) -> GitLockError {
    if error.kind() == std::io::ErrorKind::NotFound {
        GitLockError::Missing
    } else {
        GitLockError::Io
    }
}

fn require_regular(metadata: &cap_std::fs::Metadata) -> Result<(), GitLockError> {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(GitLockError::UnsafeFile);
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
        common: PathBuf,
        worktree: WorktreeId,
    }

    impl Fixture {
        fn new(linked: bool) -> Self {
            let root = std::env::temp_dir().join(format!(
                "devmil-git-locks-{}-{}",
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
            fs::create_dir_all(common.join("refs/remotes/origin/feature")).unwrap();
            fs::create_dir_all(common.join("objects/pack")).unwrap();
            let worktree_root = root.join("worktree");
            fs::create_dir_all(&worktree_root).unwrap();
            let worktree = WorktreeId::new(
                RepositoryId::new(RepositoryPath::new(common.clone()).unwrap()),
                RepositoryPath::new(worktree_root).unwrap(),
                RepositoryPath::new(git).unwrap(),
            );
            Self {
                root,
                common,
                worktree,
            }
        }

        fn git_dir(&self) -> &Path {
            self.worktree.git_dir().as_path()
        }

        fn names(inspection: &GitLockInspection) -> Vec<String> {
            inspection
                .locks
                .iter()
                .map(|lock| lock.relative_path().to_string_lossy().replace('\\', "/"))
                .collect()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn lock_conflicts_are_recognised_in_git_diagnostics() {
        let fetch = b"error: could not delete references: cannot lock ref \
'refs/remotes/origin/a': Unable to create '/r/.git/refs/remotes/origin/a.lock': File exists.\n\n\
Another git process seems to be running in this repository";
        assert!(is_lock_conflict(fetch));
        assert!(is_lock_conflict(
            b"fatal: Unable to create '/r/.git/index.lock': File exists."
        ));
        assert!(!is_lock_conflict(b"fatal: Authentication failed"));
        assert!(!is_lock_conflict(
            b"fatal: Unable to create '/r/.git/index.lock': Permission denied"
        ));
    }

    #[test]
    fn inspection_finds_index_ref_and_packed_ref_locks_but_ignores_other_files() {
        for linked in [false, true] {
            let fixture = Fixture::new(linked);
            fs::write(fixture.git_dir().join("index.lock"), b"i").unwrap();
            fs::write(fixture.git_dir().join("index"), b"index").unwrap();
            fs::write(fixture.common.join("packed-refs.lock"), b"p").unwrap();
            fs::write(
                fixture.common.join("refs/remotes/origin/feature/x.lock"),
                b"ref",
            )
            .unwrap();
            fs::write(fixture.common.join("refs/remotes/origin/main"), b"oid").unwrap();
            // Pack files and other trees are never walked.
            fs::write(fixture.common.join("objects/pack/tmp.lock"), b"").unwrap();
            let inspection =
                inspect_git_locks(&fixture.worktree, GitLockLimits::default()).unwrap();
            assert!(!inspection.truncated);
            let mut names = Fixture::names(&inspection);
            names.sort();
            assert_eq!(
                names,
                [
                    "index.lock",
                    "packed-refs.lock",
                    "refs/remotes/origin/feature/x.lock"
                ]
            );
            let index = inspection
                .locks
                .iter()
                .find(|lock| lock.relative_path() == Path::new("index.lock"))
                .unwrap();
            assert_eq!(
                index.scope(),
                if linked {
                    GitLockScope::Worktree
                } else {
                    GitLockScope::Common
                }
            );
            assert!(index.modified().is_some());
            assert_eq!(index.size_bytes(), 1);
            assert!(fixture.git_dir().join("index.lock").exists());
        }
    }

    #[test]
    fn limits_truncate_the_walk() {
        let fixture = Fixture::new(false);
        for index in 0..5 {
            fs::write(
                fixture
                    .common
                    .join(format!("refs/remotes/origin/b{index}.lock")),
                b"",
            )
            .unwrap();
        }
        let inspection = inspect_git_locks(
            &fixture.worktree,
            GitLockLimits {
                max_entries: 1_000,
                max_locks: 2,
            },
        )
        .unwrap();
        assert!(inspection.truncated);
        assert_eq!(inspection.locks.len(), 2);
    }

    #[test]
    fn nested_lock_is_removed_and_the_ref_kept() {
        let fixture = Fixture::new(false);
        let reference = fixture.common.join("refs/remotes/origin/feature/x");
        fs::write(&reference, b"oid").unwrap();
        fs::write(reference.with_extension("lock"), b"stale").unwrap();
        let inspection = inspect_git_locks(&fixture.worktree, GitLockLimits::default()).unwrap();
        let [lock] = inspection.locks.as_slice() else {
            panic!("one lock expected");
        };
        remove_git_lock(lock).unwrap();
        assert!(!reference.with_extension("lock").exists());
        assert_eq!(fs::read(reference).unwrap(), b"oid");
        assert_eq!(remove_git_lock(lock), Err(GitLockError::Missing));
    }

    #[test]
    fn changed_locks_are_preserved() {
        let fixture = Fixture::new(false);
        let lock = fixture.git_dir().join("HEAD.lock");
        fs::write(&lock, b"first").unwrap();
        let inspection = inspect_git_locks(&fixture.worktree, GitLockLimits::default()).unwrap();
        fs::write(&lock, b"changed lock").unwrap();
        assert_eq!(
            remove_git_lock(&inspection.locks[0]),
            Err(GitLockError::Changed)
        );
        assert!(lock.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_locks_and_directories_are_never_followed() {
        let fixture = Fixture::new(false);
        let outside = fixture.root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("victim.lock"), b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, fixture.common.join("refs/linked")).unwrap();
        std::os::unix::fs::symlink(
            outside.join("victim.lock"),
            fixture.common.join("config.lock"),
        )
        .unwrap();
        let inspection = inspect_git_locks(&fixture.worktree, GitLockLimits::default()).unwrap();
        assert!(inspection.locks.is_empty());
        assert_eq!(fs::read(outside.join("victim.lock")).unwrap(), b"keep");
    }
}
