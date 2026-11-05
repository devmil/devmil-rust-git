//! Bounded repository statistics with optional coherent snapshot counts.
use super::CommandExecutor;
use crate::activity::{ActivityRecord, ActivityStatus};
use crate::cancellation::Cancellation;
use crate::process::{GitProcessCommand, ProcessArgument, SemanticTimeout};
use crate::types::WorktreeId;
use std::{fmt, time::Duration};
pub const MAX_MANAGER_REFS: usize = 10_000;
pub const MAX_MANAGER_CHANGED_PATHS: usize = 10_000;
const OUTPUT_LIMIT: usize = 2 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryStatistics {
    pub commit_count: u64,
    pub local_branch_count: usize,
    pub tag_count: usize,
    pub changed_path_count: usize,
    pub incomplete: bool,
    pub activities: Vec<ActivityRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepositoryStatisticsError {
    Canceled,
    TimedOut,
    GitFailed,
    MalformedOutput,
    ResourceLimit,
}

impl fmt::Display for RepositoryStatisticsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "repository statistics failed: {self:?}")
    }
}

impl std::error::Error for RepositoryStatisticsError {}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StatisticsCounts {
    pub local_branch_count: usize,
    pub tag_count: usize,
    pub changed_path_count: usize,
    pub incomplete: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StatisticsOptions {
    pub bare: bool,
    pub known_counts: Option<StatisticsCounts>,
}
fn run(
    executable: &std::ffi::OsStr,
    worktree: &WorktreeId,
    cancellation: &dyn Cancellation,
    executor: &mut dyn CommandExecutor,
    arguments: &[&str],
    activities: &mut Vec<ActivityRecord>,
) -> Result<Vec<u8>, RepositoryStatisticsError> {
    let command = GitProcessCommand::git(executable)
        .current_dir(worktree.worktree_root().as_path())
        .args(arguments.iter().map(ProcessArgument::visible))
        .output_limit(OUTPUT_LIMIT)
        .semantic_timeout(SemanticTimeout::new(TIMEOUT, "repository statistics").unwrap())
        .redact_output();
    let mut result = executor.execute(command);
    let truncated = result.stdout.truncated || result.stderr.truncated;
    let bytes = result.stdout.take_bytes();
    activities.push(result.activity.clone());
    // Cancellation is sticky at the adapter boundary. The process runner checks it before
    // every child-status poll, but the caller can cancel after the runner's final poll and
    // before this result is mapped. In that narrow race the child is already reaped, and the
    // requested cancellation must not surface as an unrelated Git failure.
    if cancellation.is_cancelled() {
        return Err(RepositoryStatisticsError::Canceled);
    }
    if truncated {
        return Err(RepositoryStatisticsError::ResourceLimit);
    }
    match result.activity.status {
        ActivityStatus::Succeeded { .. } => Ok(bytes),
        ActivityStatus::Canceled { .. } => Err(RepositoryStatisticsError::Canceled),
        ActivityStatus::TimedOut { .. } => Err(RepositoryStatisticsError::TimedOut),
        _ => Err(RepositoryStatisticsError::GitFailed),
    }
}

/// Loads commit counts, reusing coherent status/reference counts when supplied.
/// # Errors
/// Returns cancellation, command timeout, malformed output or resource-limit errors.
pub fn load(
    executable: &std::ffi::OsStr,
    worktree: &WorktreeId,
    options: StatisticsOptions,
    cancellation: &dyn Cancellation,
    executor: &mut dyn CommandExecutor,
) -> Result<RepositoryStatistics, RepositoryStatisticsError> {
    if cancellation.is_cancelled() {
        return Err(RepositoryStatisticsError::Canceled);
    }
    let mut activities = Vec::new();
    let commits = run(
        executable,
        worktree,
        cancellation,
        executor,
        &["rev-list", "--count", "--all"],
        &mut activities,
    )?;
    let commit_count = std::str::from_utf8(&commits)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .ok_or(RepositoryStatisticsError::MalformedOutput)?;
    let counts = if let Some(counts) = options.known_counts {
        counts
    } else {
        let refs = run(
            executable,
            worktree,
            cancellation,
            executor,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads",
                "refs/tags",
            ],
            &mut activities,
        )?;
        let mut counts = StatisticsCounts::default();
        for (index, line) in refs
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .enumerate()
        {
            if index >= MAX_MANAGER_REFS {
                counts.incomplete = true;
                break;
            }
            if line.starts_with(b"refs/heads/") {
                counts.local_branch_count += 1;
            } else if line.starts_with(b"refs/tags/") {
                counts.tag_count += 1;
            } else {
                return Err(RepositoryStatisticsError::MalformedOutput);
            }
        }
        if !options.bare {
            let status = run(
                executable,
                worktree,
                cancellation,
                executor,
                &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
                &mut activities,
            )?;
            let (changed, incomplete) = count_changed_paths(&status)?;
            counts.changed_path_count = changed;
            counts.incomplete |= incomplete;
        }
        counts
    };
    if cancellation.is_cancelled() {
        return Err(RepositoryStatisticsError::Canceled);
    }
    Ok(RepositoryStatistics {
        commit_count,
        local_branch_count: counts.local_branch_count,
        tag_count: counts.tag_count,
        changed_path_count: counts.changed_path_count,
        incomplete: counts.incomplete,
        activities,
    })
}
fn count_changed_paths(raw: &[u8]) -> Result<(usize, bool), RepositoryStatisticsError> {
    if !raw.is_empty() && !raw.ends_with(&[0]) {
        return Err(RepositoryStatisticsError::MalformedOutput);
    }
    if raw.is_empty() {
        return Ok((0, false));
    }
    let mut fields = raw[..raw.len() - 1].split(|byte| *byte == 0);
    let mut count = 0;
    while let Some(record) = fields.next() {
        if record.len() < 4
            || record[2] != b' '
            || record[..2].iter().any(|byte| !b" MADRCU?!T".contains(byte))
        {
            return Err(RepositoryStatisticsError::MalformedOutput);
        }
        if record[..2].iter().any(|byte| matches!(byte, b'R' | b'C'))
            && fields.next().is_none_or(<[u8]>::is_empty)
        {
            return Err(RepositoryStatisticsError::MalformedOutput);
        }
        if count == MAX_MANAGER_CHANGED_PATHS {
            return Ok((count, true));
        }
        count += 1;
    }
    Ok((count, false))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rename_counts_one_changed_path_and_requires_its_source() {
        assert_eq!(
            count_changed_paths(b"R  new\0old\0?? other\0").unwrap(),
            (2, false)
        );
        assert!(count_changed_paths(b"R  new\0").is_err());
        assert!(count_changed_paths(b"?? missing-terminator").is_err());
    }
    #[test]
    fn changed_path_bound_reports_incomplete() {
        let raw = b"?? path\0".repeat(MAX_MANAGER_CHANGED_PATHS + 1);
        assert_eq!(
            count_changed_paths(&raw).unwrap(),
            (MAX_MANAGER_CHANGED_PATHS, true)
        );
    }
}
