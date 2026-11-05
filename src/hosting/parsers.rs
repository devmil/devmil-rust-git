//! Bounded provider response decoding.
use super::model::{
    CiStatus, HostingError, HostingIssue, HostingPullRequest, HostingRepository,
    HostingRepositoryId, IssueState, Mergeability, PullRequestState, MAX_HOSTING_ITEMS,
};
use super::providers::HOSTING_PAGE_SIZE;
use serde::Deserialize;
#[derive(Deserialize)]
struct GithubRepository {
    full_name: String,
    clone_url: String,
    html_url: String,
    private: bool,
}
#[derive(Deserialize)]
struct GitlabRepository {
    path_with_namespace: String,
    http_url_to_repo: String,
    web_url: String,
    visibility: String,
}
#[derive(Deserialize)]
struct ForgejoRepository {
    full_name: String,
    clone_url: String,
    html_url: String,
    private: bool,
}
#[derive(Deserialize)]
struct GithubPull {
    number: u64,
    title: String,
    draft: bool,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    merged_at: Option<String>,
    html_url: String,
    user: Login,
    head: GithubHead,
    base: GithubBase,
    /// Only a single pull request's response carries the two merge fields.
    #[serde(default)]
    mergeable: Option<bool>,
    #[serde(default)]
    mergeable_state: Option<String>,
}
#[derive(Deserialize)]
struct Login {
    login: String,
}
#[derive(Deserialize)]
struct GithubHead {
    #[serde(rename = "ref")]
    branch: String,
    sha: String,
    #[serde(default)]
    repo: Option<RepositoryIdentity>,
}
#[derive(Deserialize)]
struct GithubBase {
    #[serde(rename = "ref")]
    branch: String,
    #[serde(default)]
    repo: Option<RepositoryIdentity>,
}
#[derive(Deserialize)]
struct RepositoryIdentity {
    full_name: String,
}
#[derive(Deserialize)]
struct ForgejoPull {
    number: u64,
    title: String,
    draft: bool,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    merged: bool,
    html_url: String,
    user: Login,
    head: ForgejoBranch,
    base: ForgejoBranch,
    #[serde(default)]
    mergeable: Option<bool>,
}
#[derive(Deserialize)]
struct ForgejoBranch {
    #[serde(rename = "ref")]
    branch: String,
    #[serde(default)]
    label: Option<String>,
    sha: String,
    #[serde(default)]
    repo: Option<RepositoryIdentity>,
}

impl ForgejoBranch {
    /// Forgejo reports `refs/pull/N/head` as the ref once a pull request's branch is deleted, and
    /// for `AGit` pull requests, but keeps the branch in the label (`owner:branch` for forks).
    fn name(self) -> String {
        match self.label {
            Some(label) if self.branch.starts_with("refs/pull/") && !label.is_empty() => label
                .rsplit_once(':')
                .map_or_else(|| label.clone(), |(_, branch)| branch.to_owned()),
            _ => self.branch,
        }
    }
}
#[derive(Deserialize)]
struct ActionRun {
    #[serde(default)]
    head_sha: Option<String>,
    status: String,
    #[serde(default)]
    conclusion: Option<String>,
}
#[derive(Deserialize)]
struct ActionRuns {
    #[serde(default, alias = "workflow_runs")]
    workflow_runs: Vec<ActionRun>,
}
#[derive(Deserialize)]
struct GithubIssue {
    number: u64,
    title: String,
    state: String,
    html_url: String,
    user: Login,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}
#[derive(Deserialize)]
struct GitlabIssue {
    iid: u64,
    title: String,
    state: String,
    web_url: String,
    author: Username,
}
#[derive(Deserialize)]
struct ForgejoIssue {
    number: u64,
    title: String,
    state: String,
    html_url: String,
    user: Login,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}
#[derive(Deserialize)]
struct GitlabPull {
    iid: u64,
    title: String,
    draft: bool,
    #[serde(default)]
    state: Option<String>,
    web_url: String,
    author: Username,
    source_branch: String,
    target_branch: String,
    source_project_id: u64,
    target_project_id: u64,
    sha: String,
    #[serde(default, alias = "head_pipeline")]
    pipeline: Option<GitlabPipeline>,
    #[serde(default)]
    has_conflicts: bool,
    #[serde(default)]
    detailed_merge_status: Option<String>,
    #[serde(default)]
    merge_status: Option<String>,
}
#[derive(Deserialize)]
struct Username {
    username: String,
}
#[derive(Deserialize)]
struct GitlabPipeline {
    status: String,
}

