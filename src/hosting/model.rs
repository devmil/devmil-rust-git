//! Provider-neutral hosting identities, capabilities, records, and credentials.

use crate::refs::RemoteId;
use crate::types::RepositoryId;
use std::fmt;
use zeroize::Zeroize;

pub const MAX_HOSTING_ITEMS: usize = 200;
pub const MAX_HOSTING_TEXT_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum HostingProvider {
    GitHub,
    GitLab,
    Forgejo,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum HostingPreset {
    GitHub,
    GitLab,
    GitLabSelfManaged,
    Forgejo,
    Codeberg,
    GitHubEnterprise,
}

impl HostingPreset {
    #[must_use]
    pub const fn provider(self) -> HostingProvider {
        match self {
            Self::GitHub | Self::GitHubEnterprise => HostingProvider::GitHub,
            Self::GitLab | Self::GitLabSelfManaged => HostingProvider::GitLab,
            Self::Forgejo | Self::Codeberg => HostingProvider::Forgejo,
        }
    }

    #[must_use]
    pub const fn default_web_base(self) -> Option<&'static str> {
        match self {
            Self::GitHub => Some("https://github.com"),
            Self::GitLab => Some("https://gitlab.com"),
            Self::Codeberg => Some("https://codeberg.org"),
            Self::GitLabSelfManaged | Self::Forgejo | Self::GitHubEnterprise => None,
        }
    }

    #[must_use]
    pub const fn default_api_base(self) -> Option<&'static str> {
        match self {
            Self::GitHub => Some("https://api.github.com"),
            Self::GitLab => Some("https://gitlab.com/api/v4"),
            Self::Codeberg => Some("https://codeberg.org/api/v1"),
            Self::GitLabSelfManaged | Self::Forgejo | Self::GitHubEnterprise => None,
        }
    }

    /// The API base a server at `web_base_url` serves for this preset. Self-hosted servers
    /// publish their API under a path of the web origin. GitHub Enterprise Cloud with data
    /// residency (`<tenant>.ghe.com`) is the exception: its API lives on `api.<tenant>.ghe.com`.
    ///
    /// Returns `None` when `web_base_url` is not a bare `https` origin.
    #[must_use]
    pub fn api_base_for(self, web_base_url: &str) -> Option<String> {
        if let Some(fixed) = self.default_api_base() {
            return Some(fixed.to_owned());
        }
        let server = connection_server(web_base_url)?;
        Some(match self {
            Self::GitLabSelfManaged => format!("https://{server}/api/v4"),
            Self::Forgejo => format!("https://{server}/api/v1"),
            Self::GitHubEnterprise if is_github_tenant_host(&server) => {
                format!("https://api.{server}")
            }
            Self::GitHubEnterprise => format!("https://{server}/api/v3"),
            Self::GitHub | Self::GitLab | Self::Codeberg => {
                unreachable!("public presets have a fixed API base")
            }
        })
    }
}

