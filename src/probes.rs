//! Small read-only Git probes with one canonical command form each.
//!
//! Every probe runs through the caller's [`CommandExecutor`] (so activity, cancellation and
//! process-tree ownership stay with the caller), pins `LC_ALL=C`, disables optional locks with
//! `GIT_OPTIONAL_LOCKS=0`, bounds its output and redacts the public activity streams. Probes
//! answer mechanical questions only; whether a result should warn or block belongs to the
//! application.

use crate::activity::ActivityStatus;
use crate::diff::model::DiffPath;
use crate::loading::CommandExecutor;
use crate::process::{GitProcessCommand, PrivateActivityResult, ProcessArgument};
use crate::types::ObjectId;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::Path;

/// Output bound for probes that return one object ID or a yes/no answer.
pub const SMALL_PROBE_OUTPUT_BYTES: usize = 64 * 1024;
/// Output bound for probes that return path or commit lists.
pub const LIST_PROBE_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// Why a probe produced no answer. Values never contain Git output or user content.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ProbeFailure {
    /// The Git executable could not be launched.
    MissingGit,
    /// The caller canceled the probe or its deadline expired.
    Canceled,
    /// A supplied revision or range is not acceptable as a single Git argument.
    InvalidArgument,
    /// Git output was truncated or malformed.
    InvalidOutput,
    /// The result exceeds the caller's limit.
    TooMany,
    /// Git refused the probe for another reason.
    GitFailed,
}

impl std::fmt::Display for ProbeFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::MissingGit => "Git could not be launched",
            Self::Canceled => "the Git probe was canceled",
            Self::InvalidArgument => "the probe argument is not a valid revision",
            Self::InvalidOutput => "Git returned truncated or malformed probe output",
            Self::TooMany => "the probe result exceeds the requested limit",
            Self::GitFailed => "Git refused the probe",
        })
    }
}

impl std::error::Error for ProbeFailure {}

/// Which difference [`has_changes`] looks for.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ChangeScope {
    /// The index differs from `HEAD` (an unborn `HEAD` compares against the empty tree).
    Staged,
    /// Tracked worktree files differ from the index.
    Unstaged,
    /// Untracked files exist that are not ignored by the standard exclude rules.
    Untracked,
}

/// Builds the canonical read-only probe command: `LC_ALL=C`, `GIT_OPTIONAL_LOCKS=0`, bounded and
/// redacted output, run in `worktree`.
///
/// # Panics
/// Never: the environment names are constant and valid.
#[must_use]
pub fn probe_command(
    executable: &OsStr,
    worktree: &Path,
    arguments: &[&str],
    output_limit: usize,
) -> GitProcessCommand {
    GitProcessCommand::git(executable)
        .current_dir(worktree)
        .env("LC_ALL", "C")
        .and_then(|command| command.env("GIT_OPTIONAL_LOCKS", "0"))
        .expect("constant environment names are valid")
        .args(arguments.iter().copied().map(ProcessArgument::visible))
        .output_limit(output_limit)
        .redact_output()
}

fn launch_or_cancel(status: &ActivityStatus) -> ProbeFailure {
    match status {
        ActivityStatus::LaunchError { .. } => ProbeFailure::MissingGit,
        ActivityStatus::Canceled { .. } | ActivityStatus::TimedOut { .. } => ProbeFailure::Canceled,
        _ => ProbeFailure::GitFailed,
    }
}

fn successful_output(mut result: PrivateActivityResult) -> Result<Vec<u8>, ProbeFailure> {
    match result.activity.status {
        ActivityStatus::Succeeded { .. } if !result.stdout.truncated => {
            Ok(result.stdout.take_bytes())
        }
        ActivityStatus::Succeeded { .. } => Err(ProbeFailure::InvalidOutput),
        ref status => Err(launch_or_cancel(status)),
    }
}

/// Maps the exit-code convention "0 = yes, 1 = no" used by `--quiet`/`--is-ancestor` probes.
fn exit_answer(result: &PrivateActivityResult, zero_means: bool) -> Result<bool, ProbeFailure> {
    match result.activity.status {
        ActivityStatus::Succeeded { .. } => Ok(zero_means),
        ActivityStatus::Failed { exit_code: Some(1) } => Ok(!zero_means),
        ref status => Err(launch_or_cancel(status)),
    }
}

fn validate_revision(revision: &str) -> Result<(), ProbeFailure> {
    if revision.is_empty()
        || revision.starts_with('-')
        || revision.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
    {
        return Err(ProbeFailure::InvalidArgument);
    }
    Ok(())
}

