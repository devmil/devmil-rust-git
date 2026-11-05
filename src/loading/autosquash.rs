//! Bounded commit evidence and local upstream-chain queries.
use super::CommandExecutor;
use crate::{
    activity::{ActivityRecord, ActivityStatus},
    cancellation::Cancellation,
    process::{GitProcessCommand, ProcessArgument, SemanticTimeout},
    types::{ObjectId, RefId, RepositoryRelativePath, WorktreeId},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fmt,
    time::Duration,
};
pub const MAX_STACK_BRANCHES: usize = 128;
const OUTPUT_LIMIT: usize = 2 * 1024 * 1024;
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackedBranch {
    pub name: RefId,
    pub head: ObjectId,
    pub upstream: Option<RefId>,
    pub fork_point: Option<ObjectId>,
    pub published: bool,
    pub depth: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutosquashErrorKind {
    InvalidRequest,
    Canceled,
    TimedOut,
    GitFailed,
    MalformedOutput,
    ResourceLimit,
    PublicationConfirmationRequired,
    StalePlan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutosquashError {
    pub kind: AutosquashErrorKind,
    pub detail: String,
}

impl AutosquashError {
    #[must_use]
    pub fn new(kind: AutosquashErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for AutosquashError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for AutosquashError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitEvidence {
    pub commit: ObjectId,
    pub subject: String,
    pub activity: ActivityRecord,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceQuery {
    pub revision: Option<ObjectId>,
    pub path: Option<RepositoryRelativePath>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackDiscovery {
    pub branches: Vec<StackedBranch>,
    pub incomplete: bool,
    pub activities: Vec<ActivityRecord>,
}
pub struct AutosquashLoader<'a> {
    executable: OsString,
    executor: &'a mut dyn CommandExecutor,
}
impl<'a> AutosquashLoader<'a> {
    pub fn new(executable: impl Into<OsString>, executor: &'a mut dyn CommandExecutor) -> Self {
        Self {
            executable: executable.into(),
            executor,
        }
    }
    fn run(
        &mut self,
        worktree: &WorktreeId,
        arguments: Vec<ProcessArgument>,
        cancellation: &dyn Cancellation,
        timeout: Duration,
    ) -> Result<(ActivityRecord, Vec<u8>, Vec<u8>), AutosquashError> {
        if cancellation.is_cancelled() {
            return Err(error(
                AutosquashErrorKind::Canceled,
                "operation was canceled",
            ));
        }
        let command = GitProcessCommand::git(self.executable.clone())
            .current_dir(worktree.worktree_root().as_path())
            .args(arguments)
            .output_limit(OUTPUT_LIMIT)
            .output_idle_timeout(
                SemanticTimeout::new(
                    timeout,
                    "Git stopped producing output during autosquash or restack",
                )
                .expect("constant timeout is valid"),
            )
            .env("LC_ALL", "C")
            .expect("constant environment is valid")
            .redact_output();
        let mut result = self.executor.execute(command);
        if cancellation.is_cancelled() {
            return Err(error(
                AutosquashErrorKind::Canceled,
                "operation was canceled",
            ));
        }
        let truncated = result.stdout.truncated || result.stderr.truncated;
        let stdout = result.stdout.take_bytes();
        let stderr = result.stderr.take_bytes();
        if truncated {
            return Err(error(
                AutosquashErrorKind::ResourceLimit,
                "Git output exceeded the autosquash resource bound",
            ));
        }
        match result.activity.status {
            ActivityStatus::Canceled { .. } => Err(error(
                AutosquashErrorKind::Canceled,
                "operation was canceled",
            )),
            ActivityStatus::TimedOut { .. } => Err(error(
                AutosquashErrorKind::TimedOut,
                "operation exceeded its semantic timeout",
            )),
            _ => Ok((result.activity, stdout, stderr)),
        }
    }

    /// Loads the last commit touching one path or the exact selected revision.
    /// # Errors
    /// Reports cancellation, command failure, invalid output or bounds.
    pub fn evidence(
        &mut self,
        worktree: &WorktreeId,
        query: &EvidenceQuery,
        cancellation: &dyn Cancellation,
    ) -> Result<CommitEvidence, AutosquashError> {
        let revision = query.revision.as_ref().map_or("HEAD", ObjectId::as_str);
        let path = query.path.as_ref();
        let mut arguments = vec![
            ProcessArgument::visible("log"),
            ProcessArgument::visible("-1"),
            ProcessArgument::visible("--no-decorate"),
            ProcessArgument::visible("--format=%H%x00%s"),
            ProcessArgument::visible(revision),
        ];
        if let Some(path) = path {
            arguments.push(ProcessArgument::visible("--"));
            arguments.push(ProcessArgument::visible(path.as_path().as_os_str()));
        }
        let (activity, stdout, stderr) =
            self.run(worktree, arguments, cancellation, Duration::from_secs(20))?;
        if !succeeded(&activity.status) {
            return Err(error(
                AutosquashErrorKind::GitFailed,
                bounded_error(&stderr),
            ));
        }
        let (commit, subject) = parse_commit_subject(&stdout)?;
        Ok(CommitEvidence {
            commit,
            subject,
            activity,
        })
    }
}
impl AutosquashLoader<'_> {
    /// Discovers bounded local upstream chains and merge bases.
    /// # Errors
    /// Reports cancellation, malformed topology, command failure or resource bounds.
    pub fn discover(
        &mut self,
        worktree: &WorktreeId,
        cancellation: &dyn Cancellation,
    ) -> Result<StackDiscovery, AutosquashError> {
        let (activity, stdout, stderr) = self.run(
            worktree,
            vec![
                ProcessArgument::visible("for-each-ref"),
                ProcessArgument::visible(
                    "--format=%(refname)%00%(objectname)%00%(upstream)%00%(upstream:track)%00",
                ),
                ProcessArgument::visible("refs/heads"),
            ],
            cancellation,
            Duration::from_secs(20),
        )?;
        if !succeeded(&activity.status) {
            return Err(error(
                AutosquashErrorKind::GitFailed,
                bounded_error(&stderr),
            ));
        }
        let mut raw = parse_branches(&stdout)?;
        let mut activities = vec![activity];
        let incomplete = raw.len() > MAX_STACK_BRANCHES;
        raw.truncate(MAX_STACK_BRANCHES);
        let names = raw
            .iter()
            .map(|branch| branch.name.clone())
            .collect::<BTreeSet<_>>();
        let upstreams = raw
            .iter()
            .map(|branch| (branch.name.clone(), branch.upstream.clone()))
            .collect::<BTreeMap<_, _>>();
        for branch in &mut raw {
            branch.depth = branch_depth(&branch.name, &upstreams, &names)?;
            if let Some(upstream) = branch
                .upstream
                .as_ref()
                .filter(|upstream| names.contains(*upstream))
            {
                let (merge_activity, merge_stdout, merge_stderr) = self.run(
                    worktree,
                    vec![
                        ProcessArgument::visible("merge-base"),
                        ProcessArgument::visible(ref_text(&branch.name)?),
                        ProcessArgument::visible(ref_text(upstream)?),
                    ],
                    cancellation,
                    Duration::from_secs(20),
                )?;
                if !succeeded(&merge_activity.status) {
                    return Err(error(
                        AutosquashErrorKind::GitFailed,
                        bounded_error(&merge_stderr),
                    ));
                }
                branch.fork_point = Some(parse_oid(&merge_stdout)?);
                activities.push(merge_activity);
            }
        }
        raw.sort_by(|left, right| {
            left.depth
                .cmp(&right.depth)
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(StackDiscovery {
            branches: raw,
            incomplete,
            activities,
        })
    }
}
fn parse_commit_subject(bytes: &[u8]) -> Result<(ObjectId, String), AutosquashError> {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let separator = bytes.iter().position(|byte| *byte == 0).ok_or_else(|| {
        error(
            AutosquashErrorKind::MalformedOutput,
            "commit evidence omitted its NUL separator",
        )
    })?;
    let (oid, subject) = (&bytes[..separator], &bytes[separator + 1..]);
    let oid = parse_oid(oid)?;
    let subject = String::from_utf8(subject.to_vec()).map_err(|_| {
        error(
            AutosquashErrorKind::MalformedOutput,
            "commit subject is not UTF-8",
        )
    })?;
    Ok((oid, subject))
}

fn parse_oid(bytes: &[u8]) -> Result<ObjectId, AutosquashError> {
    let text = std::str::from_utf8(bytes).map(str::trim).map_err(|_| {
        error(
            AutosquashErrorKind::MalformedOutput,
            "object ID is not UTF-8",
        )
    })?;
    ObjectId::parse(text).map_err(|_| {
        error(
            AutosquashErrorKind::MalformedOutput,
            "Git returned an invalid object ID",
        )
    })
}

fn parse_branches(bytes: &[u8]) -> Result<Vec<StackedBranch>, AutosquashError> {
    let mut branches = Vec::new();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let mut fields = line.splitn(5, |byte| *byte == 0);
        let name = parse_ref(fields.next().ok_or_else(parse_error)?)?;
        let head = parse_oid(fields.next().ok_or_else(parse_error)?)?;
        let upstream_bytes = fields.next().ok_or_else(parse_error)?;
        let upstream = if upstream_bytes.is_empty() {
            None
        } else {
            Some(parse_ref(upstream_bytes)?)
        };
        let track = fields.next().ok_or_else(parse_error)?;
        let _tail = fields.next().ok_or_else(parse_error)?;
        let published = upstream
            .as_ref()
            .is_some_and(|value| value.as_bytes().starts_with(b"refs/remotes/"))
            || !track.is_empty();
        branches.push(StackedBranch {
            name,
            head,
            upstream,
            fork_point: None,
            published,
            depth: 0,
        });
    }
    Ok(branches)
}

fn branch_depth(
    branch: &RefId,
    upstreams: &BTreeMap<RefId, Option<RefId>>,
    local: &BTreeSet<RefId>,
) -> Result<usize, AutosquashError> {
    let mut current = branch;
    let mut visited = BTreeSet::new();
    let mut depth = 0usize;
    while let Some(Some(upstream)) = upstreams.get(current) {
        if !local.contains(upstream) {
            break;
        }
        if !visited.insert(current.clone()) {
            return Err(error(
                AutosquashErrorKind::MalformedOutput,
                "local branch upstreams form a cycle",
            ));
        }
        depth = depth.saturating_add(1);
        current = upstream;
    }
    Ok(depth)
}

fn parse_ref(bytes: &[u8]) -> Result<RefId, AutosquashError> {
    let value = std::str::from_utf8(bytes).map_err(|_| parse_error())?;
    RefId::parse(value).map_err(|_| parse_error())
}

fn ref_text(reference: &RefId) -> Result<&str, AutosquashError> {
    std::str::from_utf8(reference.as_bytes()).map_err(|_| {
        error(
            AutosquashErrorKind::InvalidRequest,
            "restack branch names must be UTF-8 on this process boundary",
        )
    })
}

fn parse_error() -> AutosquashError {
    error(
        AutosquashErrorKind::MalformedOutput,
        "Git returned malformed stacked-branch data",
    )
}

fn succeeded(status: &ActivityStatus) -> bool {
    matches!(status, ActivityStatus::Succeeded { .. })
}

fn bounded_error(bytes: &[u8]) -> String {
    let end = bytes.len().min(512);
    String::from_utf8_lossy(&bytes[..end]).trim().to_owned()
}

fn error(kind: AutosquashErrorKind, detail: impl Into<String>) -> AutosquashError {
    AutosquashError::new(kind, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nul_delimited_branch_stack_and_rejects_cycles() {
        let bytes = b"refs/heads/base\0\
1111111111111111111111111111111111111111\0refs/remotes/origin/base\0[ahead 1]\0\n\
refs/heads/topic\0\
2222222222222222222222222222222222222222\0refs/heads/base\0\0\n";
        let branches = parse_branches(bytes).unwrap();
        let names = branches
            .iter()
            .map(|branch| branch.name.clone())
            .collect::<BTreeSet<_>>();
        let upstreams = branches
            .iter()
            .map(|branch| (branch.name.clone(), branch.upstream.clone()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            branch_depth(&branches[1].name, &upstreams, &names).unwrap(),
            1
        );

        let mut cycle = upstreams;
        cycle.insert(branches[0].name.clone(), Some(branches[1].name.clone()));
        assert_eq!(
            branch_depth(&branches[0].name, &cycle, &names)
                .unwrap_err()
                .kind,
            AutosquashErrorKind::MalformedOutput
        );
    }
}
