use super::{
    validate_branch_query, validate_login_query, AuthorizationCodePkce, HostingProviderPlugin,
    OAuthIdentityField, OAuthLoginFlow,
};
use super::{HostingProviderRequester, HOSTING_AUTHORED_LIMIT, HOSTING_PAGE_SIZE};
use crate::cancellation::Cancellation;
use crate::hosting::model::{
    HostingConnection, HostingCredential, HostingError, HostingIssue, HostingProvider,
    HostingPullRequest, HostingRepository, HostingRepositoryId, PullRequestState,
};
use crate::hosting::parsers::{
    apply_action_run_statuses, parse_forgejo_issues, parse_forgejo_pull_requests,
    parse_forgejo_repositories, percent_encode,
};
use std::collections::HashMap;

/// Pages of recently updated pull requests a branch query reads. Forgejo cannot filter the
/// list by branch, and its base-and-head lookup answers with the oldest match, so branch
/// queries find the pull requests updated most recently.
const BRANCH_SCAN_PAGES: u32 = 2;

use crate::hosting::model::{HostingCapability, HostingCapabilitySet};
use crate::hosting::rest::{
    provider_repository_path, HostingCheck, HostingCheckRerun, HostingWebResource,
};
use crate::types::ObjectId;

pub(super) static FORGEJO: ForgejoProvider = ForgejoProvider;

pub(super) struct ForgejoProvider;

impl HostingProviderPlugin for ForgejoProvider {
    fn provider(&self) -> HostingProvider {
        HostingProvider::Forgejo
    }

    fn activity_name(&self) -> &'static str {
        "forgejo"
    }

    fn api_authorization_header_prefix(&self) -> &'static str {
        "Authorization: token "
    }

    fn pull_request_ref(&self, number: u64) -> String {
        format!("refs/pull/{number}/head")
    }

    fn login_flow(&self) -> OAuthLoginFlow {
        OAuthLoginFlow::AuthorizationCodePkce(AuthorizationCodePkce {
            authorization_path: "/login/oauth/authorize",
            scopes: &[],
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
            format!("/user/repos?limit={HOSTING_PAGE_SIZE}"),
            credential,
            cancellation,
        )?;
        parse_forgejo_repositories(&response.body)
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
                "/repos/{}/pulls?state=open&limit={HOSTING_PAGE_SIZE}",
                repository.as_str()
            ),
            credential,
            cancellation,
        )?;
        let mut pull_requests = parse_forgejo_pull_requests(&response.body)?;
        match requester.request(
            connection,
            format!(
                "/repos/{}/actions/runs?limit={HOSTING_PAGE_SIZE}",
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
                "/repos/{}/issues?state=all&type=issues&limit={HOSTING_PAGE_SIZE}",
                repository.as_str()
            ),
            credential,
            cancellation,
        )?;
        parse_forgejo_issues(&response.body)
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
        let mut pulls = Vec::new();
        self.branches_pull_requests(
            requester,
            connection,
            repository,
            &[branch.to_owned()],
            credential,
            cancellation,
            &mut |_, found| pulls = found,
        )?;
        Ok(pulls)
    }

    /// Reads the recently updated pull requests once for the whole batch.
    fn branches_pull_requests(
        &self,
        requester: &dyn HostingProviderRequester,
        connection: &HostingConnection,
        repository: &HostingRepositoryId,
        branches: &[String],
        credential: &HostingCredential,
        cancellation: &dyn Cancellation,
        found: &mut dyn FnMut(&str, Vec<HostingPullRequest>),
    ) -> Result<(), HostingError> {
        for branch in branches {
            validate_branch_query(branch)?;
        }
        let mut matches: HashMap<&str, Vec<HostingPullRequest>> = branches
            .iter()
            .map(|branch| (branch.as_str(), Vec::new()))
            .collect();
        for page in 1..=BRANCH_SCAN_PAGES {
            let response = requester.request(
                connection,
                format!(
                    "/repos/{}/pulls?state=all&sort=recentupdate&limit={HOSTING_PAGE_SIZE}&page={page}",
                    repository.as_str()
                ),
                credential,
                cancellation,
            )?;
            let pulls = parse_forgejo_pull_requests(&response.body)?;
            let complete = pulls.len() < HOSTING_PAGE_SIZE;
            for pull in pulls {
                if pull.state == PullRequestState::Closed
                    || pull.source_repository.as_ref() != Some(repository)
                {
                    continue;
                }
                if let Some(list) = matches.get_mut(pull.source_branch.as_str()) {
                    if list.iter().all(|known| known.number != pull.number) {
                        list.push(pull);
                    }
                }
            }
            if complete {
                break;
            }
        }
        for branch in branches {
            found(branch, matches.remove(branch.as_str()).unwrap_or_default());
        }
        Ok(())
    }

    /// Asks with `poster` and filters again, as servers before the filter ignore it.
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
                "/repos/{}/pulls?state=open&poster={}&limit={HOSTING_AUTHORED_LIMIT}",
                repository.as_str(),
                percent_encode(login)
            ),
            credential,
            cancellation,
        )?;
        Ok(parse_forgejo_pull_requests(&response.body)?
            .into_iter()
            .filter(|pull| pull.author == login)
            .take(HOSTING_AUTHORED_LIMIT)
            .collect())
    }

    fn web_path(&self, resource: HostingWebResource) -> Option<String> {
        match resource {
            HostingWebResource::Repository => Some(String::new()),
            HostingWebResource::ChangeRequest(number) if number > 0 => {
                Some(format!("/pulls/{number}"))
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
            provider_repository_path(HostingProvider::Forgejo, repository)?
        ))
    }

    fn parse_pull_request_head(&self, body: &[u8]) -> Result<ObjectId, HostingError> {
        crate::hosting::rest::parse_nested_head(body)
    }

    /// The combined commit status, which keeps the latest status of each context. Forgejo
    /// Actions reports its jobs there.
    fn checks_path(
        &self,
        repository: &HostingRepositoryId,
        head: &ObjectId,
        capabilities: &HostingCapabilitySet,
    ) -> Result<Option<String>, HostingError> {
        if !capabilities.supports(HostingCapability::CommitStatuses) {
            return Ok(None);
        }
        Ok(Some(format!(
            "/repos/{}/commits/{}/status",
            provider_repository_path(HostingProvider::Forgejo, repository)?,
            head.as_str()
        )))
    }

    fn parse_checks(&self, body: &[u8]) -> Result<Vec<HostingCheck>, HostingError> {
        crate::hosting::rest::parse_forgejo_commit_statuses(body)
    }

    fn check_rerun(
        &self,
        _repository: &HostingRepositoryId,
        _stable_id: &str,
        _capabilities: &HostingCapabilitySet,
    ) -> Result<Option<HostingCheckRerun>, HostingError> {
        Ok(None)
    }
}
