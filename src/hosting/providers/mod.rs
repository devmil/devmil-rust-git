//! Compile-time hosting provider units.
//!
//! Each provider owns its OAuth rules, API authentication, resource loading, pull-request refs,
//! and legacy credential identity behind the same contract. The shared hosting worker owns browser
//! launch, HTTP execution, cancellation, token storage, bounds, and activity redaction.

mod forgejo;
mod github;
mod gitlab;

use crate::cancellation::Cancellation;
use crate::hosting::model::{
    CiStatus, HostingCapabilitySet, HostingConnection, HostingCredential, HostingError,
    HostingIssue, HostingProvider, HostingPullRequest, HostingRepository, HostingRepositoryId,
    PullRequestState,
};
use crate::hosting::rest::{
    HostingCheck, HostingCheckRerun, HostingCheckState, HostingWebResource,
};
use crate::types::ObjectId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthorizationCodePkce {
    pub authorization_path: &'static str,
    pub scopes: &'static [&'static str],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceAuthorization {
    pub device_code_path: &'static str,
    pub verification_path: &'static str,
    pub scopes: &'static [&'static str],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OAuthLoginFlow {
    AuthorizationCodePkce(AuthorizationCodePkce),
    DeviceAuthorization(DeviceAuthorization),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OAuthIdentityField {
    Login,
    Username,
}

pub trait HostingProviderPlugin: Sync {
    fn provider(&self) -> HostingProvider;
    fn activity_name(&self) -> &'static str;
    fn api_authorization_header_prefix(&self) -> &'static str;
    fn pull_request_ref(&self, number: u64) -> String;
    fn login_flow(&self) -> OAuthLoginFlow;
    fn token_path(&self) -> &'static str;
    fn identity_field(&self) -> OAuthIdentityField;
    /// The path after `<web base>/<repository>` for `resource` (empty for the repository page),
    /// or `None` for a zero number. Use [`crate::hosting::rest::web_link`] for a full URL.
    fn web_path(&self, resource: HostingWebResource) -> Option<String>;
    /// The API path of one pull or merge request, whose response carries its head commit.
    ///
    /// # Errors
    /// Rejects a zero number or an invalid repository path.
    fn pull_request_head_path(
        &self,
        repository: &HostingRepositoryId,
        number: u64,
    ) -> Result<String, HostingError>;
    /// Parses the head commit from a [`Self::pull_request_head_path`] response.
    ///
    /// # Errors
    /// Returns [`HostingError::MalformedResponse`] for an unexpected body.
    fn parse_pull_request_head(&self, body: &[u8]) -> Result<ObjectId, HostingError>;
    /// The API path listing check runs (GitHub, with `CheckRuns`) or
    /// pipelines (GitLab, with `CommitStatuses`) for `head`; `Ok(None)`
    /// when the provider or the connection's capabilities offer none.
    ///
    /// # Errors
    /// Rejects an invalid repository path.
    fn checks_path(
        &self,
        repository: &HostingRepositoryId,
        head: &ObjectId,
        capabilities: &HostingCapabilitySet,
    ) -> Result<Option<String>, HostingError>;
    /// Parses a [`Self::checks_path`] response.
    ///
    /// # Errors
    /// Returns [`HostingError::MalformedResponse`] for an unexpected or oversized body.
    fn parse_checks(&self, body: &[u8]) -> Result<Vec<HostingCheck>, HostingError>;
    /// The CI result of a head from its [`Self::parse_checks`] checks. The default combines
    /// every check, the worst one deciding.
    fn ci_status_of(&self, checks: &[HostingCheck]) -> CiStatus {
        combined_ci_status(checks.iter().map(|check| check.state))
    }
    /// The CI result of commit `head`, read through [`Self::checks_path`]: for example the
    /// head of a default branch, which no pull request carries.
    ///
    /// The value is optional: a provider or connection without checks, or a failed or
    /// malformed answer, reports [`CiStatus::Unknown`].
    ///
    /// # Errors
    /// Returns only [`HostingError::Canceled`].
    fn commit_ci_status(
        &self,
        requester: &dyn HostingProviderRequester,
        connection: &HostingConnection,
        repository: &HostingRepositoryId,
        head: &ObjectId,
        credential: &HostingCredential,
        cancellation: &dyn Cancellation,
    ) -> Result<CiStatus, HostingError> {
        let Ok(Some(path)) = self.checks_path(repository, head, &connection.capabilities) else {
            return Ok(CiStatus::Unknown);
        };
        let Some(response) =
            optional(requester.request(connection, path, credential, cancellation))?
        else {
            return Ok(CiStatus::Unknown);
        };
        Ok(self
            .parse_checks(&response.body)
            .map_or(CiStatus::Unknown, |checks| self.ci_status_of(&checks)))
    }
    /// Fills in what a listing left unknown about an open pull request: the CI result of its
    /// head (through [`Self::checks_path`]) and, where listings omit it, its mergeability.
    /// Merged and closed pull requests are left as they are.
    ///
    /// Both values are optional: a failed or malformed answer leaves them unknown.
    ///
    /// # Errors
    /// Returns only [`HostingError::Canceled`].
    #[allow(clippy::too_many_arguments)]
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
        )
    }
    /// The re-run request for a check from [`Self::parse_checks`]; `Ok(None)` when unsupported.
    ///
    /// # Errors
    /// Rejects a stable ID of another provider or kind, or an invalid repository path.
    fn check_rerun(
        &self,
        repository: &HostingRepositoryId,
        stable_id: &str,
        capabilities: &HostingCapabilitySet,
    ) -> Result<Option<HostingCheckRerun>, HostingError>;
    /// # Errors
    /// Returns provider validation, authentication, transport, or cancellation failures.
    fn repositories(
        &self,
        requester: &dyn HostingProviderRequester,
        connection: &HostingConnection,
        credential: &HostingCredential,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<HostingRepository>, HostingError>;
    /// # Errors
    /// Returns provider validation, authentication, transport, or cancellation failures.
    fn pull_requests(
        &self,
        requester: &dyn HostingProviderRequester,
        connection: &HostingConnection,
        repository: &HostingRepositoryId,
        credential: &HostingCredential,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<HostingPullRequest>, HostingError>;
    /// # Errors
    /// Returns provider validation, authentication, transport, or cancellation failures.
    fn issues(
        &self,
        requester: &dyn HostingProviderRequester,
        connection: &HostingConnection,
        repository: &HostingRepositoryId,
        credential: &HostingCredential,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<HostingIssue>, HostingError>;
    /// Open and merged pull requests whose source is `branch` in `repository` itself; pull
    /// requests closed without a merge are left out.
    ///
    /// The default filters the first open page, so it finds no merged pull requests; providers
    /// with a server-side branch filter override it so a branch beyond that page is still found.
    ///
    /// # Errors
    /// Returns provider validation, authentication, transport, or cancellation failures.
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
        Ok(self
            .pull_requests(requester, connection, repository, credential, cancellation)?
            .into_iter()
            .filter(|pull| {
                pull.source_branch == branch && pull.source_repository.as_ref() == Some(repository)
            })
            .collect())
    }
    /// Answers [`Self::branch_pull_requests`] for each of `branches`, reporting each branch to
    /// `found` as it resolves, so a later failure keeps what already resolved.
    ///
    /// The default asks once per branch; providers that answer several branches with one
    /// request override it.
    ///
    /// # Errors
    /// Returns provider validation, authentication, transport, or cancellation failures.
    #[allow(clippy::too_many_arguments)]
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
            let pulls = self.branch_pull_requests(
                requester,
                connection,
                repository,
                branch,
                credential,
                cancellation,
            )?;
            found(branch, pulls);
        }
        Ok(())
    }
    /// Open pull requests in `repository` authored by `login`, at most
    /// [`HOSTING_AUTHORED_LIMIT`].
    ///
    /// The default filters the first open page; providers with a server-side author filter
    /// override it.
    ///
    /// # Errors
    /// Returns provider validation, authentication, transport, or cancellation failures.
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
        Ok(self
            .pull_requests(requester, connection, repository, credential, cancellation)?
            .into_iter()
            .filter(|pull| pull.author == login)
            .take(HOSTING_AUTHORED_LIMIT)
            .collect())
    }
}