#[derive(Deserialize)]
struct GitlabProjectPipeline {
    sha: String,
    status: String,
}

/// # Errors
/// Rejects malformed, unsafe, oversized, or unauthorized provider output.
pub fn parse_github_repositories(bytes: &[u8]) -> Result<Vec<HostingRepository>, HostingError> {
    let values: Vec<GithubRepository> =
        serde_json::from_slice(bytes).map_err(|_| HostingError::MalformedResponse)?;
    values
        .into_iter()
        .take(MAX_HOSTING_ITEMS)
        .map(|value| {
            Ok(HostingRepository {
                id: HostingRepositoryId::new(value.full_name.clone())?,
                full_name: value.full_name,
                clone_url: bounded_url(value.clone_url)?,
                web_url: bounded_url(value.html_url)?,
                private: value.private,
            })
        })
        .collect()
}
/// # Errors
/// Rejects malformed, unsafe, oversized, or unauthorized provider output.
pub fn parse_gitlab_repositories(bytes: &[u8]) -> Result<Vec<HostingRepository>, HostingError> {
    let values: Vec<GitlabRepository> =
        serde_json::from_slice(bytes).map_err(|_| HostingError::MalformedResponse)?;
    values
        .into_iter()
        .take(MAX_HOSTING_ITEMS)
        .map(|value| {
            Ok(HostingRepository {
                id: HostingRepositoryId::new(value.path_with_namespace.clone())?,
                full_name: value.path_with_namespace,
                clone_url: bounded_url(value.http_url_to_repo)?,
                web_url: bounded_url(value.web_url)?,
                private: value.visibility != "public",
            })
        })
        .collect()
}
/// # Errors
/// Rejects malformed, unsafe, oversized, or unauthorized provider output.
pub fn parse_forgejo_repositories(bytes: &[u8]) -> Result<Vec<HostingRepository>, HostingError> {
    let values: Vec<ForgejoRepository> =
        serde_json::from_slice(bytes).map_err(|_| HostingError::MalformedResponse)?;
    values
        .into_iter()
        .take(MAX_HOSTING_ITEMS)
        .map(|value| {
            Ok(HostingRepository {
                id: HostingRepositoryId::new(value.full_name.clone())?,
                full_name: value.full_name,
                clone_url: bounded_url(value.clone_url)?,
                web_url: bounded_url(value.html_url)?,
                private: value.private,
            })
        })
        .collect()
}
/// # Errors
/// Rejects malformed, unsafe, oversized, or unauthorized provider output.
pub fn parse_github_pull_requests(bytes: &[u8]) -> Result<Vec<HostingPullRequest>, HostingError> {
    let values: Vec<GithubPull> =
        serde_json::from_slice(bytes).map_err(|_| HostingError::MalformedResponse)?;
    values
        .into_iter()
        .take(MAX_HOSTING_ITEMS)
        .map(github_pull_request)
        .collect()
}
/// Decodes one pull request, as returned by `GET /repos/{repository}/pulls/{number}`.
///
/// # Errors
/// Rejects malformed, unsafe, oversized, or unauthorized provider output.
pub fn parse_github_pull_request(bytes: &[u8]) -> Result<HostingPullRequest, HostingError> {
    let value: GithubPull =
        serde_json::from_slice(bytes).map_err(|_| HostingError::MalformedResponse)?;
    github_pull_request(value)
}
/// Numbers of the pull requests in a GitHub issue listing, which mixes issues and pull requests.
///
/// # Errors
/// Rejects malformed provider output.
pub fn parse_github_issue_pull_numbers(bytes: &[u8]) -> Result<Vec<u64>, HostingError> {
    let values: Vec<GithubIssue> =
        serde_json::from_slice(bytes).map_err(|_| HostingError::MalformedResponse)?;
    values
        .into_iter()
        .take(MAX_HOSTING_ITEMS)
        .filter(|value| value.pull_request.is_some())
        .map(|value| {
            if value.number == 0 {
                Err(HostingError::MalformedResponse)
            } else {
                Ok(value.number)
            }
        })
        .collect()
}
fn github_pull_request(value: GithubPull) -> Result<HostingPullRequest, HostingError> {
    if value.number == 0 {
        return Err(HostingError::MalformedResponse);
    }
    let state = pull_request_state(
        value.draft,
        value.merged_at.is_some(),
        value.state.as_deref() == Some("closed"),
    );
    Ok(HostingPullRequest {
        number: value.number,
        title: bounded(value.title),
        author: bounded(value.user.login),
        source_repository: repository_identity(value.head.repo)?,
        source_branch: bounded(value.head.branch),
        target_repository: repository_identity(value.base.repo)?,
        target_branch: bounded(value.base.branch),
        head_sha: bounded(value.head.sha),
        web_url: bounded_url(value.html_url)?,
        state,
        ci_status: CiStatus::Unknown,
        mergeability: open_mergeability(
            state,
            github_mergeability(value.mergeable_state.as_deref(), value.mergeable),
        ),
    })
}
/// # Errors
/// Rejects malformed, unsafe, oversized, or unauthorized provider output.
pub fn parse_gitlab_pull_requests(
    bytes: &[u8],
    repository: &HostingRepositoryId,
) -> Result<Vec<HostingPullRequest>, HostingError> {
    let values: Vec<GitlabPull> =
        serde_json::from_slice(bytes).map_err(|_| HostingError::MalformedResponse)?;
    values
        .into_iter()
        .take(MAX_HOSTING_ITEMS)
        .map(|value| {
            if value.iid == 0 {
                return Err(HostingError::MalformedResponse);
            }
            let state = pull_request_state(
                value.draft,
                value.state.as_deref() == Some("merged"),
                matches!(value.state.as_deref(), Some("closed" | "locked")),
            );
            let mergeability = open_mergeability(
                state,
                gitlab_mergeability(
                    value.has_conflicts,
                    value.detailed_merge_status.as_deref(),
                    value.merge_status.as_deref(),
                ),
            );
            Ok(HostingPullRequest {
                number: value.iid,
                title: bounded(value.title),
                author: bounded(value.author.username),
                source_repository: (value.source_project_id == value.target_project_id)
                    .then(|| repository.clone()),
                source_branch: bounded(value.source_branch),
                target_repository: Some(repository.clone()),
                target_branch: bounded(value.target_branch),
                head_sha: bounded(value.sha),
                web_url: bounded_url(value.web_url)?,
                state,
                ci_status: value
                    .pipeline
                    .map_or(CiStatus::Unknown, |pipeline| map_ci(&pipeline.status)),
                mergeability,
            })
        })
        .collect()
}
/// # Errors
/// Rejects malformed, unsafe, oversized, or unauthorized provider output.
///
/// Forgejo lists a pull request it fails to load as `null`; such entries are skipped.
pub fn parse_forgejo_pull_requests(bytes: &[u8]) -> Result<Vec<HostingPullRequest>, HostingError> {
    let values: Vec<Option<ForgejoPull>> =
        serde_json::from_slice(bytes).map_err(|_| HostingError::MalformedResponse)?;
    values
        .into_iter()
        .flatten()
        .take(MAX_HOSTING_ITEMS)
        .map(|mut value| {
            if value.number == 0 {
                return Err(HostingError::MalformedResponse);
            }
            let state = pull_request_state(
                value.draft,
                value.merged,
                value.state.as_deref() == Some("closed"),
            );
            Ok(HostingPullRequest {
                number: value.number,
                title: bounded(value.title),
                author: bounded(value.user.login),
                source_repository: repository_identity(value.head.repo.take())?,
                head_sha: bounded(std::mem::take(&mut value.head.sha)),
                source_branch: bounded(value.head.name()),
                target_repository: repository_identity(value.base.repo)?,
                target_branch: bounded(value.base.branch),
                web_url: bounded_url(value.html_url)?,
                state,
                ci_status: CiStatus::Unknown,
                mergeability: open_mergeability(
                    state,
                    match value.mergeable {
                        Some(true) => Mergeability::Mergeable,
                        Some(false) => Mergeability::Conflicting,
                        None => Mergeability::Unknown,
                    },
                ),
            })
        })
        .collect()
}

