//! Provider REST knowledge shared by the provider units: web links, pull-request heads, checks
//! and pipelines, and re-run requests. Every function only builds paths or parses bounded
//! responses; authorization, confirmation, execution and scheduling stay with the caller.

use super::model::{
    CiStatus, HostingCapability, HostingCapabilitySet, HostingConnection, HostingError,
    HostingProvider, HostingRepositoryId, MAX_HOSTING_ITEMS,
};
use super::parsers::percent_encode;
use super::providers::provider_plugin;
use crate::types::ObjectId;
use serde::Deserialize;

/// A page on the provider's website.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HostingWebResource {
    Repository,
    /// A pull or merge request by number.
    ChangeRequest(u64),
    /// The page that starts a new pull or merge request.
    CreateChangeRequest,
    Issue(u64),
}

/// Repository path segments accepted in web links: ASCII letters, digits, `-`, `_` and `.`,
/// never `.` or `..` alone.
#[must_use]
pub fn safe_path_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// The browser URL of `resource` in `repository` on `connection`'s website.
///
/// # Errors
/// Rejects an unsafe repository path, a zero number, or a connection without an `https` web base.
pub fn web_link(
    connection: &HostingConnection,
    repository: &HostingRepositoryId,
    resource: HostingWebResource,
) -> Result<String, HostingError> {
    if !connection.web_base_url.starts_with("https://")
        || connection.web_base_url.contains(['@', '?', '#'])
        || !repository.as_str().split('/').all(safe_path_segment)
    {
        return Err(HostingError::InvalidIdentity);
    }
    let suffix = provider_plugin(connection.provider())
        .web_path(resource)
        .ok_or(HostingError::InvalidIdentity)?;
    Ok(format!(
        "{}/{}{suffix}",
        connection.web_base_url.trim_end_matches('/'),
        repository.as_str()
    ))
}

/// The repository part of an API path: GitHub and Forgejo use `owner/name` with each segment
/// percent-encoded; GitLab uses the URL-encoded full project path.
///
/// # Errors
/// Rejects empty, `.`/`..` or (for GitHub and Forgejo) nested paths.
pub fn provider_repository_path(
    provider: HostingProvider,
    repository: &HostingRepositoryId,
) -> Result<String, HostingError> {
    let repository = repository.as_str();
    match provider {
        HostingProvider::GitHub | HostingProvider::Forgejo => {
            let mut components = repository.split('/');
            let owner = components.next().unwrap_or_default();
            let name = components.next().unwrap_or_default();
            if components.next().is_some()
                || owner.is_empty()
                || name.is_empty()
                || matches!(owner, "." | "..")
                || matches!(name, "." | "..")
            {
                return Err(HostingError::InvalidIdentity);
            }
            Ok(format!(
                "{}/{}",
                percent_encode(owner),
                percent_encode(name)
            ))
        }
        HostingProvider::GitLab => {
            if repository
                .split('/')
                .any(|component| component.is_empty() || matches!(component, "." | ".."))
            {
                return Err(HostingError::InvalidIdentity);
            }
            Ok(percent_encode(repository))
        }
    }
}

/// The state of one check run or pipeline. Unlike [`CiStatus`] it distinguishes skipped and
/// neutral results.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HostingCheckState {
    Pending,
    Passing,
    Failing,
    Canceled,
    Skipped,
    Unknown,
}

impl From<CiStatus> for HostingCheckState {
    fn from(status: CiStatus) -> Self {
        match status {
            CiStatus::Pending => Self::Pending,
            CiStatus::Passing => Self::Passing,
            CiStatus::Failing => Self::Failing,
            CiStatus::Canceled => Self::Canceled,
            CiStatus::Unknown => Self::Unknown,
        }
    }
}

/// Maps a provider's status and optional conclusion (the conclusion wins when present).
#[must_use]
pub fn check_state(status: &str, conclusion: Option<&str>) -> HostingCheckState {
    match conclusion.unwrap_or(status).to_ascii_lowercase().as_str() {
        "queued" | "in_progress" | "pending" | "running" | "created" => HostingCheckState::Pending,
        "success" | "passed" => HostingCheckState::Passing,
        "failure" | "failed" | "timed_out" | "action_required" => HostingCheckState::Failing,
        "cancelled" | "canceled" => HostingCheckState::Canceled,
        "skipped" | "neutral" => HostingCheckState::Skipped,
        _ => HostingCheckState::Unknown,
    }
}

/// One check run (GitHub) or pipeline (GitLab) for a commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostingCheck {
    /// `github-check:<id>` or `gitlab-pipeline:<id>`; pass it to
    /// [`super::providers::HostingProviderPlugin::check_rerun`].
    pub stable_id: String,
    pub name: String,
    pub state: HostingCheckState,
    pub conclusion: String,
    pub details_url: Option<String>,
}

/// A re-run request: `POST <path>` with the JSON body `{}`. `status_path` reads the run back
/// (`GET`) to reconcile an uncertain submission with [`rerun_in_progress`].
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct HostingCheckRerun {
    pub path: String,
    pub status_path: String,
}

