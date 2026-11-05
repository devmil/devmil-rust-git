use super::{
    fill_ci_status, optional, status_applies, validate_branch_query, validate_login_query,
    DeviceAuthorization, HostingProviderPlugin, OAuthIdentityField, OAuthLoginFlow,
};
use super::{HostingProviderRequester, HOSTING_AUTHORED_LIMIT, HOSTING_PAGE_SIZE};
use crate::cancellation::Cancellation;
use crate::hosting::model::{
    HostingConnection, HostingCredential, HostingError, HostingIssue, HostingProvider,
    HostingPullRequest, HostingRepository, HostingRepositoryId, Mergeability, PullRequestState,
};
use crate::hosting::parsers::{
    apply_action_run_statuses, parse_github_issue_pull_numbers, parse_github_issues,
    parse_github_pull_request, parse_github_pull_requests, parse_github_repositories,
    percent_encode,
};

use crate::hosting::model::{HostingCapability, HostingCapabilitySet};
use crate::hosting::rest::{
    provider_repository_path, stable_numeric_id, supports, HostingCheck, HostingCheckRerun,
    HostingWebResource,
};
use crate::types::ObjectId;

pub(super) static GITHUB: GitHubProvider = GitHubProvider;

pub(super) struct GitHubProvider;

impl HostingProviderPlugin for GitHubProvider {
    fn provider(&self) -> HostingProvider {
        HostingProvider::GitHub
    }