/// Mergeability only describes a pull request that can still be merged.
const fn open_mergeability(state: PullRequestState, value: Mergeability) -> Mergeability {
    match state {
        PullRequestState::Open | PullRequestState::Draft => value,
        PullRequestState::Merged | PullRequestState::Closed => Mergeability::Unknown,
    }
}

/// GitHub reports `mergeable` and `mergeable_state` only for one pull request; `mergeable` stays
/// `null` and the state `unknown` while GitHub computes it.
fn github_mergeability(state: Option<&str>, mergeable: Option<bool>) -> Mergeability {
    match (state, mergeable) {
        (None, _) => Mergeability::Unknown,
        (Some("dirty"), _) | (_, Some(false)) => Mergeability::Conflicting,
        (_, Some(true)) => Mergeability::Mergeable,
        _ => Mergeability::Checking,
    }
}

/// GitLab 15.6+ reports `detailed_merge_status`; older servers only `merge_status`. Blockers
/// other than conflicts (approvals, discussions, pipelines) still merge without conflicts.
fn gitlab_mergeability(
    has_conflicts: bool,
    detailed: Option<&str>,
    legacy: Option<&str>,
) -> Mergeability {
    if has_conflicts {
        return Mergeability::Conflicting;
    }
    // Both fields share these values; the detailed one wins.
    match detailed.or(legacy) {
        None => Mergeability::Unknown,
        Some(
            "checking"
            | "unchecked"
            | "preparing"
            | "approvals_syncing"
            | "cannot_be_merged_recheck",
        ) => Mergeability::Checking,
        Some("conflict" | "broken_status" | "cannot_be_merged") => Mergeability::Conflicting,
        Some(_) => Mergeability::Mergeable,
    }
}

