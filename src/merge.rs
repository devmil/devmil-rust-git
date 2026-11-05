//! Git sequencer markers and tracked-dirty status projection.

use crate::diff::model::DiffPath;

pub mod markers;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirtyTreeParseError;
impl std::fmt::Display for DirtyTreeParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Git status porcelain-v2 output is malformed")
    }
}
impl std::error::Error for DirtyTreeParseError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SequencerOperation {
    Merge,
    CherryPick,
    Revert,
}

impl SequencerOperation {
    /// The Git subcommand that starts and resolves this sequencer.
    #[must_use]
    pub const fn git_command(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
        }
    }

    /// The per-worktree marker Git maintains while this sequencer is in progress.
    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Merge => "MERGE_HEAD",
            Self::CherryPick => "CHERRY_PICK_HEAD",
            Self::Revert => "REVERT_HEAD",
        }
    }
}

/// Which of this family's sequencers are currently in progress, discovered from the
/// per-worktree Git markers.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InProgressOperations {
    pub merge: bool,
    pub cherry_pick: bool,
    pub revert: bool,
}

impl InProgressOperations {
    pub const NONE: Self = Self {
        merge: false,
        cherry_pick: false,
        revert: false,
    };

    #[must_use]
    pub const fn any(self) -> bool {
        self.merge || self.cherry_pick || self.revert
    }

    #[must_use]
    pub const fn contains(self, operation: SequencerOperation) -> bool {
        match operation {
            SequencerOperation::Merge => self.merge,
            SequencerOperation::CherryPick => self.cherry_pick,
            SequencerOperation::Revert => self.revert,
        }
    }
}

/// The tracked-dirty content of one `git status --porcelain=v2 -z --untracked-files=no`
/// response: every ordinary, rename, and unmerged path (snapshot scope and dirty-tree
/// revalidation) plus the unmerged subset (conflict outcome reporting).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TrackedDirtyTree {
    dirty: Vec<DiffPath>,
    unmerged: Vec<DiffPath>,
}

impl TrackedDirtyTree {
    /// Every dirty tracked path, including both sides of a staged rename.
    #[must_use]
    pub fn dirty(&self) -> &[DiffPath] {
        &self.dirty
    }

    /// The unmerged (conflicted) subset of [`Self::dirty`].
    #[must_use]
    pub fn unmerged(&self) -> &[DiffPath] {
        &self.unmerged
    }

    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.dirty.is_empty()
    }
}

fn parse_error() -> DirtyTreeParseError {
    DirtyTreeParseError
}

fn record_path(record: &[u8], fields: usize) -> Result<DiffPath, DirtyTreeParseError> {
    let parts: Vec<&[u8]> = record.splitn(fields, |byte| *byte == b' ').collect();
    if parts.len() != fields {
        return Err(parse_error());
    }
    DiffPath::from_bytes(parts[fields - 1].to_vec()).map_err(|_| parse_error())
}

/// Parses the minimal tracked-dirty projection of a porcelain-v2 `-z` status response.
///
/// Untracked and ignored records are tolerated (and never produced with
/// `--untracked-files=no`). Rename records consume their NUL-separated source entry so the
/// source is never misread as a new record. Any malformed record fails closed with
/// [`DirtyTreeParseError`].
///
/// # Errors
///
/// Returns [`DirtyTreeParseError`] for malformed ordinary, rename, or unmerged records.
pub fn parse_tracked_dirty_porcelain_v2(
    bytes: &[u8],
) -> Result<TrackedDirtyTree, DirtyTreeParseError> {
    let mut tree = TrackedDirtyTree::default();
    let mut records = bytes.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        let Some(&kind) = record.first() else {
            continue;
        };
        if kind == b'#' {
            continue;
        }
        match kind {
            b'1' => tree.dirty.push(record_path(record, 9)?),
            b'2' => {
                let path = record_path(record, 10)?;
                let source = records.next().ok_or_else(parse_error)?;
                if source.is_empty() {
                    return Err(parse_error());
                }
                tree.dirty.push(path);
                tree.dirty
                    .push(DiffPath::from_bytes(source.to_vec()).map_err(|_| parse_error())?);
            }
            b'u' => {
                let path = record_path(record, 11)?;
                tree.unmerged.push(path.clone());
                tree.dirty.push(path);
            }
            b'?' | b'!' => {}
            _ => return Err(parse_error()),
        }
    }
    Ok(tree)
}