fn parse_oid(bytes: &[u8]) -> Result<ObjectId, ProbeFailure> {
    crate::history::search::parse_one_oid(bytes).map_err(|_| ProbeFailure::InvalidOutput)
}

/// Resolves `revision` with `git rev-parse --verify --quiet <revision>`.
///
/// Returns `Ok(None)` when Git reports that the revision does not resolve (exit status 1).
/// Use a peel suffix such as `HEAD^{commit}` to require an object type.
///
/// # Errors
/// Rejects empty, option-like or control-character revisions; reports launch, cancellation,
/// malformed-output and other Git failures.
pub fn resolve(
    executable: &OsStr,
    worktree: &Path,
    revision: &str,
    executor: &mut dyn CommandExecutor,
) -> Result<Option<ObjectId>, ProbeFailure> {
    validate_revision(revision)?;
    let mut result = executor.execute(probe_command(
        executable,
        worktree,
        &["rev-parse", "--verify", "--quiet", revision],
        SMALL_PROBE_OUTPUT_BYTES,
    ));
    match result.activity.status {
        ActivityStatus::Succeeded { .. } if !result.stdout.truncated => {
            parse_oid(&result.stdout.take_bytes()).map(Some)
        }
        ActivityStatus::Succeeded { .. } => Err(ProbeFailure::InvalidOutput),
        ActivityStatus::Failed { exit_code: Some(1) } => Ok(None),
        ref status => Err(launch_or_cancel(status)),
    }
}

/// Answers whether `ancestor` is reachable from `descendant` with
/// `git merge-base --is-ancestor`. A commit is its own ancestor.
///
/// # Errors
/// Reports launch, cancellation and Git failures (for example a missing object).
pub fn is_ancestor(
    executable: &OsStr,
    worktree: &Path,
    ancestor: &ObjectId,
    descendant: &ObjectId,
    executor: &mut dyn CommandExecutor,
) -> Result<bool, ProbeFailure> {
    let result = executor.execute(probe_command(
        executable,
        worktree,
        &[
            "merge-base",
            "--is-ancestor",
            ancestor.as_str(),
            descendant.as_str(),
        ],
        SMALL_PROBE_OUTPUT_BYTES,
    ));
    exit_answer(&result, true)
}

/// Lists unmerged index paths once each, in index order, with `git ls-files --unmerged -z`.
///
/// Paths are Git's raw bytes (no quoting, no lossy conversion).
///
/// # Errors
/// Reports launch, cancellation, truncated/malformed output and Git failures.
pub fn unmerged_paths(
    executable: &OsStr,
    worktree: &Path,
    executor: &mut dyn CommandExecutor,
) -> Result<Vec<DiffPath>, ProbeFailure> {
    let bytes = successful_output(executor.execute(probe_command(
        executable,
        worktree,
        &["ls-files", "--unmerged", "-z"],
        LIST_PROBE_OUTPUT_BYTES,
    )))?;
    parse_unmerged_paths(&bytes)
}

/// The unmerged index entries (stages 1 to 3 per path) from `git ls-files --unmerged -z`, for
/// callers that need the stage objects and modes rather than only the paths.
///
/// # Errors
/// Reports launch, cancellation, truncated or malformed output and Git failures.
pub fn unmerged_index(
    executable: &OsStr,
    worktree: &Path,
    executor: &mut dyn CommandExecutor,
) -> Result<UnmergedIndex, ProbeFailure> {
    let bytes = successful_output(executor.execute(probe_command(
        executable,
        worktree,
        &["ls-files", "--unmerged", "-z"],
        LIST_PROBE_OUTPUT_BYTES,
    )))?;
    crate::diff::index::parse_unmerged_index(&bytes).map_err(|_| ProbeFailure::InvalidOutput)
}

/// Lists the paths `commit` changes against its first parent (the empty tree for a root
/// commit) with `git diff-tree -r -z --name-only --no-renames --root --no-commit-id
/// --diff-merges=first-parent <commit>`.
///
/// Renames are reported as their deleted source and added destination, so the result names
/// every path the change writes. Paths are Git's raw bytes in Git's order.
///
/// # Errors
/// Reports launch, cancellation, truncated/malformed output and Git failures.
pub fn changed_paths(
    executable: &OsStr,
    worktree: &Path,
    commit: &ObjectId,
    executor: &mut dyn CommandExecutor,
) -> Result<Vec<DiffPath>, ProbeFailure> {
    let bytes = successful_output(executor.execute(probe_command(
        executable,
        worktree,
        &[
            "diff-tree",
            "-r",
            "-z",
            "--name-only",
            "--no-renames",
            "--root",
            "--no-commit-id",
            "--diff-merges=first-parent",
            commit.as_str(),
        ],
        LIST_PROBE_OUTPUT_BYTES,
    )))?;
    parse_name_list(&bytes)
}

