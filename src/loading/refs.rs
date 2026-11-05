//! Reference snapshot loading, including remote configuration, stashes and linked worktrees.
use super::CommandExecutor;
use crate::activity::{ActivityRecord, ActivityStatus};
use crate::process::{GitProcessCommand, PrivateActivityBytes, ProcessArgument};
use crate::refs::{
    parse_branch_upstreams, parse_references, parse_remotes, parse_stashes, parse_worktrees,
    BranchUpstream, LinkedWorktree, Remote, RepositoryReference, StashEntry, REF_FORMAT,
    UPSTREAM_REMOTE_FORMAT,
};
use crate::types::{RefId, WorktreeId};
use std::ffi::OsString;
use std::path::Path;
const OUTPUT_LIMIT: usize = 32 * 1024 * 1024;
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RefsSection {
    Branches,
    Remotes,
    Tags,
    Stashes,
    Worktrees,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RefsSectionData {
    Branches {
        head_symbolic: Option<RefId>,
        rows: Vec<RepositoryReference>,
    },
    Remotes {
        references: Vec<RepositoryReference>,
        remotes: Vec<Remote>,
    },
    Tags(Vec<RepositoryReference>),
    Stashes(Vec<StashEntry>),
    Worktrees(Vec<LinkedWorktree>),
}

impl RefsSectionData {
    pub const fn section(&self) -> RefsSection {
        match self {
            Self::Branches { .. } => RefsSection::Branches,
            Self::Remotes { .. } => RefsSection::Remotes,
            Self::Tags(_) => RefsSection::Tags,
            Self::Stashes(_) => RefsSection::Stashes,
            Self::Worktrees(_) => RefsSection::Worktrees,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefsFailureKind {
    MissingGit,
    GitFailed,
    InvalidOutput,
    Canceled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefsRequest {
    pub worktree: WorktreeId,
    pub include_secondary: bool,
    pub head_symbolic_hint: Option<RefId>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefsResult {
    pub updates: Vec<RefsSectionData>,
    pub activities: Vec<ActivityRecord>,
    pub failure: Option<RefsFailureKind>,
}
#[derive(Debug)]
pub struct ReferenceLoader {
    executable: OsString,
}
impl ReferenceLoader {
    pub fn new(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
        }
    }
    fn command(
        &self,
        directory: &Path,
        arguments: impl IntoIterator<Item = ProcessArgument>,
    ) -> GitProcessCommand {
        GitProcessCommand::git(self.executable.clone())
            .current_dir(directory)
            .args(arguments)
            .output_limit(OUTPUT_LIMIT)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .expect("constant name")
            .env("LC_ALL", "C")
            .expect("constant name")
    }
    fn run(
        &self,
        directory: &Path,
        arguments: impl IntoIterator<Item = ProcessArgument>,
        executor: &mut dyn CommandExecutor,
    ) -> ActivityRecord {
        executor
            .execute(self.command(directory, arguments))
            .activity
    }
    fn run_private_stdout(
        &self,
        directory: &Path,
        arguments: impl IntoIterator<Item = ProcessArgument>,
        executor: &mut dyn CommandExecutor,
    ) -> (ActivityRecord, PrivateActivityBytes) {
        let result = executor.execute(self.command(directory, arguments).redact_stdout());
        (result.activity, result.stdout)
    }
}
impl ReferenceLoader {
    /// Lists every local branch's configured upstream remote and remote ref with
    /// `git for-each-ref --format=<UPSTREAM_REMOTE_FORMAT> refs/heads/`.
    ///
    /// # Errors
    /// Returns a typed launch, cancellation, Git or malformed-output failure.
    pub fn branch_upstreams(
        &self,
        worktree: &Path,
        executor: &mut dyn CommandExecutor,
    ) -> Result<Vec<BranchUpstream>, RefsFailureKind> {
        let result = executor.execute(
            self.command(
                worktree,
                [
                    ProcessArgument::visible("for-each-ref"),
                    ProcessArgument::visible(format!("--format={UPSTREAM_REMOTE_FORMAT}")),
                    ProcessArgument::visible("refs/heads/"),
                ],
            )
            .redact_stdout(),
        );
        match result.activity.status {
            ActivityStatus::Succeeded { .. } if !result.stdout.truncated => {}
            ActivityStatus::Succeeded { .. } => return Err(RefsFailureKind::InvalidOutput),
            ActivityStatus::Canceled { .. } => return Err(RefsFailureKind::Canceled),
            ActivityStatus::LaunchError { .. } => return Err(RefsFailureKind::MissingGit),
            _ => return Err(RefsFailureKind::GitFailed),
        }
        parse_branch_upstreams(&result.stdout.bytes).map_err(|_| RefsFailureKind::InvalidOutput)
    }
}

impl ReferenceLoader {
    pub fn query(&self, request: &RefsRequest, executor: &mut dyn CommandExecutor) -> RefsResult {
        self.query_inner(request, executor, false)
    }

    /// Loads bare repository references without invoking worktree-only stash commands.
    pub fn query_bare(
        &self,
        request: &RefsRequest,
        executor: &mut dyn CommandExecutor,
    ) -> RefsResult {
        self.query_inner(request, executor, true)
    }

    fn query_inner(
        &self,
        request: &RefsRequest,
        executor: &mut dyn CommandExecutor,
        bare: bool,
    ) -> RefsResult {
        let directory = request.worktree.worktree_root().as_path();
        let mut activities = Vec::new();
        let mut updates = Vec::new();

        let refs = self.run(
            directory,
            [
                ProcessArgument::visible("for-each-ref"),
                ProcessArgument::visible("--sort=refname"),
                ProcessArgument::visible(format!("--format={REF_FORMAT}")),
            ],
            executor,
        );
        let mut failure = failure_for(&refs);
        if failure.is_none() {
            self.append_reference_sections(
                request,
                executor,
                &refs,
                &mut updates,
                &mut activities,
                &mut failure,
            );
        }
        activities.insert(0, refs);

        if request.include_secondary {
            self.append_stashes_and_worktrees(
                request,
                executor,
                &mut updates,
                &mut activities,
                &mut failure,
                bare,
            );
        } else {
            updates.push(update(request, RefsSectionData::Stashes(Vec::new())));
            updates.push(update(request, RefsSectionData::Worktrees(Vec::new())));
        }

        publication(request, updates, activities, failure)
    }
}

impl ReferenceLoader {
    fn append_reference_sections(
        &self,
        request: &RefsRequest,
        executor: &mut dyn CommandExecutor,
        refs: &ActivityRecord,
        updates: &mut Vec<RefsSectionData>,
        activities: &mut Vec<ActivityRecord>,
        failure: &mut Option<RefsFailureKind>,
    ) {
        let Ok(references) = parse_references(&refs.stdout.bytes) else {
            *failure = Some(RefsFailureKind::InvalidOutput);
            return;
        };
        let head_symbolic = if request.include_secondary {
            let head = self.run(
                request.worktree.worktree_root().as_path(),
                [
                    ProcessArgument::visible("symbolic-ref"),
                    ProcessArgument::visible("--quiet"),
                    ProcessArgument::visible("HEAD"),
                ],
                executor,
            );
            let head_symbolic = successful_bytes(&head)
                .and_then(|bytes| RefId::from_bytes(trim_line(bytes).to_vec()).ok())
                .or_else(|| request.head_symbolic_hint.clone());
            if matches!(head.status, ActivityStatus::Canceled { .. }) {
                *failure = Some(RefsFailureKind::Canceled);
            }
            activities.push(head);
            head_symbolic
        } else {
            references
                .iter()
                .find(|reference| reference.is_head)
                .map(|reference| reference.id.clone())
                .or_else(|| request.head_symbolic_hint.clone())
        };
        updates.push(update(
            request,
            RefsSectionData::Branches {
                head_symbolic,
                rows: references
                    .iter()
                    .filter(|row| matches!(row.kind, crate::refs::ReferenceKind::LocalBranch))
                    .cloned()
                    .collect(),
            },
        ));
        if request.include_secondary {
            self.append_remote_section(
                request,
                executor,
                &references,
                updates,
                activities,
                failure,
            );
        } else {
            updates.push(update(
                request,
                RefsSectionData::Remotes {
                    references: references
                        .iter()
                        .filter(|row| matches!(row.kind, crate::refs::ReferenceKind::RemoteBranch))
                        .cloned()
                        .collect(),
                    remotes: Vec::new(),
                },
            ));
        }
        updates.push(update(
            request,
            RefsSectionData::Tags(
                references
                    .into_iter()
                    .filter(|row| matches!(row.kind, crate::refs::ReferenceKind::Tag))
                    .collect(),
            ),
        ));
    }

    fn append_remote_section(
        &self,
        request: &RefsRequest,
        executor: &mut dyn CommandExecutor,
        references: &[RepositoryReference],
        updates: &mut Vec<RefsSectionData>,
        activities: &mut Vec<ActivityRecord>,
        failure: &mut Option<RefsFailureKind>,
    ) {
        let (record, stdout) = self.run_private_stdout(
            request.worktree.worktree_root().as_path(),
            [
                ProcessArgument::visible("config"),
                ProcessArgument::visible("--null"),
                ProcessArgument::visible("--get-regexp"),
                ProcessArgument::visible(r"^remote\..*\.url$"),
            ],
            executor,
        );
        let succeeded = matches!(
            record.status,
            ActivityStatus::Succeeded { .. } | ActivityStatus::Failed { exit_code: Some(1) }
        ) && !stdout.truncated;
        let remotes = if succeeded {
            parse_remotes(&stdout.bytes).unwrap_or_else(|_| {
                *failure = Some(RefsFailureKind::InvalidOutput);
                Vec::new()
            })
        } else {
            *failure = failure.or_else(|| failure_for(&record));
            Vec::new()
        };
        activities.push(record);
        updates.push(update(
            request,
            RefsSectionData::Remotes {
                references: references
                    .iter()
                    .filter(|row| matches!(row.kind, crate::refs::ReferenceKind::RemoteBranch))
                    .cloned()
                    .collect(),
                remotes,
            },
        ));
    }
}

fn publication(
    _request: &RefsRequest,
    updates: Vec<RefsSectionData>,
    activities: Vec<ActivityRecord>,
    failure: Option<RefsFailureKind>,
) -> RefsResult {
    RefsResult {
        updates,
        activities,
        failure,
    }
}

impl ReferenceLoader {
    fn append_stashes_and_worktrees(
        &self,
        request: &RefsRequest,
        executor: &mut dyn CommandExecutor,
        updates: &mut Vec<RefsSectionData>,
        activities: &mut Vec<ActivityRecord>,
        failure: &mut Option<RefsFailureKind>,
        bare: bool,
    ) {
        if bare {
            updates.push(RefsSectionData::Stashes(Vec::new()));
            updates.push(RefsSectionData::Worktrees(Vec::new()));
            return;
        }
        if failure.is_none() {
            let stash = self.run(
                request.worktree.worktree_root().as_path(),
                crate::commands::stash::list_arguments()
                    .into_iter()
                    .map(ProcessArgument::visible),
                executor,
            );
            match failure_for(&stash) {
                Some(kind) => *failure = Some(kind),
                None => match parse_stashes(&stash.stdout.bytes) {
                    Ok(rows) => updates.push(update(request, RefsSectionData::Stashes(rows))),
                    Err(_) => *failure = Some(RefsFailureKind::InvalidOutput),
                },
            }
            activities.push(stash);
        }
        if failure.is_none() {
            let worktrees = self.run(
                request.worktree.worktree_root().as_path(),
                [
                    ProcessArgument::visible("worktree"),
                    ProcessArgument::visible("list"),
                    ProcessArgument::visible("--porcelain"),
                    ProcessArgument::visible("-z"),
                ],
                executor,
            );
            match failure_for(&worktrees) {
                Some(kind) => *failure = Some(kind),
                None => match parse_worktrees(&worktrees.stdout.bytes, &request.worktree) {
                    Ok(rows) => updates.push(update(request, RefsSectionData::Worktrees(rows))),
                    Err(_) => *failure = Some(RefsFailureKind::InvalidOutput),
                },
            }
            activities.push(worktrees);
        }
    }
}

fn update(_request: &RefsRequest, data: RefsSectionData) -> RefsSectionData {
    data
}

fn successful_bytes(record: &ActivityRecord) -> Option<&[u8]> {
    matches!(record.status, ActivityStatus::Succeeded { .. })
        .then_some(record.stdout.bytes.as_slice())
        .filter(|_| !record.stdout.truncated)
}

fn failure_for(record: &ActivityRecord) -> Option<RefsFailureKind> {
    if record.stdout.truncated {
        return Some(RefsFailureKind::InvalidOutput);
    }
    match record.status {
        ActivityStatus::Succeeded { .. } => None,
        ActivityStatus::Canceled { .. } => Some(RefsFailureKind::Canceled),
        ActivityStatus::LaunchError { .. } => Some(RefsFailureKind::MissingGit),
        _ => Some(RefsFailureKind::GitFailed),
    }
}

fn trim_line(mut value: &[u8]) -> &[u8] {
    while value
        .last()
        .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
    {
        value = &value[..value.len() - 1];
    }
    value
}