#[derive(Deserialize)]
struct HeadObject {
    sha: String,
}

#[derive(Deserialize)]
struct GitHubPullHead {
    head: HeadObject,
}

/// Parses `{"head":{"sha":…}}` (GitHub, Forgejo) into the pull request's head commit.
///
/// # Errors
/// Returns [`HostingError::MalformedResponse`] for other shapes.
pub(crate) fn parse_nested_head(body: &[u8]) -> Result<ObjectId, HostingError> {
    let head: GitHubPullHead =
        serde_json::from_slice(body).map_err(|_| HostingError::MalformedResponse)?;
    ObjectId::parse(head.head.sha).map_err(|_| HostingError::MalformedResponse)
}

/// Parses `{"sha":…}` (GitLab merge request) into the head commit.
///
/// # Errors
/// Returns [`HostingError::MalformedResponse`] for other shapes.
pub(crate) fn parse_flat_head(body: &[u8]) -> Result<ObjectId, HostingError> {
    let head: HeadObject =
        serde_json::from_slice(body).map_err(|_| HostingError::MalformedResponse)?;
    ObjectId::parse(head.sha).map_err(|_| HostingError::MalformedResponse)
}

#[derive(Deserialize)]
struct GitHubCheckRuns {
    #[serde(default)]
    check_runs: Vec<GitHubCheckRun>,
}

#[derive(Deserialize)]
struct GitHubCheckRun {
    id: u64,
    name: String,
    status: String,
    conclusion: Option<String>,
    html_url: Option<String>,
}

#[derive(Deserialize)]
struct GitLabPipeline {
    id: u64,
    status: String,
    #[serde(default)]
    web_url: String,
}

pub(crate) fn parse_github_check_runs(body: &[u8]) -> Result<Vec<HostingCheck>, HostingError> {
    let parsed: GitHubCheckRuns =
        serde_json::from_slice(body).map_err(|_| HostingError::MalformedResponse)?;
    if parsed.check_runs.len() > MAX_HOSTING_ITEMS {
        return Err(HostingError::MalformedResponse);
    }
    Ok(parsed
        .check_runs
        .into_iter()
        .map(|check| HostingCheck {
            stable_id: format!("github-check:{}", check.id),
            name: check.name,
            state: check_state(&check.status, check.conclusion.as_deref()),
            conclusion: check.conclusion.unwrap_or_default(),
            details_url: check.html_url,
        })
        .collect())
}

pub(crate) fn parse_gitlab_pipelines(body: &[u8]) -> Result<Vec<HostingCheck>, HostingError> {
    let parsed: Vec<GitLabPipeline> =
        serde_json::from_slice(body).map_err(|_| HostingError::MalformedResponse)?;
    if parsed.len() > MAX_HOSTING_ITEMS {
        return Err(HostingError::MalformedResponse);
    }
    Ok(parsed
        .into_iter()
        .map(|pipeline| HostingCheck {
            stable_id: format!("gitlab-pipeline:{}", pipeline.id),
            name: format!("Pipeline {}", pipeline.id),
            state: check_state(&pipeline.status, Some(&pipeline.status)),
            conclusion: pipeline.status,
            details_url: (!pipeline.web_url.is_empty()).then_some(pipeline.web_url),
        })
        .collect())
}

#[derive(Deserialize)]
struct ForgejoCombinedStatus {
    #[serde(default)]
    statuses: Option<Vec<ForgejoCommitStatus>>,
}

#[derive(Deserialize)]
struct ForgejoCommitStatus {
    id: u64,
    status: String,
    #[serde(default)]
    context: String,
    #[serde(default)]
    target_url: Option<String>,
}

/// Parses Forgejo's combined commit status (`/commits/{sha}/status`) into one check per context.
pub(crate) fn parse_forgejo_commit_statuses(
    body: &[u8],
) -> Result<Vec<HostingCheck>, HostingError> {
    let parsed: ForgejoCombinedStatus =
        serde_json::from_slice(body).map_err(|_| HostingError::MalformedResponse)?;
    let statuses = parsed.statuses.unwrap_or_default();
    if statuses.len() > MAX_HOSTING_ITEMS {
        return Err(HostingError::MalformedResponse);
    }
    Ok(statuses
        .into_iter()
        .map(|status| HostingCheck {
            stable_id: format!("forgejo-status:{}", status.id),
            name: status.context,
            // Forgejo reports a job that could not run as `error`.
            state: match status.status.as_str() {
                "error" => HostingCheckState::Failing,
                other => check_state(other, None),
            },
            conclusion: status.status,
            details_url: status.target_url.filter(|url| !url.is_empty()),
        })
        .collect())
}

/// Extracts the positive numeric ID after `prefix` in a check stable ID.
pub(crate) fn stable_numeric_id(stable_id: &str, prefix: &str) -> Result<u64, HostingError> {
    stable_id
        .strip_prefix(prefix)
        .and_then(|id| id.parse::<u64>().ok())
        .filter(|id| *id > 0)
        .ok_or(HostingError::InvalidIdentity)
}