/// A response without state fields describes an open pull request, as every open listing does.
const fn pull_request_state(draft: bool, merged: bool, closed: bool) -> PullRequestState {
    if merged {
        PullRequestState::Merged
    } else if closed {
        PullRequestState::Closed
    } else if draft {
        PullRequestState::Draft
    } else {
        PullRequestState::Open
    }
}

fn repository_identity(
    repository: Option<RepositoryIdentity>,
) -> Result<Option<HostingRepositoryId>, HostingError> {
    repository
        .map(|repository| HostingRepositoryId::new(repository.full_name))
        .transpose()
}
/// # Errors
/// Rejects malformed, unsafe, oversized, or unauthorized provider output.
pub fn apply_action_run_statuses(
    pull_requests: &mut [HostingPullRequest],
    bytes: &[u8],
) -> Result<(), HostingError> {
    let runs: ActionRuns =
        serde_json::from_slice(bytes).map_err(|_| HostingError::MalformedResponse)?;
    for pull_request in pull_requests {
        let mut aggregate = CiStatus::Unknown;
        for run in runs
            .workflow_runs
            .iter()
            .filter(|run| run.head_sha.as_deref() == Some(pull_request.head_sha.as_str()))
            .take(MAX_HOSTING_ITEMS)
        {
            let run_status = if run.status == "completed" {
                run.conclusion.as_deref().map_or(CiStatus::Unknown, map_ci)
            } else {
                map_ci(&run.status)
            };
            aggregate = aggregate_ci(aggregate, run_status);
        }
        pull_request.ci_status = aggregate;
    }
    Ok(())
}

/// # Errors
/// Rejects malformed, unsafe, oversized, or unauthorized provider output.
pub fn apply_gitlab_pipeline_statuses(
    pull_requests: &mut [HostingPullRequest],
    bytes: &[u8],
) -> Result<(), HostingError> {
    let pipelines: Vec<GitlabProjectPipeline> =
        serde_json::from_slice(bytes).map_err(|_| HostingError::MalformedResponse)?;
    for pull_request in pull_requests
        .iter_mut()
        .filter(|pull_request| pull_request.ci_status == CiStatus::Unknown)
    {
        if let Some(pipeline) = pipelines
            .iter()
            .take(HOSTING_PAGE_SIZE)
            .find(|pipeline| pipeline.sha == pull_request.head_sha)
        {
            pull_request.ci_status = map_ci(&pipeline.status);
        }
    }
    Ok(())
}