/// Whether [`HostingProviderPlugin::pull_request_status`] has anything to read for `pull`.
pub(crate) fn status_applies(pull: &HostingPullRequest) -> bool {
    matches!(pull.state, PullRequestState::Open | PullRequestState::Draft)
}

/// Optional enrichment keeps the value unknown on any failure except cancellation.
pub(crate) fn optional<T>(result: Result<T, HostingError>) -> Result<Option<T>, HostingError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(HostingError::Canceled) => Err(HostingError::Canceled),
        Err(_) => Ok(None),
    }
}

/// Reads the checks of an open pull request's head when its CI result is still unknown.
pub(crate) fn fill_ci_status<P: HostingProviderPlugin + ?Sized>(
    plugin: &P,
    requester: &dyn HostingProviderRequester,
    connection: &HostingConnection,
    repository: &HostingRepositoryId,
    pull: &mut HostingPullRequest,
    credential: &HostingCredential,
    cancellation: &dyn Cancellation,
) -> Result<(), HostingError> {
    if !status_applies(pull) || pull.ci_status != CiStatus::Unknown {
        return Ok(());
    }
    let Ok(head) = ObjectId::parse(pull.head_sha.clone()) else {
        return Ok(());
    };
    pull.ci_status = plugin.commit_ci_status(
        requester,
        connection,
        repository,
        &head,
        credential,
        cancellation,
    )?;
    Ok(())
}