pub(crate) fn supports(capabilities: &HostingCapabilitySet, capability: HostingCapability) -> bool {
    capabilities.supports(capability)
}

#[derive(Deserialize)]
struct RunStatus {
    status: String,
}

/// Whether a run read back from [`HostingCheckRerun::status_path`] is queued or running again.
#[must_use]
pub fn rerun_in_progress(body: &[u8]) -> bool {
    serde_json::from_slice::<RunStatus>(body).is_ok_and(|status| {
        matches!(
            status.status.to_ascii_lowercase().as_str(),
            "queued" | "pending" | "in_progress" | "running" | "created"
        )
    })
}

/// Maps a provider CI string to [`CiStatus`], the rule used for pull-request lists.
#[must_use]
pub fn ci_status(value: &str) -> CiStatus {
    super::parsers::map_ci(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosting::model::{HostingConnectionId, HostingPreset};

    fn connection(preset: HostingPreset, web: &str) -> HostingConnection {
        HostingConnection {
            id: HostingConnectionId::new("work").unwrap(),
            preset,
            login: "me".into(),
            web_base_url: web.into(),
            api_base_url: format!("{web}/api"),
            oauth_client_id: String::new(),
            enabled: true,
            capabilities: HostingCapabilitySet::baseline(preset.provider()),
        }
    }

    #[test]
    fn web_links_follow_each_provider_layout() {
        let repository = HostingRepositoryId::new("team/project").unwrap();
        let cases = [
            (
                HostingPreset::GitHub,
                HostingWebResource::ChangeRequest(4),
                "/pull/4",
            ),
            (
                HostingPreset::GitHub,
                HostingWebResource::CreateChangeRequest,
                "/compare",
            ),
            (
                HostingPreset::GitLab,
                HostingWebResource::ChangeRequest(4),
                "/-/merge_requests/4",
            ),
            (
                HostingPreset::GitLab,
                HostingWebResource::CreateChangeRequest,
                "/-/merge_requests/new",
            ),
            (
                HostingPreset::GitLab,
                HostingWebResource::Issue(2),
                "/-/issues/2",
            ),
            (
                HostingPreset::Forgejo,
                HostingWebResource::ChangeRequest(4),
                "/pulls/4",
            ),
            (
                HostingPreset::Forgejo,
                HostingWebResource::Issue(2),
                "/issues/2",
            ),
            (HostingPreset::Codeberg, HostingWebResource::Repository, ""),
        ];
        for (preset, resource, suffix) in cases {
            assert_eq!(
                web_link(
                    &connection(preset, "https://host.test/"),
                    &repository,
                    resource
                )
                .unwrap(),
                format!("https://host.test/team/project{suffix}")
            );
        }
        assert!(web_link(
            &connection(HostingPreset::GitHub, "https://host.test"),
            &repository,
            HostingWebResource::Issue(0)
        )
        .is_err());
        assert!(web_link(
            &connection(HostingPreset::GitHub, "https://host.test"),
            &HostingRepositoryId::new("team/a:b").unwrap(),
            HostingWebResource::Repository
        )
        .is_err());
    }

    #[test]
    fn checks_heads_and_reruns_parse_provider_shapes() {
        let checks = parse_github_check_runs(
            br#"{"check_runs":[{"id":7,"name":"ci","status":"completed","conclusion":"skipped","html_url":null}]}"#,
        )
        .unwrap();
        assert_eq!(checks[0].stable_id, "github-check:7");
        assert_eq!(checks[0].state, HostingCheckState::Skipped);
        let pipelines =
            parse_gitlab_pipelines(br#"[{"id":9,"status":"running","web_url":"https://x"}]"#)
                .unwrap();
        assert_eq!(pipelines[0].name, "Pipeline 9");
        assert_eq!(pipelines[0].state, HostingCheckState::Pending);
        let sha = "a".repeat(40);
        assert_eq!(
            parse_nested_head(format!(r#"{{"head":{{"sha":"{sha}"}}}}"#).as_bytes())
                .unwrap()
                .as_str(),
            sha
        );
        assert_eq!(
            parse_flat_head(format!(r#"{{"sha":"{sha}"}}"#).as_bytes())
                .unwrap()
                .as_str(),
            sha
        );
        assert!(parse_flat_head(br#"{"sha":"short"}"#).is_err());
        assert!(rerun_in_progress(br#"{"status":"Queued"}"#));
        assert!(!rerun_in_progress(br#"{"status":"completed"}"#));
        assert_eq!(
            stable_numeric_id("github-check:0", "github-check:").ok(),
            None
        );
        assert_eq!(check_state("in_progress", None), HostingCheckState::Pending);
        assert_eq!(
            check_state("completed", Some("timed_out")),
            HostingCheckState::Failing
        );
        assert_eq!(
            HostingCheckState::from(ci_status("passed")),
            HostingCheckState::Passing
        );
        assert_eq!(
            provider_repository_path(
                HostingProvider::GitLab,
                &HostingRepositoryId::new("group/sub/project").unwrap()
            )
            .unwrap(),
            "group%2Fsub%2Fproject"
        );
    }
}