use crate::activity::{ActivityArgument, ActivityCommand, ActivityEnvironment};
use crate::types::{ObjectId, WorktreeId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SequencerAction {
    Continue,
    Skip,
    Abort,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MergeMode {
    FastForwardIfPossible,
    FastForwardOnly,
    NoFastForward,
}

/// Builds a noninteractive merge argument vector for a validated revision.
pub fn merge_arguments(source: &str) -> Vec<String> {
    merge_arguments_with_mode(source, MergeMode::FastForwardIfPossible)
}

/// Builds a noninteractive merge argument vector with an explicit fast-forward policy.
pub fn merge_arguments_with_mode(source: &str, mode: MergeMode) -> Vec<String> {
    let mode = match mode {
        MergeMode::FastForwardIfPossible => "--ff",
        MergeMode::FastForwardOnly => "--ff-only",
        MergeMode::NoFastForward => "--no-ff",
    };
    vec![
        "merge".to_owned(),
        "--no-edit".to_owned(),
        mode.to_owned(),
        source.to_owned(),
    ]
}

/// Builds arguments preserving the caller's commit order.
pub fn cherry_pick_arguments(commits: &[ObjectId]) -> Vec<String> {
    let mut arguments = vec!["cherry-pick".to_owned()];
    arguments.extend(commits.iter().map(|oid| oid.as_str().to_owned()));
    arguments
}

/// Builds a noninteractive revert argument vector.
pub fn revert_arguments(commit: &ObjectId) -> Vec<String> {
    revert_many_arguments(std::slice::from_ref(commit))
}

/// Builds a noninteractive revert argument vector preserving caller order.
pub fn revert_many_arguments(commits: &[ObjectId]) -> Vec<String> {
    revert_many_arguments_with_mode(commits, false)
}

/// Builds a revert in caller order, optionally leaving the result in the index.
pub fn revert_many_arguments_with_mode(commits: &[ObjectId], no_commit: bool) -> Vec<String> {
    let mut arguments = vec![
        "revert".to_owned(),
        if no_commit {
            "--no-commit"
        } else {
            "--no-edit"
        }
        .to_owned(),
    ];
    arguments.extend(commits.iter().map(|commit| commit.as_str().to_owned()));
    arguments
}

/// Finish a resolved single-commit apply-only revert while retaining its index.
/// `revert --continue` creates a commit, even when the revert began with `--no-commit`.
/// Do not use this for a multi-commit sequence: quitting drops its remaining steps.
pub fn revert_no_commit_finish_arguments() -> Vec<String> {
    vec!["revert".to_owned(), "--quit".to_owned()]
}

/// Builds reset arguments. The caller owns confirmation and recovery policy.
pub fn reset_arguments(mode: ResetMode, target: &ObjectId) -> Vec<String> {
    let mode = match mode {
        ResetMode::Soft => "--soft",
        ResetMode::Mixed => "--mixed",
        ResetMode::Hard => "--hard",
    };
    vec![
        "reset".to_owned(),
        mode.to_owned(),
        target.as_str().to_owned(),
    ]
}

/// Builds arguments to resume or abandon a non-rebase sequencer.
pub fn sequencer_arguments(operation: SequencerOperation, action: SequencerAction) -> Vec<String> {
    let action = match action {
        SequencerAction::Continue => "--continue",
        SequencerAction::Skip => "--skip",
        SequencerAction::Abort => "--abort",
    };
    vec![operation.git_command().to_owned(), action.to_owned()]
}

fn visible_argument(value: String) -> ActivityArgument {
    ActivityArgument {
        display: value,
        redacted: false,
        native_lossless: true,
    }
}

/// Builds the exact preview/execution command: `git -C <worktree> <arguments...>`. Continue
/// commands pin `GIT_EDITOR=true` because `git merge --continue` may otherwise open an editor.
pub fn activity_command(
    repository: &WorktreeId,
    arguments: &[String],
    pin_editor: bool,
) -> ActivityCommand {
    let mut argv = vec![
        visible_argument("-C".to_owned()),
        visible_argument(
            repository
                .worktree_root()
                .as_path()
                .to_string_lossy()
                .into_owned(),
        ),
    ];
    argv.extend(arguments.iter().cloned().map(visible_argument));
    let environment = if pin_editor {
        vec![ActivityEnvironment {
            name: "GIT_EDITOR".to_owned(),
            display: "true".to_owned(),
            redacted: false,
        }]
    } else {
        Vec::new()
    };
    ActivityCommand {
        executable: "git".to_owned(),
        arguments: argv,
        environment,
        working_directory: None,
        stdin_bytes: 0,
    }
}

#[cfg(test)]
mod command_tests {
    use super::*;

    #[test]
    fn command_vectors_preserve_commit_order_and_noninteractive_flags() {
        let first = ObjectId::parse("1".repeat(40)).unwrap();
        let second = ObjectId::parse("2".repeat(40)).unwrap();
        assert_eq!(
            cherry_pick_arguments(&[second.clone(), first.clone()]),
            vec!["cherry-pick", second.as_str(), first.as_str()]
        );
        assert_eq!(
            merge_arguments("refs/heads/topic"),
            vec!["merge", "--no-edit", "--ff", "refs/heads/topic"]
        );
        assert_eq!(
            merge_arguments_with_mode("refs/heads/topic", MergeMode::NoFastForward),
            vec!["merge", "--no-edit", "--no-ff", "refs/heads/topic"]
        );
        assert_eq!(
            revert_arguments(&first),
            vec!["revert", "--no-edit", first.as_str()]
        );
        assert_eq!(
            revert_many_arguments(&[second.clone(), first.clone()]),
            vec!["revert", "--no-edit", second.as_str(), first.as_str()]
        );
        assert_eq!(
            revert_many_arguments_with_mode(&[second.clone(), first.clone()], true),
            vec!["revert", "--no-commit", second.as_str(), first.as_str()]
        );
        assert_eq!(revert_no_commit_finish_arguments(), ["revert", "--quit"]);
        assert_eq!(
            reset_arguments(ResetMode::Hard, &first),
            vec!["reset", "--hard", first.as_str()]
        );
        assert_eq!(
            sequencer_arguments(SequencerOperation::CherryPick, SequencerAction::Continue),
            vec!["cherry-pick", "--continue"]
        );
        assert_eq!(
            sequencer_arguments(SequencerOperation::CherryPick, SequencerAction::Skip),
            vec!["cherry-pick", "--skip"]
        );
    }
}