    fn activity_name(&self) -> &'static str {
        "github"
    }

    fn api_authorization_header_prefix(&self) -> &'static str {
        "Authorization: Bearer "
    }

    fn pull_request_ref(&self, number: u64) -> String {
        format!("refs/pull/{number}/head")
    }

    fn login_flow(&self) -> OAuthLoginFlow {
        OAuthLoginFlow::DeviceAuthorization(DeviceAuthorization {
            device_code_path: "/login/device/code",
            verification_path: "/login/device",
            scopes: &["repo"],
        })
    }

    fn token_path(&self) -> &'static str {
        "/login/oauth/access_token"
    }

    fn identity_field(&self) -> OAuthIdentityField {
        OAuthIdentityField::Login
    }

    fn repositories(
        &self,
        requester: &dyn HostingProviderRequester,
        connection: &HostingConnection,
        credential: &HostingCredential,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<HostingRepository>, HostingError> {
        let response = requester.request(
            connection,
            format!("/user/repos?per_page={HOSTING_PAGE_SIZE}"),
            credential,
            cancellation,
        )?;
        parse_github_repositories(&response.body)
    }

    fn pull_requests(
        &self,
        requester: &dyn HostingProviderRequester,
        connection: &HostingConnection,
        repository: &HostingRepositoryId,
        credential: &HostingCredential,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<HostingPullRequest>, HostingError> {
        let response = requester.request(
            connection,
            format!(
                "/repos/{}/pulls?state=open&per_page={HOSTING_PAGE_SIZE}",
                repository.as_str()
            ),
            credential,
            cancellation,
        )?;
        let mut pull_requests = parse_github_pull_requests(&response.body)?;
        match requester.request(
            connection,
            format!(
                "/repos/{}/actions/runs?per_page={HOSTING_PAGE_SIZE}",
                repository.as_str()
            ),
            credential,
            cancellation,
        ) {
            Ok(runs) => {
                let _ = apply_action_run_statuses(&mut pull_requests, &runs.body);
            }
            Err(HostingError::Canceled) => return Err(HostingError::Canceled),
            Err(_) => {}
        }
        Ok(pull_requests)
    }

    fn issues(
        &self,
        requester: &dyn HostingProviderRequester,
        connection: &HostingConnection,
        repository: &HostingRepositoryId,
        credential: &HostingCredential,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<HostingIssue>, HostingError> {
        let response = requester.request(
            connection,
            format!(
                "/repos/{}/issues?state=all&per_page={HOSTING_PAGE_SIZE}",
                repository.as_str()
            ),
            credential,
            cancellation,
        )?;
        parse_github_issues(&response.body)
    }

    /// Uses the `head` filter, which names a branch as `owner:branch` in the repository's own
    /// namespace, so fork branches with the same name are not matched.
    fn branch_pull_requests(
        &self,
        requester: &dyn HostingProviderRequester,
        connection: &HostingConnection,
        repository: &HostingRepositoryId,
        branch: &str,
        credential: &HostingCredential,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<HostingPullRequest>, HostingError> {
        validate_branch_query(branch)?;
        let owner = repository
            .as_str()
            .split_once('/')
            .map(|(owner, _)| owner)
            .ok_or(HostingError::InvalidIdentity)?;
        let response = requester.request(
            connection,
            format!(
                "/repos/{}/pulls?state=all&head={}&per_page={HOSTING_PAGE_SIZE}",
                repository.as_str(),
                percent_encode(&format!("{owner}:{branch}"))
            ),
            credential,
            cancellation,
        )?;
        Ok(parse_github_pull_requests(&response.body)?
            .into_iter()
            .filter(|pull| pull.state != PullRequestState::Closed)
            .collect())
    }

    /// The pull-request list cannot filter by author, so the issue listing (which can) names
    /// the pull requests and each one is then read for its branches.
    fn authored_pull_requests(
        &self,
        requester: &dyn HostingProviderRequester,
        connection: &HostingConnection,
        repository: &HostingRepositoryId,
        login: &str,
        credential: &HostingCredential,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<HostingPullRequest>, HostingError> {
        validate_login_query(login)?;
        let response = requester.request(
            connection,
            format!(
                "/repos/{}/issues?state=open&creator={}&per_page={HOSTING_PAGE_SIZE}",
                repository.as_str(),
                percent_encode(login)
            ),
            credential,
            cancellation,
        )?;
        let mut pull_requests = Vec::new();
        for number in parse_github_issue_pull_numbers(&response.body)?
            .into_iter()
            .take(HOSTING_AUTHORED_LIMIT)
        {
            let response = requester.request(
                connection,
                format!("/repos/{}/pulls/{number}", repository.as_str()),
                credential,
                cancellation,
            )?;
            pull_requests.push(parse_github_pull_request(&response.body)?);
        }
        Ok(pull_requests)
    }

    fn web_path(&self, resource: HostingWebResource) -> Option<String> {
        match resource {
            HostingWebResource::Repository => Some(String::new()),
            HostingWebResource::ChangeRequest(number) if number > 0 => {
                Some(format!("/pull/{number}"))
            }
            HostingWebResource::CreateChangeRequest => Some("/compare".into()),
            HostingWebResource::Issue(number) if number > 0 => Some(format!("/issues/{number}")),
            _ => None,
        }
    }

    fn pull_request_head_path(
        &self,
        repository: &HostingRepositoryId,
        number: u64,
    ) -> Result<String, HostingError> {
        if number == 0 {
            return Err(HostingError::InvalidIdentity);
        }
        Ok(format!(
            "/repos/{}/pulls/{number}",
            provider_repository_path(HostingProvider::GitHub, repository)?
        ))
    }

    fn parse_pull_request_head(&self, body: &[u8]) -> Result<ObjectId, HostingError> {
        crate::hosting::rest::parse_nested_head(body)
    }

    fn checks_path(
        &self,
        repository: &HostingRepositoryId,
        head: &ObjectId,
        capabilities: &HostingCapabilitySet,
    ) -> Result<Option<String>, HostingError> {
        if !supports(capabilities, HostingCapability::CheckRuns) {
            return Ok(None);
        }
        Ok(Some(format!(
            "/repos/{}/commits/{}/check-runs",
            provider_repository_path(HostingProvider::GitHub, repository)?,
            head.as_str()
        )))
    }

    fn parse_checks(&self, body: &[u8]) -> Result<Vec<HostingCheck>, HostingError> {
        crate::hosting::rest::parse_github_check_runs(body)
    }

    /// Pull-request listings omit mergeability, so the pull request itself is read for it.
    fn pull_request_status(
        &self,
        requester: &dyn HostingProviderRequester,
        connection: &HostingConnection,
        repository: &HostingRepositoryId,
        pull: &mut HostingPullRequest,
        credential: &HostingCredential,
        cancellation: &dyn Cancellation,
    ) -> Result<(), HostingError> {
        fill_ci_status(
            self,
            requester,
            connection,
            repository,
            pull,
            credential,
            cancellation,
        )?;
        if !status_applies(pull) || pull.mergeability != Mergeability::Unknown {
            return Ok(());
        }
        let Ok(path) = self.pull_request_head_path(repository, pull.number) else {
            return Ok(());
        };
        if let Some(response) =
            optional(requester.request(connection, path, credential, cancellation))?
        {
            // A head that moved since the listing describes another commit.
            if let Ok(detail) = parse_github_pull_request(&response.body) {
                if detail.head_sha == pull.head_sha {
                    pull.mergeability = detail.mergeability;
                }
            }
        }
        Ok(())
    }

    fn check_rerun(
        &self,
        repository: &HostingRepositoryId,
        stable_id: &str,
        capabilities: &HostingCapabilitySet,
    ) -> Result<Option<HostingCheckRerun>, HostingError> {
        if !supports(capabilities, HostingCapability::CheckRuns) {
            return Ok(None);
        }
        let id = stable_numeric_id(stable_id, "github-check:")?;
        let status_path = format!(
            "/repos/{}/check-runs/{id}",
            provider_repository_path(HostingProvider::GitHub, repository)?
        );
        Ok(Some(HostingCheckRerun {
            path: format!("{status_path}/rerequest"),
            status_path,
        }))
    }
}
