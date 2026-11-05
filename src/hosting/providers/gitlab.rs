use super::{
    validate_branch_query, validate_login_query, AuthorizationCodePkce, HostingProviderPlugin,
    OAuthIdentityField, OAuthLoginFlow,
};
use super::{HostingProviderRequester, HOSTING_AUTHORED_LIMIT, HOSTING_PAGE_SIZE};
use crate::cancellation::Cancellation;
use crate::hosting::model::{
    CiStatus, HostingConnection, HostingCredential, HostingError, HostingIssue, HostingProvider,
    HostingPullRequest, HostingRepository, HostingRepositoryId, PullRequestState,
};
use crate::hosting::parsers::{
    apply_gitlab_pipeline_statuses, parse_gitlab_issues, parse_gitlab_pull_requests,
    parse_gitlab_repositories, percent_encode,
};

use crate::hosting::model::{HostingCapability, HostingCapabilitySet};
use crate::hosting::rest::{
    provider_repository_path, stable_numeric_id, supports, HostingCheck, HostingCheckRerun,
    HostingWebResource,
};
use crate::types::ObjectId;

pub(super) static GITLAB: GitLabProvider = GitLabProvider;

pub(super) struct GitLabProvider;

impl HostingProviderPlugin for GitLabProvider {
    fn provider(&self) -> HostingProvider {
        HostingProvider::GitLab
    }

    fn activity_name(&self) -> &'static str {
        "gitlab"
    }

    fn api_authorization_header_prefix(&self) -> &'static str {
        "PRIVATE-TOKEN: "
    }

    fn pull_request_ref(&self, number: u64) -> String {
        format!("refs/merge-requests/{number}/head")
    }

    fn login_flow(&self) -> OAuthLoginFlow {
        OAuthLoginFlow::AuthorizationCodePkce(AuthorizationCodePkce {
            authorization_path: "/oauth/authorize",
            scopes: &["api"],
        })
    }

    fn token_path(&self) -> &'static str {
        "/oauth/token"
    }

    fn identity_field(&self) -> OAuthIdentityField {
        OAuthIdentityField::Username
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
            format!("/projects?membership=true&per_page={HOSTING_PAGE_SIZE}"),
            credential,
            cancellation,
        )?;
        parse_gitlab_repositories(&response.body)
    }

    fn pull_requests(
        &self,
        requester: &dyn HostingProviderRequester,
        connection: &HostingConnection,
        repository: &HostingRepositoryId,
        credential: &HostingCredential,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<HostingPullRequest>, HostingError> {
        let project = percent_encode(repository.as_str());
        let response = requester.request(
            connection,
            format!("/projects/{project}/merge_requests?state=opened&per_page={HOSTING_PAGE_SIZE}"),
            credential,
            cancellation,
        )?;
        let mut pull_requests = parse_gitlab_pull_requests(&response.body, repository)?;
        if pull_requests
            .iter()
            .any(|pull_request| pull_request.ci_status == CiStatus::Unknown)
        {
            match requester.request(
                connection,
                format!("/projects/{project}/pipelines?per_page={HOSTING_PAGE_SIZE}"),
                credential,
                cancellation,
            ) {
                Ok(pipelines) => {
                    let _ = apply_gitlab_pipeline_statuses(&mut pull_requests, &pipelines.body);
                }
                Err(HostingError::Canceled) => return Err(HostingError::Canceled),
                Err(_) => {}
            }
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
                "/projects/{}/issues?state=all&per_page={HOSTING_PAGE_SIZE}",
                percent_encode(repository.as_str())
            ),
            credential,
            cancellation,
        )?;
        parse_gitlab_issues(&response.body)
    }

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
        let project = percent_encode(repository.as_str());
        let response = requester.request(
            connection,
            format!(
                "/projects/{project}/merge_requests?state=all&source_branch={}&per_page={HOSTING_PAGE_SIZE}",
                percent_encode(branch)
            ),
            credential,
            cancellation,
        )?;
        Ok(parse_gitlab_pull_requests(&response.body, repository)?
            .into_iter()
            .filter(|pull| {
                pull.source_repository.as_ref() == Some(repository)
                    && pull.state != PullRequestState::Closed
            })
            .collect())
    }

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
        let project = percent_encode(repository.as_str());
        let response = requester.request(
            connection,
            format!(
                "/projects/{project}/merge_requests?state=opened&author_username={}&per_page={HOSTING_AUTHORED_LIMIT}",
                percent_encode(login)
            ),
            credential,
            cancellation,
        )?;
        let mut pull_requests = parse_gitlab_pull_requests(&response.body, repository)?;
        pull_requests.truncate(HOSTING_AUTHORED_LIMIT);
        Ok(pull_requests)
    }

    fn web_path(&self, resource: HostingWebResource) -> Option<String> {
        match resource {
            HostingWebResource::Repository => Some(String::new()),
            HostingWebResource::ChangeRequest(number) if number > 0 => {
                Some(format!("/-/merge_requests/{number}"))
            }
            HostingWebResource::CreateChangeRequest => Some("/-/merge_requests/new".into()),
            HostingWebResource::Issue(number) if number > 0 => Some(format!("/-/issues/{number}")),
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
            "/projects/{}/merge_requests/{number}",
            provider_repository_path(HostingProvider::GitLab, repository)?
        ))
    }

    fn parse_pull_request_head(&self, body: &[u8]) -> Result<ObjectId, HostingError> {
        crate::hosting::rest::parse_flat_head(body)
    }

    fn checks_path(
        &self,
        repository: &HostingRepositoryId,
        head: &ObjectId,
        capabilities: &HostingCapabilitySet,
    ) -> Result<Option<String>, HostingError> {
        if !supports(capabilities, HostingCapability::CommitStatuses) {
            return Ok(None);
        }
        Ok(Some(format!(
            "/projects/{}/pipelines?sha={}&per_page=20",
            provider_repository_path(HostingProvider::GitLab, repository)?,
            head.as_str()
        )))
    }

    fn parse_checks(&self, body: &[u8]) -> Result<Vec<HostingCheck>, HostingError> {
        crate::hosting::rest::parse_gitlab_pipelines(body)
    }

    /// Pipelines arrive newest first, and a newer pipeline replaces the older ones' result.
    fn ci_status_of(&self, checks: &[HostingCheck]) -> CiStatus {
        super::combined_ci_status(checks.first().map(|check| check.state))
    }

    fn check_rerun(
        &self,
        repository: &HostingRepositoryId,
        stable_id: &str,
        capabilities: &HostingCapabilitySet,
    ) -> Result<Option<HostingCheckRerun>, HostingError> {
        if !supports(capabilities, HostingCapability::CommitStatuses) {
            return Ok(None);
        }
        let id = stable_numeric_id(stable_id, "gitlab-pipeline:")?;
        let status_path = format!(
            "/projects/{}/pipelines/{id}",
            provider_repository_path(HostingProvider::GitLab, repository)?
        );
        Ok(Some(HostingCheckRerun {
            path: format!("{status_path}/retry"),
            status_path,
        }))
    }
}