/// The worst of `states`: failing, then pending, then canceled, then passing. Skipped and
/// unknown checks decide nothing, so a head with only those stays unknown.
#[must_use]
pub fn combined_ci_status(states: impl IntoIterator<Item = HostingCheckState>) -> CiStatus {
    let mut combined = CiStatus::Unknown;
    for state in states {
        let rank = |status: CiStatus| match status {
            CiStatus::Failing => 4,
            CiStatus::Pending => 3,
            CiStatus::Canceled => 2,
            CiStatus::Passing => 1,
            CiStatus::Unknown => 0,
        };
        let next = match state {
            HostingCheckState::Failing => CiStatus::Failing,
            HostingCheckState::Pending => CiStatus::Pending,
            HostingCheckState::Canceled => CiStatus::Canceled,
            HostingCheckState::Passing => CiStatus::Passing,
            HostingCheckState::Skipped | HostingCheckState::Unknown => CiStatus::Unknown,
        };
        if rank(next) > rank(combined) {
            combined = next;
        }
    }
    combined
}

/// Upper bound for one authored pull-request query.
pub const HOSTING_AUTHORED_LIMIT: usize = 20;

/// Branch names are sent to providers as query values; reject what Git itself never allows.
pub(crate) fn validate_branch_query(branch: &str) -> Result<(), HostingError> {
    if branch.is_empty() || branch.len() > 1024 || branch.chars().any(char::is_control) {
        return Err(HostingError::InvalidIdentity);
    }
    Ok(())
}

pub(crate) fn validate_login_query(login: &str) -> Result<(), HostingError> {
    if login.is_empty() || login.len() > 512 || login.chars().any(char::is_control) {
        return Err(HostingError::InvalidIdentity);
    }
    Ok(())
}