/// Whether `server` is a GitHub Enterprise Cloud data-residency tenant (`<tenant>.ghe.com`).
fn is_github_tenant_host(server: &str) -> bool {
    let host = server.split(':').next().unwrap_or(server);
    host.strip_suffix(".ghe.com")
        .is_some_and(|tenant| !tenant.is_empty() && !tenant.contains('.'))
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HostingConnectionId(String);

impl HostingConnectionId {
    /// # Errors
    ///
    /// Returns [`HostingError::InvalidIdentity`] for an empty, oversized, or unsafe identity.
    pub fn new(value: impl Into<String>) -> Result<Self, HostingError> {
        let value = value.into();
        validate_connection_identity(&value)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostingConnection {
    pub id: HostingConnectionId,
    pub preset: HostingPreset,
    pub login: String,
    pub web_base_url: String,
    pub api_base_url: String,
    /// Public OAuth application identifier. OAuth client secrets are never accepted by desktop
    /// clients. Provider units select authorization code with PKCE or device authorization.
    pub oauth_client_id: String,
    pub enabled: bool,
    pub capabilities: HostingCapabilitySet,
}

impl HostingConnection {
    #[must_use]
    pub const fn provider(&self) -> HostingProvider {
        self.preset.provider()
    }

    /// Converts a legacy account to the durable connection model while retaining its persisted
    /// identity, including GitHub Enterprise Server accounts.
    #[must_use]
    pub fn from_legacy_account(account: &HostingAccount) -> Option<Self> {
        let preset = match account.provider {
            HostingProvider::GitHub if account.server.eq_ignore_ascii_case("github.com") => {
                HostingPreset::GitHub
            }
            HostingProvider::GitHub => HostingPreset::GitHubEnterprise,
            HostingProvider::GitLab if account.server.eq_ignore_ascii_case("gitlab.com") => {
                HostingPreset::GitLab
            }
            HostingProvider::GitLab => HostingPreset::GitLabSelfManaged,
            HostingProvider::Forgejo if account.server.eq_ignore_ascii_case("codeberg.org") => {
                HostingPreset::Codeberg
            }
            HostingProvider::Forgejo => HostingPreset::Forgejo,
        };
        let web_base_url = format!("https://{}", account.server);
        let api_base_url = preset.api_base_for(&web_base_url)?;
        Some(Self {
            id: account.id.clone(),
            preset,
            login: account.login.clone(),
            web_base_url,
            api_base_url,
            oauth_client_id: String::new(),
            enabled: true,
            capabilities: HostingCapabilitySet::baseline(account.provider),
        })
    }

    /// Produces a legacy compatibility view used by the adapter while it moves to direct
    /// connection inputs. The stable identity keeps existing credential-helper entries readable.
    #[must_use]
    pub fn legacy_account(&self) -> HostingAccount {
        HostingAccount {
            id: self.id.clone(),
            provider: self.provider(),
            login: self.login.clone(),
            server: connection_server(&self.web_base_url).unwrap_or_default(),
        }
    }
}

pub fn connection_server(web_base_url: &str) -> Option<String> {
    let authority = web_base_url.strip_prefix("https://")?;
    (!authority.is_empty() && !authority.contains(['/', '@', '?', '#']))
        .then(|| authority.trim_end_matches('.').to_ascii_lowercase())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostingRepositoryBinding {
    pub repository: RepositoryId,
    pub remote: RemoteId,
    pub connection: HostingConnectionId,
    pub provider_repository: HostingRepositoryId,
}

/// A repository binding derived from a credential-free Git remote URL. This compatibility type
/// bridges the current account model to [`HostingRepositoryBinding`] without persisting redundant
/// repository metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InferredHostingBinding {
    pub remote: RemoteId,
    pub account_id: HostingAccountId,
    pub provider_repository: HostingRepositoryId,
    pub web_url: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostingBindingResolution {
    Unmatched,
    Ambiguous { remote: RemoteId, server: String },
    Bound(InferredHostingBinding),
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum HostingCapability {
    ChangeRequests,
    ChangeRequestCheckout,
    ChangeRequestReview,
    CommitStatuses,
    CheckRuns,
    Issues,
    RemoteReferences,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostingCapabilitySet {
    pub supported: std::collections::BTreeSet<HostingCapability>,
}

impl HostingCapabilitySet {
    #[must_use]
    pub fn baseline(provider: HostingProvider) -> Self {
        use HostingCapability::{
            ChangeRequestCheckout, ChangeRequests, CommitStatuses, Issues, RemoteReferences,
        };
        Self {
            supported: [
                ChangeRequests,
                ChangeRequestCheckout,
                CommitStatuses,
                Issues,
                RemoteReferences,
            ]
            .into_iter()
            .chain((provider == HostingProvider::GitHub).then_some(HostingCapability::CheckRuns))
            .collect(),
        }
    }

    #[must_use]
    pub fn supports(&self, capability: HostingCapability) -> bool {
        self.supported.contains(&capability)
    }
}

/// Compatibility name for account identities used by connection records.
pub type HostingAccountId = HostingConnectionId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostingAccount {
    pub id: HostingAccountId,
    pub provider: HostingProvider,
    pub login: String,
    pub server: String,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HostingRepositoryId(String);

impl HostingRepositoryId {
    /// # Errors
    ///
    /// Returns [`HostingError::InvalidIdentity`] for an empty, oversized, or unsafe identity.
    pub fn new(value: impl Into<String>) -> Result<Self, HostingError> {
        let value = value.into();
        validate_identity(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostingRepository {
    pub id: HostingRepositoryId,
    pub full_name: String,
    pub clone_url: String,
    pub web_url: String,
    pub private: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PullRequestState {
    Open,
    Draft,
    /// Merged into its target. Only queries that ask for finished pull requests return it.
    Merged,
    /// Closed without being merged.
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CiStatus {
    Pending,
    Passing,
    Failing,
    Canceled,
    Unknown,
}

/// Whether a pull request merges into its target without conflicts, as far as the provider says.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mergeability {
    Mergeable,
    Conflicting,
    /// The provider is still computing it.
    Checking,
    /// Not reported, not applicable (a merged or closed pull request) or not read.
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostingPullRequest {
    pub number: u64,
    pub title: String,
    pub author: String,
    /// Exact provider repository identity for the source branch. This is `None` when the
    /// provider response does not contain enough information to identify a fork safely.
    pub source_repository: Option<HostingRepositoryId>,
    pub source_branch: String,
    /// Exact provider repository identity for the target branch when supplied by the provider.
    pub target_repository: Option<HostingRepositoryId>,
    pub target_branch: String,
    pub head_sha: String,
    pub web_url: String,
    pub state: PullRequestState,
    /// The CI result of `head_sha`; [`CiStatus::Unknown`] when not reported or not read.
    pub ci_status: CiStatus,
    pub mergeability: Mergeability,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IssueState {
    Open,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostingIssue {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub web_url: String,
    pub state: IssueState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteReferenceKind {
    Issue,
    ChangeRequest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostingRemoteReference {
    pub stable_id: String,
    pub kind: RemoteReferenceKind,
    pub number: u64,
    pub title: String,
    pub insertion: String,
    pub web_url: String,
}

/// A transient secret. It is never cloneable or printable and is cleared on drop.
pub struct HostingCredential(Vec<u8>);

impl HostingCredential {
    /// # Errors
    ///
    /// Returns [`HostingError::InvalidCredential`] for an empty or oversized secret.
    pub fn new(value: Vec<u8>) -> Result<Self, HostingError> {
        if value.is_empty() || value.len() > 16 * 1024 {
            return Err(HostingError::InvalidCredential);
        }
        Ok(Self(value))
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for HostingCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HostingCredential([REDACTED])")
    }
}

impl Drop for HostingCredential {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostingError {
    InvalidIdentity,
    InvalidAccount,
    InvalidCredential,
    CredentialUnavailable,
    Canceled,
    Unauthorized,
    PermissionDenied,
    RateLimited,
    MalformedResponse,
    Unavailable(String),
}

impl fmt::Display for HostingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentity => formatter.write_str("hosting identity is invalid"),
            Self::InvalidAccount => formatter.write_str("hosting account is invalid"),
            Self::InvalidCredential => formatter.write_str("hosting credential is invalid"),
            Self::CredentialUnavailable => formatter.write_str("hosting credential is unavailable"),
            Self::Canceled => formatter.write_str("hosting request was canceled"),
            Self::Unauthorized => formatter.write_str("hosting account authorization failed"),
            Self::PermissionDenied => formatter.write_str("hosting provider permission denied"),
            Self::RateLimited => formatter.write_str("hosting provider rate limit was reached"),
            Self::MalformedResponse => {
                formatter.write_str("hosting provider returned malformed data")
            }
            Self::Unavailable(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for HostingError {}

pub fn bound_text(mut value: String) -> String {
    while value.len() > MAX_HOSTING_TEXT_BYTES {
        value.pop();
    }
    value
}

/// Checks enabled state, login bounds, and HTTPS connection endpoints.
///
/// # Errors
/// Returns [`HostingError::InvalidAccount`] for an invalid or disabled connection.
pub fn validate_connection(connection: &HostingConnection) -> Result<(), HostingError> {
    if !connection.enabled
        || connection.login.is_empty()
        || connection.login.len() > MAX_HOSTING_TEXT_BYTES
        || connection_server(&connection.web_base_url).is_none()
        || !connection.api_base_url.starts_with("https://")
        || connection.api_base_url.len() > 2_048
        || connection.api_base_url.contains(['@', '?', '#'])
    {
        return Err(HostingError::InvalidAccount);
    }
    Ok(())
}

/// Validates a provider or connection identity without embedding it in error text.
///
/// # Errors
/// Returns [`HostingError::InvalidIdentity`] for empty, oversized, or unsafe values.
pub fn validate_identity(value: &str) -> Result<(), HostingError> {
    if value.is_empty()
        || value.len() > MAX_HOSTING_TEXT_BYTES
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
    {
        Err(HostingError::InvalidIdentity)
    } else {
        Ok(())
    }
}

fn validate_connection_identity(value: &str) -> Result<(), HostingError> {
    if value.is_empty()
        || value.len() > MAX_HOSTING_TEXT_BYTES
        || value.trim() != value
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/' | b' ')
        })
    {
        Err(HostingError::InvalidIdentity)
    } else {
        Ok(())
    }
}

/// Resolves the first preferred Git remote whose host has a configured account. Callers order
/// remotes by repository semantics, normally current upstream, `origin`, then remaining names.
/// No provider request or credential lookup occurs here.
#[must_use]
pub fn infer_hosting_binding<'a>(
    accounts: &[HostingAccount],
    remotes: impl IntoIterator<Item = (&'a RemoteId, &'a str)>,
) -> HostingBindingResolution {
    for (remote, url) in remotes {
        let Some((server, repository)) = parse_remote_repository(url) else {
            continue;
        };
        let matches = accounts
            .iter()
            .filter(|account| normalized_server(&account.server) == server)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => {}
            [account] => {
                let Ok(provider_repository) = HostingRepositoryId::new(repository.clone()) else {
                    continue;
                };
                return HostingBindingResolution::Bound(InferredHostingBinding {
                    remote: remote.clone(),
                    account_id: account.id.clone(),
                    provider_repository,
                    web_url: format!(
                        "https://{}/{}",
                        account.server.trim().trim_end_matches('/'),
                        repository
                    ),
                });
            }
            _ => {
                return HostingBindingResolution::Ambiguous {
                    remote: remote.clone(),
                    server,
                };
            }
        }
    }
    HostingBindingResolution::Unmatched
}

pub fn parse_remote_repository(value: &str) -> Option<(String, String)> {
    let (authority, path) = if let Some(after_scheme) = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .or_else(|| value.strip_prefix("ssh://"))
    {
        if after_scheme.contains(['?', '#']) {
            return None;
        }
        after_scheme.split_once('/')?
    } else {
        let (authority, path) = value.split_once(':')?;
        (authority, path)
    };
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let server = normalized_server(authority);
    if server.is_empty()
        || !server
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-".contains(&byte))
    {
        return None;
    }
    let repository = path.trim_matches('/');
    let repository = repository.strip_suffix(".git").unwrap_or(repository);
    if repository.split('/').count() < 2
        || repository
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return None;
    }
    Some((server, repository.to_owned()))
}

pub fn normalized_server(value: &str) -> String {
    let value = value.trim().trim_end_matches('.').to_ascii_lowercase();
    if let Some(bracketed) = value.strip_prefix('[') {
        return bracketed
            .split_once(']')
            .map_or_else(|| value.clone(), |(host, _)| host.to_owned());
    }
    value
        .rsplit_once(':')
        .filter(|(host, port)| {
            !host.contains(':')
                && !host.is_empty()
                && port.bytes().all(|byte| byte.is_ascii_digit())
        })
        .map_or(value.clone(), |(host, _)| host.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> HostingAccount {
        HostingAccount {
            id: HostingAccountId::new("github:alice").unwrap(),
            provider: HostingProvider::GitHub,
            login: "alice".into(),
            server: "github.com".into(),
        }
    }

    #[test]
    fn binding_inference_accepts_https_ssh_and_scp_without_credentials() {
        let account = account();
        let remote = RemoteId::from_bytes(b"origin".to_vec()).unwrap();
        for url in [
            "https://github.com/team/project.git",
            "ssh://git@github.com:22/team/project.git",
            "git@github.com:team/project.git",
        ] {
            let HostingBindingResolution::Bound(binding) =
                infer_hosting_binding(std::slice::from_ref(&account), [(&remote, url)])
            else {
                panic!("configured account should bind");
            };
            assert_eq!(binding.account_id, account.id);
            assert_eq!(binding.provider_repository.as_str(), "team/project");
            assert_eq!(binding.web_url, "https://github.com/team/project");
        }
    }

    #[test]
    fn ambiguous_accounts_and_unsafe_remote_paths_do_not_bind() {
        let account = account();
        let remote = RemoteId::from_bytes(b"origin".to_vec()).unwrap();
        assert!(matches!(
            infer_hosting_binding(
                &[account.clone(), account.clone()],
                [(&remote, "https://github.com/team/project")]
            ),
            HostingBindingResolution::Ambiguous { .. }
        ));
        for url in [
            "https://github.com/team/../escape",
            "https://github.com/team/project?token=secret",
            "/local/project",
            "https://unknown.test/team/project",
        ] {
            assert_eq!(
                infer_hosting_binding(std::slice::from_ref(&account), [(&remote, url)]),
                HostingBindingResolution::Unmatched
            );
        }
    }

    #[test]
    fn presets_and_legacy_conversion_preserve_provider_identity() {
        assert_eq!(HostingPreset::Codeberg.provider(), HostingProvider::Forgejo);
        assert_eq!(
            HostingPreset::Codeberg.default_api_base(),
            Some("https://codeberg.org/api/v1")
        );
        assert_eq!(HostingPreset::Forgejo.default_api_base(), None);
        let original = account();
        let connection = HostingConnection::from_legacy_account(&original).unwrap();
        validate_connection(&connection).unwrap();
        assert_eq!(connection.legacy_account(), original);
        assert!(connection
            .capabilities
            .supports(HostingCapability::CheckRuns));

        let enterprise = HostingAccount {
            server: "github.enterprise.test".into(),
            ..original
        };
        let connection = HostingConnection::from_legacy_account(&enterprise).unwrap();
        assert_eq!(connection.preset, HostingPreset::GitHubEnterprise);
        assert_eq!(
            connection.api_base_url,
            "https://github.enterprise.test/api/v3"
        );
        validate_connection(&connection).unwrap();
    }

    #[test]
    fn enterprise_api_base_follows_the_server_kind() {
        let enterprise = HostingPreset::GitHubEnterprise;
        assert_eq!(
            enterprise.api_base_for("https://github.enterprise.test"),
            Some("https://github.enterprise.test/api/v3".into())
        );
        assert_eq!(
            enterprise.api_base_for("https://Octo.GHE.com"),
            Some("https://api.octo.ghe.com".into())
        );
        // Only a direct tenant of ghe.com has the split API origin.
        assert_eq!(
            enterprise.api_base_for("https://ghe.com"),
            Some("https://ghe.com/api/v3".into())
        );
        assert_eq!(
            enterprise.api_base_for("https://a.b.ghe.com"),
            Some("https://a.b.ghe.com/api/v3".into())
        );
        assert_eq!(
            enterprise.api_base_for("https://notghe.com"),
            Some("https://notghe.com/api/v3".into())
        );
        assert_eq!(enterprise.api_base_for("https://octo.ghe.com/path"), None);
        assert_eq!(
            HostingPreset::GitLabSelfManaged.api_base_for("https://gitlab.example.test"),
            Some("https://gitlab.example.test/api/v4".into())
        );
        assert_eq!(
            HostingPreset::Forgejo.api_base_for("https://forge.example.test:8443"),
            Some("https://forge.example.test:8443/api/v1".into())
        );
        assert_eq!(
            HostingPreset::GitHub.api_base_for("https://github.com"),
            Some("https://api.github.com".into())
        );
    }

    #[test]
    fn identities_connections_and_credentials_enforce_bounds_without_secret_diagnostics() {
        assert!(HostingConnectionId::new("My Work Forgejo").is_ok());
        assert!(HostingConnectionId::new(" My Work Forgejo").is_err());
        assert!(HostingConnectionId::new("My Work Forgejo ").is_err());
        assert!(HostingConnectionId::new("My\tWork Forgejo").is_err());
        assert_eq!(
            HostingRepositoryId::new("bad?identity"),
            Err(HostingError::InvalidIdentity)
        );
        assert!(HostingAccountId::new("a".repeat(MAX_HOSTING_TEXT_BYTES + 1)).is_err());
        let mut connection = HostingConnection::from_legacy_account(&account()).unwrap();
        connection.api_base_url = "https://user:secret@example.test/api".into();
        assert_eq!(
            validate_connection(&connection),
            Err(HostingError::InvalidAccount)
        );
        let credential = HostingCredential::new(b"private-secret".to_vec()).unwrap();
        assert_eq!(credential.expose(), b"private-secret");
        assert_eq!(format!("{credential:?}"), "HostingCredential([REDACTED])");
        assert!(HostingCredential::new(Vec::new()).is_err());
        assert!(HostingCredential::new(vec![b'x'; 16 * 1024 + 1]).is_err());
    }
}
