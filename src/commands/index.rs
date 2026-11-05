//! Staging and unstaging invocations.
//!
//! Paths are Git's repository-relative bytes ([`DiffPath`]). With [`PathspecForm::Arguments`]
//! they become native arguments after `--` (redacted as `<path>` in activity records); with
//! [`PathspecForm::StdinNul`] they are passed NUL-separated on standard input through
//! `--pathspec-from-file=- --pathspec-file-nul`, which keeps very large selections below
//! command-line limits. Every invocation sets `GIT_LITERAL_PATHSPECS=1`, so a file named `*.txt`
//! selects only itself; directory paths still select their contents.
//!
//! Index transactions on a private index file are in [`crate::index_transaction::IndexTransaction`].

use super::invocation::{CommandArgument, GitInvocation};
use super::CommandValidationError;
use crate::diff::model::DiffPath;
use crate::native_bytes::bytes_to_os_string;

/// The side whose whole-file changes are selected from porcelain status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexSelectionSide {
    Staged,
    Unstaged,
}

/// Complete literal path identities for whole-file staging, without generating a patch.
/// Rename sources belong only to the side that reports the rename. In particular, staging
/// an edit to a staged rename's destination must not also stage a replacement at its source.
/// Untracked directories are normalized to their literal directory path. Conflicted and
/// ignored entries are excluded; consumers own conflict-resolution policy.
///
/// # Errors
/// Returns a validation failure if a status path cannot be represented as a diff path.
pub fn status_paths(
    status: &crate::status::RepositoryStatus,
    side: IndexSelectionSide,
) -> Result<Vec<DiffPath>, CommandValidationError> {
    use crate::status::{ChangeKind, StatusEntry};
    let mut paths = std::collections::BTreeSet::new();
    for entry in &status.entries {
        let state = match entry {
            StatusEntry::Ordinary(value) => Some(value.state),
            StatusEntry::Rename(value) => Some(value.state),
            StatusEntry::Untracked(_) if side == IndexSelectionSide::Unstaged => None,
            _ => continue,
        };
        if let Some(state) = state {
            let change = match side {
                IndexSelectionSide::Staged => state.index,
                IndexSelectionSide::Unstaged => state.worktree,
            };
            if change == ChangeKind::Unmodified {
                continue;
            }
            if change == ChangeKind::Renamed {
                if let StatusEntry::Rename(value) = entry {
                    paths.insert(crate::native_bytes::path_to_bytes(value.source.as_path()));
                }
            }
        }
        let bytes = crate::native_bytes::path_to_bytes(entry.path().as_path());
        paths.insert(bytes);
    }
    paths.into_iter().map(|bytes| status_path(&bytes)).collect()
}

/// Converts a porcelain path identity to a literal index pathspec. Git reports untracked
/// directories with a trailing slash; it is not part of the directory's native name.
/// # Errors
/// Rejects invalid repository-relative paths.
pub fn status_path(bytes: &[u8]) -> Result<DiffPath, CommandValidationError> {
    DiffPath::from_bytes(bytes.strip_suffix(b"/").unwrap_or(bytes).to_vec())
        .map_err(|_| CommandValidationError("invalid status path".into()))
}

/// Expands selected status-row paths to the index endpoints on the selected side.
/// Selecting a rename destination includes its source only when that side reports a rename;
/// a separate untracked replacement at the source remains a separate selection.
/// # Errors
/// Rejects duplicate or unavailable row paths instead of silently widening the selection.
pub fn selected_status_paths(
    status: &crate::status::RepositoryStatus,
    side: IndexSelectionSide,
    selected: &[DiffPath],
) -> Result<Vec<DiffPath>, CommandValidationError> {
    let requested = selected
        .iter()
        .cloned()
        .collect::<std::collections::HashSet<_>>();
    if requested.len() != selected.len() {
        return Err(CommandValidationError("duplicate status selection".into()));
    }
    let mut matched = std::collections::HashSet::new();
    let mut entries = Vec::new();
    for entry in &status.entries {
        let path = status_path(&crate::native_bytes::path_to_bytes(entry.path().as_path()))?;
        if !requested.contains(&path) {
            continue;
        }
        let single = crate::status::RepositoryStatus::from_parts(
            status.branch.clone(),
            vec![entry.clone()],
            vec![],
        );
        if !status_paths(&single, side)?.is_empty() {
            matched.insert(path);
            entries.push(entry.clone());
        }
    }
    if matched != requested {
        return Err(CommandValidationError(
            "status selection is unavailable".into(),
        ));
    }
    status_paths(
        &crate::status::RepositoryStatus::from_parts(status.branch.clone(), entries, vec![]),
        side,
    )
}

/// How paths reach Git.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum PathspecForm {
    /// `-- <path>...`
    #[default]
    Arguments,
    /// `--pathspec-from-file=- --pathspec-file-nul` with NUL-separated stdin (Git 2.25+).
    StdinNul,
}