pub fn provider_plugin(provider: HostingProvider) -> &'static dyn HostingProviderPlugin {
    let plugin: &'static dyn HostingProviderPlugin = match provider {
        HostingProvider::GitHub => &github::GITHUB,
        HostingProvider::GitLab => &gitlab::GITLAB,
        HostingProvider::Forgejo => &forgejo::FORGEJO,
    };
    debug_assert_eq!(plugin.provider(), provider);
    plugin
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_provider_unit_reports_its_registration_identity() {
        for provider in [
            HostingProvider::GitHub,
            HostingProvider::GitLab,
            HostingProvider::Forgejo,
        ] {
            assert_eq!(provider_plugin(provider).provider(), provider);
        }
    }

    #[test]
    fn provider_units_build_head_check_and_rerun_paths() {
        use crate::hosting::model::HostingCapabilitySet;
        let repository = HostingRepositoryId::new("group/project").unwrap();
        let head = ObjectId::parse("a".repeat(40)).unwrap();
        let github = provider_plugin(HostingProvider::GitHub);
        let all = HostingCapabilitySet::baseline(HostingProvider::GitHub);
        assert_eq!(
            github.pull_request_head_path(&repository, 3).unwrap(),
            "/repos/group/project/pulls/3"
        );
        assert!(github.pull_request_head_path(&repository, 0).is_err());
        assert_eq!(
            github
                .checks_path(&repository, &head, &all)
                .unwrap()
                .unwrap(),
            format!("/repos/group/project/commits/{}/check-runs", head.as_str())
        );
        let rerun = github
            .check_rerun(&repository, "github-check:5", &all)
            .unwrap()
            .unwrap();
        assert_eq!(rerun.path, "/repos/group/project/check-runs/5/rerequest");
        assert_eq!(rerun.status_path, "/repos/group/project/check-runs/5");
        assert!(github
            .check_rerun(&repository, "gitlab-pipeline:5", &all)
            .is_err());
        let none = HostingCapabilitySet {
            supported: std::collections::BTreeSet::new(),
        };
        assert_eq!(github.checks_path(&repository, &head, &none).unwrap(), None);

        let gitlab = provider_plugin(HostingProvider::GitLab);
        let all = HostingCapabilitySet::baseline(HostingProvider::GitLab);
        assert_eq!(
            gitlab.pull_request_head_path(&repository, 3).unwrap(),
            "/projects/group%2Fproject/merge_requests/3"
        );
        assert_eq!(
            gitlab
                .check_rerun(&repository, "gitlab-pipeline:9", &all)
                .unwrap()
                .unwrap()
                .path,
            "/projects/group%2Fproject/pipelines/9/retry"
        );
        let forgejo = provider_plugin(HostingProvider::Forgejo);
        let repository = HostingRepositoryId::new("team/project").unwrap();
        assert_eq!(
            forgejo
                .checks_path(
                    &repository,
                    &head,
                    &HostingCapabilitySet::baseline(HostingProvider::Forgejo)
                )
                .unwrap(),
            Some(format!(
                "/repos/team/project/commits/{}/status",
                head.as_str()
            ))
        );
        assert_eq!(
            forgejo.checks_path(&repository, &head, &none).unwrap(),
            None
        );
        assert_eq!(
            forgejo
                .check_rerun(
                    &repository,
                    "forgejo-status:4",
                    &HostingCapabilitySet::baseline(HostingProvider::Forgejo)
                )
                .unwrap(),
            None
        );
    }

    #[test]
    fn provider_units_keep_distinct_oauth_rules() {
        let github = provider_plugin(HostingProvider::GitHub);
        assert_eq!(
            github.login_flow(),
            OAuthLoginFlow::DeviceAuthorization(DeviceAuthorization {
                device_code_path: "/login/device/code",
                verification_path: "/login/device",
                scopes: &["repo"],
            })
        );
        assert_eq!(github.token_path(), "/login/oauth/access_token");
        assert_eq!(github.identity_field(), OAuthIdentityField::Login);
        assert_eq!(github.activity_name(), "github");
        assert_eq!(
            github.api_authorization_header_prefix(),
            "Authorization: Bearer "
        );
        assert_eq!(github.pull_request_ref(7), "refs/pull/7/head");

        let gitlab = provider_plugin(HostingProvider::GitLab);
        assert_eq!(
            gitlab.login_flow(),
            OAuthLoginFlow::AuthorizationCodePkce(AuthorizationCodePkce {
                authorization_path: "/oauth/authorize",
                scopes: &["api"],
            })
        );
        assert_eq!(gitlab.token_path(), "/oauth/token");
        assert_eq!(gitlab.identity_field(), OAuthIdentityField::Username);
        assert_eq!(gitlab.activity_name(), "gitlab");
        assert_eq!(gitlab.api_authorization_header_prefix(), "PRIVATE-TOKEN: ");
        assert_eq!(gitlab.pull_request_ref(7), "refs/merge-requests/7/head");

        let forgejo = provider_plugin(HostingProvider::Forgejo);
        assert_eq!(
            forgejo.login_flow(),
            OAuthLoginFlow::AuthorizationCodePkce(AuthorizationCodePkce {
                authorization_path: "/login/oauth/authorize",
                scopes: &[],
            })
        );
        assert_eq!(forgejo.token_path(), "/login/oauth/access_token");
        assert_eq!(forgejo.identity_field(), OAuthIdentityField::Login);
        assert_eq!(forgejo.activity_name(), "forgejo");
        assert_eq!(
            forgejo.api_authorization_header_prefix(),
            "Authorization: token "
        );
        assert_eq!(forgejo.pull_request_ref(7), "refs/pull/7/head");
    }
}

pub const HOSTING_PAGE_SIZE: usize = 50;
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostingHttpRequest {
    pub base_url: String,
    pub path: String,
    pub provider: HostingProvider,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostingHttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

pub trait HostingProviderRequester {
    /// # Errors
    /// Returns provider validation, authentication, transport, or cancellation failures.
    fn request(
        &self,
        connection: &HostingConnection,
        path: String,
        credential: &HostingCredential,
        cancellation: &dyn Cancellation,
    ) -> Result<HostingHttpResponse, HostingError>;
}
