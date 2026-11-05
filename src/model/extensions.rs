//! Cached repository extension reads.
use super::{ReadTicket, Repository, RepositoryError, RepositoryErrorKind};
use crate::loading::extensions::{self, ExtensionLoader, RepositoryExtensionErrorKind as Kind};
use crate::types::{RepositoryRelativePath, WorktreeId};

fn failure(error: &extensions::RepositoryExtensionError) -> RepositoryError {
    let kind = match error.kind() {
        Kind::InvalidRequest | Kind::UnsafeTarget => RepositoryErrorKind::InvalidOptions,
        Kind::Canceled => RepositoryErrorKind::Canceled,
        Kind::InvalidOutput => RepositoryErrorKind::InvalidOutput,
        Kind::TooLarge => RepositoryErrorKind::TooLarge,
        Kind::Unavailable => RepositoryErrorKind::MissingGit,
        Kind::UnsupportedRemote => RepositoryErrorKind::Unsupported,
        Kind::GitFailed => RepositoryErrorKind::Read,
    };
    RepositoryError::new(kind, error.to_string())
}
fn path_weight(path: &std::path::Path) -> usize {
    path.as_os_str().as_encoded_bytes().len().saturating_mul(2)
}
fn identity_weight(worktree: &WorktreeId) -> usize {
    path_weight(worktree.worktree_root().as_path())
        .saturating_add(path_weight(worktree.git_dir().as_path()))
        .saturating_add(path_weight(
            worktree.repository().common_git_dir().as_path(),
        ))
}
fn key(prefix: &[u8], path: &RepositoryRelativePath) -> Vec<u8> {
    let bytes = path.as_path().as_os_str().as_encoded_bytes();
    let mut key = prefix.to_vec();
    key.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    key.extend_from_slice(bytes);
    key
}
impl Repository {
    /// Loads and caches linked-worktree identities until the next refresh.
    /// # Errors
    /// Returns repository lifecycle or scheduling failures.
    pub fn worktrees(&self) -> Result<ReadTicket<extensions::WorktreeInventory>, RepositoryError> {
        let executable = self.shared.options.git_executable.clone();
        let shared = super::Arc::clone(&self.shared);
        self.schedule_weighted(
            b"worktree-inventory-v1".to_vec(),
            false,
            |result: &extensions::WorktreeInventory| {
                std::mem::size_of_val(result)
                    .saturating_add(
                        result
                            .worktrees
                            .capacity()
                            .saturating_mul(std::mem::size_of::<extensions::ManagedWorktree>()),
                    )
                    .saturating_add(super::pages::activities(&result.activities))
                    .saturating_add(result.worktrees.iter().fold(0usize, |sum, row| {
                        sum.saturating_add(identity_weight(&row.identity))
                            .saturating_add(256)
                    }))
            },
            move |repository, executor, cancellation| {
                let snapshot = super::Arc::clone(&super::lock(&shared.state).snapshot);
                if snapshot.refs_failure.is_none() {
                    if let Some(rows) = snapshot.refs.iter().find_map(|section| match section {
                        crate::loading::refs::RefsSectionData::Worktrees(rows) => Some(rows),
                        _ => None,
                    }) {
                        if rows.len() > extensions::MAX_WORKTREES {
                            return Err(RepositoryError::new(
                                RepositoryErrorKind::TooLarge,
                                "worktree inventory exceeds its bound",
                            ));
                        }
                        return Ok(extensions::WorktreeInventory {
                            worktrees: rows
                                .iter()
                                .map(|row| extensions::ManagedWorktree {
                                    identity: row.id.clone(),
                                    head: row.head.clone(),
                                    branch: row.branch.clone(),
                                    locked: row.locked_reason.is_some(),
                                    prunable: row.prunable_reason.is_some(),
                                })
                                .collect(),
                            activities: snapshot
                                .refs_activities
                                .iter()
                                .filter(|record| {
                                    record
                                        .command
                                        .arguments
                                        .first()
                                        .is_some_and(|argument| argument.display == "worktree")
                                })
                                .cloned()
                                .collect(),
                        });
                    }
                }
                ExtensionLoader::new(executable, executor)
                    .worktrees(&repository.worktree, cancellation)
                    .map_err(|error| failure(&error))
            },
        )
    }
    /// Loads recursive submodule checkout state.
    /// # Errors
    /// Returns repository lifecycle or scheduling failures.
    pub fn submodules(
        &self,
    ) -> Result<ReadTicket<extensions::SubmoduleStatusResult>, RepositoryError> {
        let executable = self.shared.options.git_executable.clone();
        self.schedule_weighted(
            b"submodules-v1".to_vec(),
            false,
            |result: &extensions::SubmoduleStatusResult| {
                std::mem::size_of_val(result)
                    .saturating_add(
                        result
                            .submodules
                            .capacity()
                            .saturating_mul(std::mem::size_of::<extensions::SubmoduleStatus>()),
                    )
                    .saturating_add(super::pages::activities(&result.activities))
                    .saturating_add(result.submodules.iter().fold(0usize, |sum, row| {
                        sum.saturating_add(path_weight(row.path.as_path()))
                            .saturating_add(128)
                    }))
            },
            move |repository, executor, cancellation| {
                ExtensionLoader::new(executable, executor)
                    .submodules(&repository.worktree, cancellation)
                    .map_err(|error| failure(&error))
            },
        )
    }
    /// Loads Git LFS availability and working-tree materialization state.
    /// # Errors
    /// Returns repository lifecycle or scheduling failures.
    pub fn lfs_status(&self) -> Result<ReadTicket<extensions::LfsStatusResult>, RepositoryError> {
        let executable = self.shared.options.git_executable.clone();
        self.schedule_weighted(
            b"lfs-status-v1".to_vec(),
            false,
            |result: &extensions::LfsStatusResult| {
                std::mem::size_of_val(result)
                    .saturating_add(
                        result
                            .entries
                            .capacity()
                            .saturating_mul(std::mem::size_of::<extensions::LfsStatusEntry>()),
                    )
                    .saturating_add(super::pages::activities(&result.activities))
                    .saturating_add(result.unavailable_reason.capacity())
                    .saturating_add(result.entries.iter().fold(0usize, |sum, row| {
                        sum.saturating_add(path_weight(row.path.as_path()))
                            .saturating_add(
                                row.pointer.as_ref().map_or(0, |p| p.oid_sha256.capacity()),
                            )
                    }))
            },
            move |repository, executor, cancellation| {
                ExtensionLoader::new(executable, executor)
                    .lfs_status(&repository.worktree, cancellation)
                    .map_err(|error| failure(&error))
            },
        )
    }
    /// Resolves an initialized submodule's native repository identity.
    /// # Errors
    /// Returns repository lifecycle or scheduling failures.
    pub fn enter_submodule(
        &self,
        path: RepositoryRelativePath,
    ) -> Result<ReadTicket<WorktreeId>, RepositoryError> {
        let executable = self.shared.options.git_executable.clone();
        self.schedule_weighted(
            key(b"submodule-identity-v1", &path),
            false,
            |identity| std::mem::size_of_val(identity).saturating_add(identity_weight(identity)),
            move |repository, executor, cancellation| {
                ExtensionLoader::new(executable, executor)
                    .enter_submodule(&repository.worktree, &path, cancellation)
                    .map_err(|error| failure(&error))
            },
        )
    }
    /// Reads a bounded LFS pointer preview without invoking Git.
    /// # Errors
    /// Returns repository lifecycle or scheduling failures.
    pub fn lfs_pointer_preview(
        &self,
        path: RepositoryRelativePath,
    ) -> Result<ReadTicket<Option<extensions::LfsPointerPreview>>, RepositoryError> {
        let executable = self.shared.options.git_executable.clone();
        self.schedule_weighted(
            key(b"lfs-pointer-v1", &path),
            false,
            |preview: &Option<extensions::LfsPointerPreview>| {
                std::mem::size_of_val(preview)
                    .saturating_add(preview.as_ref().map_or(0, |p| p.oid_sha256.capacity()))
            },
            move |repository, executor, _| {
                ExtensionLoader::new(executable, executor)
                    .lfs_pointer_preview(&repository.worktree, &path)
                    .map_err(|error| failure(&error))
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RepositoryOptions;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };
    use std::time::Duration;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    #[test]
    fn worktree_inventory_reuses_discovery_and_cached_reads() {
        let root = std::env::temp_dir().join(format!(
            "devmil-extension-model-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let output = std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(output.status.success());
        let repository = Repository::new(
            &root,
            RepositoryOptions {
                watch: false,
                ..RepositoryOptions::default()
            },
        )
        .unwrap();
        let first = repository
            .worktrees()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert_eq!(first.worktrees.len(), 1);
        // The reference snapshot already resolved inventory and native administration paths.
        assert_eq!(first.activities.len(), 1);
        let snapshot = repository.snapshot();
        let original = snapshot
            .refs_activities
            .iter()
            .find(|record| {
                record
                    .command
                    .arguments
                    .first()
                    .is_some_and(|argument| argument.display == "worktree")
            })
            .unwrap();
        assert_eq!(first.activities[0].id, original.id);
        let second = repository
            .worktrees()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        repository.close();
        std::fs::remove_dir_all(root).unwrap();
    }
}
