//! Standard Git ignore exclusions for watcher registration.
use super::{WatchExclusionError, MAX_WATCH_EXCLUSIONS};
use crate::activity::ActivityStatus;
use crate::loading::CommandExecutor;
use crate::process::{GitProcessCommand, ProcessArgument, SemanticTimeout};
use crate::types::{RepositoryRelativePath, WorktreeId};
use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::path::PathBuf;
use std::time::Duration;
/// Git walks every unignored directory to list the ignored ones, which takes most of a second
/// in a worktree of 60k files. A shorter bound left such repositories watched without exclusions.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Reads bounded collapsed ignore roots through recorded Git execution.
/// # Errors
/// Returns cancellation, process, framing or path failures.
pub fn load(
    executable: &OsStr,
    worktree: &WorktreeId,
    executor: &mut dyn CommandExecutor,
) -> Result<Vec<RepositoryRelativePath>, WatchExclusionError> {
    let command = GitProcessCommand::git(executable)
        .current_dir(worktree.worktree_root().as_path())
        .args(
            [
                "ls-files",
                "-o",
                "-i",
                "--directory",
                "--exclude-standard",
                "-z",
            ]
            .map(ProcessArgument::visible),
        )
        .output_limit(4 * 1024 * 1024)
        .semantic_timeout(
            SemanticTimeout::new(DISCOVERY_TIMEOUT, "watch ignore discovery")
                .map_err(|_| WatchExclusionError::new("invalid ignore timeout"))?,
        )
        .env("GIT_OPTIONAL_LOCKS", "0")
        .map_err(|_| WatchExclusionError::new("invalid ignore environment"))?;
    let result = executor.execute(command);
    match result.activity.status {
        ActivityStatus::Succeeded { .. } if !result.stdout.truncated => {
            parse_ignored_paths(&result.stdout.bytes)
        }
        ActivityStatus::Canceled { .. } => {
            Err(WatchExclusionError::new("ignore discovery was canceled"))
        }
        _ => Err(WatchExclusionError::new(
            "Git could not resolve standard ignore rules",
        )),
    }
}
/// Parses bounded, NUL-delimited ignored roots without losing native path bytes.
/// # Errors
/// Rejects paths that escape the repository or cannot be represented on this platform.
pub fn parse_ignored_paths(
    output: &[u8],
) -> Result<Vec<RepositoryRelativePath>, WatchExclusionError> {
    let mut paths = Vec::new();
    for bytes in output
        .split(|byte| *byte == 0)
        .filter(|bytes| !bytes.is_empty())
    {
        if paths.len() == MAX_WATCH_EXCLUSIONS {
            break;
        }
        paths.push(native_path(bytes)?);
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

#[cfg(unix)]
fn native_path(bytes: &[u8]) -> Result<RepositoryRelativePath, WatchExclusionError> {
    use std::os::unix::ffi::OsStringExt;
    RepositoryRelativePath::new(PathBuf::from(OsString::from_vec(bytes.to_vec())))
        .map_err(|_| WatchExclusionError::new("Git returned an invalid ignored path"))
}

#[cfg(not(unix))]
fn native_path(bytes: &[u8]) -> Result<RepositoryRelativePath, WatchExclusionError> {
    let path = std::str::from_utf8(bytes)
        .map_err(|_| WatchExclusionError::new("Git returned a non-Unicode ignored path"))?;
    RepositoryRelativePath::new(path)
        .map_err(|_| WatchExclusionError::new("Git returned an invalid ignored path"))
}