fn aggregate_ci(current: CiStatus, next: CiStatus) -> CiStatus {
    match (current, next) {
        (_, CiStatus::Failing) | (CiStatus::Failing, _) => CiStatus::Failing,
        (_, CiStatus::Pending) | (CiStatus::Pending, _) => CiStatus::Pending,
        (_, CiStatus::Canceled) | (CiStatus::Canceled, _) => CiStatus::Canceled,
        (_, CiStatus::Passing) => CiStatus::Passing,
        (current, CiStatus::Unknown) => current,
    }
}
/// # Errors
/// Rejects malformed, unsafe, oversized, or unauthorized provider output.
pub fn parse_github_issues(bytes: &[u8]) -> Result<Vec<HostingIssue>, HostingError> {
    let values: Vec<GithubIssue> =
        serde_json::from_slice(bytes).map_err(|_| HostingError::MalformedResponse)?;
    values
        .into_iter()
        .filter(|value| value.pull_request.is_none())
        .take(MAX_HOSTING_ITEMS)
        .map(|value| {
            issue(
                value.number,
                value.title,
                value.user.login,
                value.html_url,
                &value.state,
            )
        })
        .collect()
}
/// # Errors
/// Rejects malformed, unsafe, oversized, or unauthorized provider output.
pub fn parse_gitlab_issues(bytes: &[u8]) -> Result<Vec<HostingIssue>, HostingError> {
    let values: Vec<GitlabIssue> =
        serde_json::from_slice(bytes).map_err(|_| HostingError::MalformedResponse)?;
    values
        .into_iter()
        .take(MAX_HOSTING_ITEMS)
        .map(|value| {
            issue(
                value.iid,
                value.title,
                value.author.username,
                value.web_url,
                &value.state,
            )
        })
        .collect()
}
/// # Errors
/// Rejects malformed, unsafe, oversized, or unauthorized provider output.
pub fn parse_forgejo_issues(bytes: &[u8]) -> Result<Vec<HostingIssue>, HostingError> {
    let values: Vec<ForgejoIssue> =
        serde_json::from_slice(bytes).map_err(|_| HostingError::MalformedResponse)?;
    values
        .into_iter()
        .filter(|value| value.pull_request.is_none())
        .take(MAX_HOSTING_ITEMS)
        .map(|value| {
            issue(
                value.number,
                value.title,
                value.user.login,
                value.html_url,
                &value.state,
            )
        })
        .collect()
}
fn issue(
    number: u64,
    title: String,
    author: String,
    web_url: String,
    state: &str,
) -> Result<HostingIssue, HostingError> {
    if number == 0 {
        return Err(HostingError::MalformedResponse);
    }
    let state = match state {
        "open" | "opened" => IssueState::Open,
        "closed" => IssueState::Closed,
        _ => return Err(HostingError::MalformedResponse),
    };
    Ok(HostingIssue {
        number,
        title: bounded(title),
        author: bounded(author),
        web_url: bounded_url(web_url)?,
        state,
    })
}
pub(crate) fn map_ci(value: &str) -> CiStatus {
    match value {
        "success" | "passed" => CiStatus::Passing,
        "failure" | "failed" | "error" => CiStatus::Failing,
        "pending" | "running" | "in_progress" | "queued" | "waiting" | "created" => {
            CiStatus::Pending
        }
        "canceled" | "cancelled" => CiStatus::Canceled,
        _ => CiStatus::Unknown,
    }
}
fn bounded(mut value: String) -> String {
    while value.len() > 512 {
        value.pop();
    }
    value
}
fn bounded_url(value: String) -> Result<String, HostingError> {
    if value.len() > 2_048 {
        return Err(HostingError::MalformedResponse);
    }
    let authority = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .and_then(|rest| rest.split('/').next())
        .filter(|authority| !authority.is_empty() && !authority.contains('@'))
        .ok_or(HostingError::MalformedResponse)?;
    if authority.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(HostingError::MalformedResponse);
    }
    Ok(value)
}
pub fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}