/// What the index is unstaged against.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum UnstageBase {
    /// `git reset -q HEAD -- <paths>`.
    Head,
    /// No commit exists yet: `git rm --cached -r -f -q -- <paths>`.
    Unborn,
}

/// Appends `paths` in `form` and pins literal pathspecs.
pub(super) fn path_invocation(
    mut arguments: Vec<CommandArgument>,
    paths: &[DiffPath],
    form: PathspecForm,
) -> Result<GitInvocation, CommandValidationError> {
    if paths.is_empty() {
        return Err(CommandValidationError(
            "a path operation needs at least one path".into(),
        ));
    }
    let invocation = match form {
        PathspecForm::Arguments => {
            arguments.push(CommandArgument::visible("--"));
            for path in paths {
                let bytes = path
                    .as_bytes()
                    .strip_suffix(b"/")
                    .unwrap_or(path.as_bytes());
                let native = bytes_to_os_string(bytes).map_err(|_| {
                    CommandValidationError("a path is not a valid native name".into())
                })?;
                arguments.push(CommandArgument::redacted(native, "<path>"));
            }
            GitInvocation::new(arguments)
        }
        PathspecForm::StdinNul => {
            arguments.push(CommandArgument::visible("--pathspec-from-file=-"));
            arguments.push(CommandArgument::visible("--pathspec-file-nul"));
            let mut stdin = Vec::new();
            for path in paths {
                stdin.extend_from_slice(
                    path.as_bytes()
                        .strip_suffix(b"/")
                        .unwrap_or(path.as_bytes()),
                );
                stdin.push(0);
            }
            GitInvocation::new(arguments).stdin_bytes(stdin)
        }
    };
    Ok(invocation.env("GIT_LITERAL_PATHSPECS", "1"))
}

/// `git add -A -- <paths>`: stages modifications, additions and deletions of the paths.
///
/// # Errors
/// Rejects an empty selection or a path that is not a native name on this platform.
pub fn stage_paths(
    paths: &[DiffPath],
    form: PathspecForm,
) -> Result<GitInvocation, CommandValidationError> {
    path_invocation(
        ["add", "-A"].map(CommandArgument::visible).to_vec(),
        paths,
        form,
    )
}

/// Restores the index entries of `paths` from `HEAD`, or removes them when `HEAD` is unborn.
///
/// # Errors
/// Rejects an empty selection or a path that is not a native name on this platform.
pub fn unstage_paths(
    paths: &[DiffPath],
    base: UnstageBase,
    form: PathspecForm,
) -> Result<GitInvocation, CommandValidationError> {
    let arguments = match base {
        UnstageBase::Head => vec!["reset", "-q", "HEAD"],
        UnstageBase::Unborn => vec!["rm", "--cached", "-r", "-f", "-q"],
    };
    path_invocation(
        arguments
            .into_iter()
            .map(CommandArgument::visible)
            .collect(),
        paths,
        form,
    )
}

/// `git apply --cached --recount --unidiff-zero -` with `patch` on stdin: applies a partial
/// (row-level) patch to the index only. Build a reverse patch to unstage rows.
#[must_use]
pub fn apply_to_index(patch: Vec<u8>) -> GitInvocation {
    // The explicit `-` operand avoids Git-for-Windows path translation and the
    // version-dependent no-argument stdin form.
    GitInvocation::visible(["apply", "--cached", "--recount", "--unidiff-zero", "-"])
        .stdin_bytes(patch)
}

/// `git read-tree --empty`: initializes an empty index (for a repository without one).
#[must_use]
pub fn read_tree_empty() -> GitInvocation {
    GitInvocation::visible(["read-tree", "--empty"])
}

/// Which conflict side [`checkout_conflict_side`] writes to the worktree.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ConflictSide {
    /// `--ours`: stage 2.
    Ours,
    /// `--theirs`: stage 3.
    Theirs,
}

/// `git checkout --ours|--theirs -- <path>`: writes one side of an unmerged path to the
/// worktree, leaving the index unmerged. Stage the result with [`stage_paths`].
///
/// # Errors
/// Rejects a path that is not a native name on this platform.
pub fn checkout_conflict_side(
    path: &DiffPath,
    side: ConflictSide,
) -> Result<GitInvocation, CommandValidationError> {
    path_invocation(
        vec![
            CommandArgument::visible("checkout"),
            CommandArgument::visible(match side {
                ConflictSide::Ours => "--ours",
                ConflictSide::Theirs => "--theirs",
            }),
        ],
        std::slice::from_ref(path),
        PathspecForm::Arguments,
    )
}

/// One index stage entry for [`restore_conflict_stages`].
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct IndexStageEntry {
    /// Git file mode, such as `0o100644`.
    pub mode: u32,
    pub object: crate::types::ObjectId,
}