/// Parses a NUL-terminated path list such as `git diff-tree -z --name-only` prints.
///
/// # Errors
/// Returns [`ProbeFailure::InvalidOutput`] for a path Git would not print.
pub fn parse_name_list(bytes: &[u8]) -> Result<Vec<DiffPath>, ProbeFailure> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| {
            DiffPath::from_bytes(record.to_vec()).map_err(|_| ProbeFailure::InvalidOutput)
        })
        .collect()
}

/// Unmerged paths with their base (1), ours (2) and theirs (3) stage entries.
pub type UnmergedIndex =
    std::collections::BTreeMap<DiffPath, [Option<crate::diff::index::IndexStage>; 3]>;

/// Parses `git ls-files --unmerged -z` (`<mode> <oid> <stage>\t<path>\0` per stage) into unique
/// paths in first-seen order.
///
/// # Errors
/// Returns [`ProbeFailure::InvalidOutput`] for a malformed record.
pub fn parse_unmerged_paths(bytes: &[u8]) -> Result<Vec<DiffPath>, ProbeFailure> {
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for record in bytes.split(|byte| *byte == 0).filter(|r| !r.is_empty()) {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or(ProbeFailure::InvalidOutput)?;
        let header = &record[..tab];
        let fields: Vec<&[u8]> = header.split(|byte| *byte == b' ').collect();
        if fields.len() != 3
            || fields[0].len() != 6
            || !matches!(fields[2], b"1" | b"2" | b"3")
            || parse_oid(fields[1]).is_err()
        {
            return Err(ProbeFailure::InvalidOutput);
        }
        let path = &record[tab + 1..];
        if seen.insert(path.to_vec()) {
            paths.push(
                DiffPath::from_bytes(path.to_vec()).map_err(|_| ProbeFailure::InvalidOutput)?,
            );
        }
    }
    Ok(paths)
}

/// Answers whether the repository has changes in `scope`.
///
/// `Staged` runs `git diff --cached --quiet --`, `Unstaged` runs `git diff --quiet --`, and
/// `Untracked` runs `git ls-files --others --exclude-standard --directory
/// --no-empty-directory -z`. Diff probes disable external diff drivers and text conversion.
///
/// # Errors
/// Reports launch, cancellation and Git failures.
pub fn has_changes(
    executable: &OsStr,
    worktree: &Path,
    scope: ChangeScope,
    executor: &mut dyn CommandExecutor,
) -> Result<bool, ProbeFailure> {
    match scope {
        ChangeScope::Staged | ChangeScope::Unstaged => {
            let mut arguments = vec!["diff", "--no-ext-diff", "--no-textconv"];
            if scope == ChangeScope::Staged {
                arguments.push("--cached");
            }
            arguments.extend(["--quiet", "--"]);
            let result = executor.execute(probe_command(
                executable,
                worktree,
                &arguments,
                SMALL_PROBE_OUTPUT_BYTES,
            ));
            exit_answer(&result, false)
        }
        ChangeScope::Untracked => {
            let result = executor.execute(probe_command(
                executable,
                worktree,
                &[
                    "ls-files",
                    "--others",
                    "--exclude-standard",
                    "--directory",
                    "--no-empty-directory",
                    "-z",
                ],
                SMALL_PROBE_OUTPUT_BYTES,
            ));
            // A truncated listing is still a nonempty one.
            match result.activity.status {
                ActivityStatus::Succeeded { .. } => Ok(!result.stdout.bytes.is_empty()),
                ref status => Err(launch_or_cancel(status)),
            }
        }
    }
}

/// Answers whether `commit` is reachable from any remote-tracking ref (`refs/remotes/*`),
/// with `git for-each-ref --count=1 --contains <commit> refs/remotes/`.
///
/// # Errors
/// Reports launch, cancellation and Git failures.
pub fn is_published(
    executable: &OsStr,
    worktree: &Path,
    commit: &ObjectId,
    executor: &mut dyn CommandExecutor,
) -> Result<bool, ProbeFailure> {
    let bytes = successful_output(executor.execute(probe_command(
        executable,
        worktree,
        &[
            "for-each-ref",
            "--count=1",
            "--format=%(objectname)",
            "--contains",
            commit.as_str(),
            "refs/remotes/",
        ],
        SMALL_PROBE_OUTPUT_BYTES,
    )))?;
    Ok(bytes.iter().any(|byte| !byte.is_ascii_whitespace()))
}

