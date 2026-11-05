//! Exact, non-forced removal of registered linked worktrees.
use crate::activity::ActivityStatus;
use crate::cancellation::Cancellation;
use crate::loading::CommandExecutor;
use crate::process::{GitProcessCommand, ProcessArgument};
use crate::refs::parse_worktrees;
use crate::types::WorktreeId;
use std::ffi::{OsStr, OsString};
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoveError {
    InvalidTarget,
    Stale,
    Protected,
    Locked,
    Dirty,
    Canceled,
    Read,
    Git,
}

pub use crate::filesystem::same_worktree_path;

fn read(
    executable: &OsStr,
    directory: &Path,
    args: &[&str],
    executor: &mut dyn CommandExecutor,
    cancellation: &dyn Cancellation,
) -> Result<Vec<u8>, RemoveError> {
    if cancellation.is_cancelled() {
        return Err(RemoveError::Canceled);
    }
    let command = GitProcessCommand::git(executable.to_owned())
        .current_dir(directory)
        .args(args.iter().copied().map(ProcessArgument::visible))
        .output_limit(8 * 1024 * 1024)
        .redact_output();
    let mut result = executor.execute(command);
    match result.activity.status {
        ActivityStatus::Succeeded { .. }
            if !result.stdout.truncated && !result.stderr.truncated =>
        {
            Ok(result.stdout.take_bytes())
        }
        ActivityStatus::Canceled { .. } => Err(RemoveError::Canceled),
        _ => Err(RemoveError::Git),
    }
}

/// Rechecks live registration and filesystem facts before producing `worktree remove` argv.
/// Missing directories are removable registrations. Existing directories require their Git
/// marker and a clean status. Locked, main and current worktrees remain protected. No force
/// or repository-wide prune is used, so Git repeats its checks at execution time.
/// Call under the repository's mutation lease and execute the returned arguments immediately.
///
/// # Errors
/// Returns a typed refusal for unregistered, replaced, dirty or protected targets and read failures.
pub fn remove_arguments(
    executable: &OsStr,
    current: &WorktreeId,
    target: &Path,
    executor: &mut dyn CommandExecutor,
    cancellation: &dyn Cancellation,
) -> Result<Vec<OsString>, RemoveError> {
    if std::fs::symlink_metadata(target).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(RemoveError::Stale);
    }
    if !target.is_absolute() {
        return Err(RemoveError::InvalidTarget);
    }
    let raw = read(
        executable,
        current.worktree_root().as_path(),
        &["worktree", "list", "--porcelain", "-z"],
        executor,
        cancellation,
    )?;
    let rows = parse_worktrees(&raw, current).map_err(|_| RemoveError::Read)?;
    if rows.len() > crate::loading::extensions::MAX_WORKTREES {
        return Err(RemoveError::Read);
    }
    let entry = rows
        .iter()
        .find(|row| same_worktree_path(row.id.worktree_root().as_path(), target))
        .ok_or(RemoveError::Stale)?;
    if entry.id == *current
        || entry.id.git_dir() == current.repository().common_git_dir()
        || same_worktree_path(target, current.worktree_root().as_path())
    {
        return Err(RemoveError::Protected);
    }
    if entry.locked_reason.is_some() {
        return Err(RemoveError::Locked);
    }
    let root = entry.id.worktree_root().as_path();
    match std::fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(RemoveError::Read),
        Ok(metadata) => {
            if !metadata.is_dir() || !root.join(".git").is_file() {
                return Err(RemoveError::Stale);
            }
            let status = read(
                executable,
                root,
                &["status", "--porcelain=v1", "--untracked-files=normal"],
                executor,
                cancellation,
            )?;
            if !status.is_empty() {
                return Err(RemoveError::Dirty);
            }
        }
    }
    Ok(vec![
        OsString::from("worktree"),
        OsString::from("remove"),
        OsString::from("--"),
        root.as_os_str().to_owned(),
    ])
}

/// Builds `git worktree add -- <path> <commit-ish>`.
///
/// A branch name checks that branch out in the new worktree (Git refuses a branch checked out
/// elsewhere); any other commit-ish produces a detached `HEAD`. The caller owns the
/// "destination must not exist" policy.
///
/// # Errors
/// Rejects a relative path, and a commit-ish that is empty, longer than 1024 bytes, option-like
/// or contains control characters.
pub fn add_arguments(
    path: &Path,
    commit_ish: &str,
) -> Result<Vec<OsString>, super::CommandValidationError> {
    if !path.is_absolute() {
        return Err(super::CommandValidationError(
            "a new worktree path must be absolute".into(),
        ));
    }
    super::validate_label(commit_ish, 1024, "worktree start point")?;
    Ok(vec![
        "worktree".into(),
        "add".into(),
        "--".into(),
        path.as_os_str().to_owned(),
        commit_ish.into(),
    ])
}