/// `git update-index -z --index-info` that removes the stage-0 entry of `path` and writes the
/// given stages 1 (base), 2 (ours) and 3 (theirs), restoring an unmerged path after a
/// resolution was staged.
///
/// # Errors
/// Rejects a mode that is not a Git file mode, or stages with mixed object formats.
pub fn restore_conflict_stages(
    path: &DiffPath,
    stages: &[Option<IndexStageEntry>; 3],
) -> Result<GitInvocation, CommandValidationError> {
    let mut lengths = stages
        .iter()
        .flatten()
        .map(|stage| stage.object.as_str().len());
    let zeros = lengths.next().unwrap_or(40);
    if lengths.any(|length| length != zeros) {
        return Err(CommandValidationError(
            "conflict stages mix object formats".into(),
        ));
    }
    let mut input = format!("0 {}\t", "0".repeat(zeros)).into_bytes();
    input.extend_from_slice(path.as_bytes());
    input.push(0);
    for (index, stage) in stages.iter().enumerate() {
        let Some(stage) = stage else { continue };
        if !matches!(stage.mode, 0o100_644 | 0o100_755 | 0o120_000 | 0o160_000) {
            return Err(CommandValidationError(
                "conflict stage mode is not a Git file mode".into(),
            ));
        }
        input.extend_from_slice(
            format!(
                "{:06o} {} {}\t",
                stage.mode,
                stage.object.as_str(),
                index + 1
            )
            .as_bytes(),
        );
        input.extend_from_slice(path.as_bytes());
        input.push(0);
    }
    Ok(GitInvocation::visible(["update-index", "-z", "--index-info"]).stdin_bytes(input))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(values: &[&[u8]]) -> Vec<DiffPath> {
        values
            .iter()
            .map(|value| DiffPath::from_bytes(value.to_vec()).unwrap())
            .collect()
    }

    #[test]
    fn status_selection_keeps_rename_endpoints_on_their_own_side() {
        let object = "a".repeat(40);
        let bytes = format!("# branch.oid {object}\0# branch.head main\02 RM N... 100644 100644 100644 {object} {object} R100 new.txt\0old.txt\0? old.txt\0? folder/\0! ignored.txt\0");
        let (branch, entries) = crate::status::parse_porcelain_v2(bytes.as_bytes()).unwrap();
        let status = crate::status::RepositoryStatus::from_parts(branch, entries, vec![]);
        assert_eq!(
            status_paths(&status, IndexSelectionSide::Staged).unwrap(),
            paths(&[b"new.txt", b"old.txt"])
        );
        assert_eq!(
            status_paths(&status, IndexSelectionSide::Unstaged).unwrap(),
            paths(&[b"folder", b"new.txt", b"old.txt"])
        );
        let only_rename = crate::status::RepositoryStatus::from_parts(
            status.branch.clone(),
            vec![status.entries[0].clone()],
            vec![],
        );
        assert_eq!(
            status_paths(&only_rename, IndexSelectionSide::Unstaged).unwrap(),
            paths(&[b"new.txt"])
        );
        assert_eq!(
            selected_status_paths(&status, IndexSelectionSide::Staged, &paths(&[b"new.txt"]))
                .unwrap(),
            paths(&[b"new.txt", b"old.txt"])
        );
        assert_eq!(
            selected_status_paths(&status, IndexSelectionSide::Unstaged, &paths(&[b"new.txt"]))
                .unwrap(),
            paths(&[b"new.txt"])
        );
        assert!(
            selected_status_paths(&status, IndexSelectionSide::Staged, &paths(&[b"old.txt"]))
                .is_err()
        );
    }

    #[test]
    fn argument_and_stdin_forms_select_the_same_paths() {
        let selection = paths(&[b"a.txt", b"dir/b c.txt"]);
        let arguments = stage_paths(&selection, PathspecForm::Arguments).unwrap();
        assert_eq!(
            arguments.display_arguments(),
            ["add", "-A", "--", "<path>", "<path>"]
        );
        assert_eq!(arguments.argv()[4], "dir/b c.txt");
        let stdin = unstage_paths(&selection, UnstageBase::Head, PathspecForm::StdinNul).unwrap();
        assert_eq!(
            stdin.display_arguments(),
            [
                "reset",
                "-q",
                "HEAD",
                "--pathspec-from-file=-",
                "--pathspec-file-nul"
            ]
        );
        assert_eq!(stdin.stdin().unwrap(), b"a.txt\0dir/b c.txt\0");
        assert_eq!(
            unstage_paths(&selection, UnstageBase::Unborn, PathspecForm::Arguments)
                .unwrap()
                .display_arguments()[..5],
            ["rm", "--cached", "-r", "-f", "-q"]
        );
        assert!(stage_paths(&[], PathspecForm::Arguments).is_err());
        assert_eq!(arguments.environment()[0].name(), "GIT_LITERAL_PATHSPECS");
    }
}