/// A commit range `base..tip`. Without a base, the range is every commit reachable from `tip`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitRange {
    pub base: Option<ObjectId>,
    pub tip: ObjectId,
}

/// Returns the commits of `range` that are reachable from any remote-tracking ref
/// (`refs/remotes/*`), in `git rev-list` order (newest first).
///
/// Runs `git rev-list --max-count=<limit + 1> <tip> [^<base>]` and
/// `git rev-list <tip> --not [<base>] --remotes`; the published commits are the first list minus
/// the second. Whether rewriting them should warn is the caller's policy.
///
/// # Errors
/// Returns [`ProbeFailure::TooMany`] when the range holds more than `limit` commits, and
/// reports launch, cancellation, malformed-output and Git failures.
pub fn published_commits(
    executable: &OsStr,
    worktree: &Path,
    range: &CommitRange,
    limit: usize,
    executor: &mut dyn CommandExecutor,
) -> Result<Vec<ObjectId>, ProbeFailure> {
    let max_count = format!("--max-count={}", limit.saturating_add(1));
    let excluded = range
        .base
        .as_ref()
        .map(|base| format!("^{}", base.as_str()));
    let mut all_arguments = vec!["rev-list", max_count.as_str(), range.tip.as_str()];
    if let Some(excluded) = &excluded {
        all_arguments.push(excluded);
    }
    let all = parse_oid_lines(&successful_output(executor.execute(probe_command(
        executable,
        worktree,
        &all_arguments,
        LIST_PROBE_OUTPUT_BYTES,
    )))?)?;
    if all.len() > limit {
        return Err(ProbeFailure::TooMany);
    }
    if all.is_empty() {
        return Ok(all);
    }
    let mut local_arguments = vec!["rev-list", range.tip.as_str(), "--not"];
    if let Some(base) = &range.base {
        local_arguments.push(base.as_str());
    }
    local_arguments.push("--remotes");
    let local: HashSet<ObjectId> =
        parse_oid_lines(&successful_output(executor.execute(probe_command(
            executable,
            worktree,
            &local_arguments,
            LIST_PROBE_OUTPUT_BYTES,
        )))?)?
        .into_iter()
        .collect();
    Ok(all
        .into_iter()
        .filter(|commit| !local.contains(commit))
        .collect())
}

fn parse_oid_lines(bytes: &[u8]) -> Result<Vec<ObjectId>, ProbeFailure> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(parse_oid)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmerged_records_are_deduplicated_and_validated() {
        let oid = "a".repeat(40);
        let bytes =
            format!("100644 {oid} 1\tsame\0100644 {oid} 2\tsame\0100644 {oid} 3\tother path\0");
        let paths = parse_unmerged_paths(bytes.as_bytes()).unwrap();
        assert_eq!(
            paths.iter().map(DiffPath::as_bytes).collect::<Vec<_>>(),
            [&b"same"[..], b"other path"]
        );
        assert_eq!(
            parse_unmerged_paths(b"garbage\0"),
            Err(ProbeFailure::InvalidOutput)
        );
        assert_eq!(
            parse_unmerged_paths(format!("100644 {oid} 0\tx\0").as_bytes()),
            Err(ProbeFailure::InvalidOutput)
        );
    }

    #[test]
    fn option_like_revisions_are_rejected_before_spawning() {
        let mut executor = |_: GitProcessCommand| -> PrivateActivityResult {
            panic!("a rejected revision must not run Git")
        };
        for revision in ["", "--all", "HEAD\nx"] {
            assert_eq!(
                resolve(OsStr::new("git"), Path::new("."), revision, &mut executor),
                Err(ProbeFailure::InvalidArgument)
            );
        }
    }

    #[test]
    fn probe_command_pins_locale_and_optional_locks() {
        let command = probe_command(OsStr::new("git"), Path::new("."), &["status"], 10);
        let activity = command.activity_command();
        let environment: Vec<_> = activity
            .environment
            .iter()
            .map(|value| (value.name.as_str(), value.display.as_str()))
            .collect();
        assert!(environment.contains(&("LC_ALL", "C")));
        assert!(environment.contains(&("GIT_OPTIONAL_LOCKS", "0")));
    }
}
