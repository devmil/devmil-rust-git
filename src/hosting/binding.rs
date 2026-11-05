//! Port-aware hosting binding and connection presets.
//!
//! [`crate::hosting::model::infer_hosting_binding`] compares host names only and keeps its
//! behaviour for existing consumers. The functions here keep explicit ports: a server is
//! `host[:port]`, lowercase, with the scheme's default port (`443`, `80`, `22`) removed, so
//! `https://forge.test:8443` and `https://forge.test` are different servers. An SSH port is a
//! transport detail and never decides which web connection a remote belongs to.

use super::model::{
    HostingCapabilitySet, HostingConnection, HostingConnectionId, HostingError, HostingPreset,
    HostingProvider, HostingRepositoryId,
};
use super::rest::safe_path_segment;
use crate::refs::RemoteId;

/// The URL scheme an authority belongs to, which selects the default port to remove.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AuthorityScheme {
    Https,
    Http,
    Ssh,
}

/// Normalizes `host[:port]`: lowercase, the scheme's default port removed, other ports kept.
/// Returns `None` for user info, separators, or a host outside ASCII letters, digits, `.` and `-`.
#[must_use]
pub fn normalize_authority(authority: &str, scheme: AuthorityScheme) -> Option<String> {
    if authority.is_empty() || authority.contains(['/', '\\', '@']) {
        return None;
    }
    let authority = authority.to_ascii_lowercase();
    let (host, port) = authority
        .rsplit_once(':')
        .map_or((authority.as_str(), None), |parts| {
            if parts.0.contains(':') || !parts.1.bytes().all(|byte| byte.is_ascii_digit()) {
                (authority.as_str(), None)
            } else {
                (parts.0, Some(parts.1))
            }
        });
    if host.is_empty()
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        return None;
    }
    let port = match (scheme, port) {
        (AuthorityScheme::Https, Some("443"))
        | (AuthorityScheme::Http, Some("80"))
        | (AuthorityScheme::Ssh, Some("22"))
        | (_, None) => None,
        (_, Some(port)) => Some(port),
    };
    Some(port.map_or_else(|| host.to_owned(), |port| format!("{host}:{port}")))
}

/// The port-aware server of an `https` web or API base (`https://host[:port][/path]`).
#[must_use]
pub fn connection_server_with_port(base: &str) -> Option<String> {
    let rest = base.strip_prefix("https://")?;
    normalize_authority(rest.split('/').next()?, AuthorityScheme::Https)
}

/// A hosting repository named by a Git remote URL.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RemoteLocation {
    /// Port-aware server (see the module documentation).
    pub server: String,
    /// `owner/name` (or a nested GitLab path) without `.git`.
    pub repository: String,
    /// The remote uses SSH (an `ssh://` URL or scp-like `host:path`).
    pub ssh_transport: bool,
}

/// Parses `https://`, `http://`, `ssh://` and scp-like (`user@host:owner/name.git`) remote
/// URLs. User info is ignored; queries, fragments and unsafe path segments are rejected.
#[must_use]
pub fn parse_remote_location(value: &str) -> Option<RemoteLocation> {
    if value.contains(['?', '#']) {
        return None;
    }
    let (server, path, ssh_transport) = if let Some((scheme, rest)) = value.split_once("://") {
        let scheme = match scheme {
            "https" => AuthorityScheme::Https,
            "http" => AuthorityScheme::Http,
            "ssh" => AuthorityScheme::Ssh,
            _ => return None,
        };
        let (authority, path) = rest.split_once('/')?;
        let authority = authority
            .rsplit_once('@')
            .map_or(authority, |parts| parts.1);
        (
            normalize_authority(authority, scheme)?,
            path,
            scheme == AuthorityScheme::Ssh,
        )
    } else {
        let (authority, path) = value.split_once(':')?;
        let authority = authority
            .rsplit_once('@')
            .map_or(authority, |parts| parts.1);
        (
            normalize_authority(authority, AuthorityScheme::Ssh)?,
            path,
            true,
        )
    };
    let repository = path
        .trim_matches('/')
        .strip_suffix(".git")
        .unwrap_or(path.trim_matches('/'));
    if repository.split('/').count() < 2 || !repository.split('/').all(safe_path_segment) {
        return None;
    }
    Some(RemoteLocation {
        server,
        repository: repository.to_owned(),
        ssh_transport,
    })
}

/// A remote bound to one connection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionBinding {
    pub remote: RemoteId,
    /// The remote's port-aware server.
    pub server: String,
    pub connection: HostingConnectionId,
    pub provider_repository: HostingRepositoryId,
    /// `<web base>/<repository>`.
    pub web_url: String,
}

/// The result of [`resolve_connection_binding`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConnectionBindingResolution {
    Unmatched,
    /// More than one enabled connection serves the first matching remote's server.
    Ambiguous {
        remote: RemoteId,
        server: String,
    },
    Bound(ConnectionBinding),
}

/// Binds the first remote (in caller order: normally upstream, `origin`, then the rest) whose
/// server an enabled connection serves. HTTP(S) remotes match the connection's port-aware web
/// server exactly; SSH remotes match its host, ignoring both ports. With `preferred`, only that
/// connection may bind (an ambiguity is resolved in its favour, other servers are skipped).
#[must_use]
pub fn resolve_connection_binding<'a>(
    connections: &[HostingConnection],
    remotes: impl IntoIterator<Item = (&'a RemoteId, &'a str)>,
    preferred: Option<&HostingConnectionId>,
) -> ConnectionBindingResolution {
    let host = |server: &str| server.split(':').next().unwrap_or(server).to_owned();
    for (remote, url) in remotes {
        let Some(location) = parse_remote_location(url) else {
            continue;
        };
        let Ok(provider_repository) = HostingRepositoryId::new(location.repository.clone()) else {
            continue;
        };
        let mut matches = connections.iter().filter(|connection| {
            connection.enabled
                && connection_server_with_port(&connection.web_base_url).is_some_and(|candidate| {
                    if location.ssh_transport {
                        host(&candidate) == host(&location.server)
                    } else {
                        candidate == location.server
                    }
                })
        });
        let bound = |connection: &HostingConnection| {
            ConnectionBindingResolution::Bound(ConnectionBinding {
                remote: remote.clone(),
                server: location.server.clone(),
                connection: connection.id.clone(),
                provider_repository: provider_repository.clone(),
                web_url: format!(
                    "{}/{}",
                    connection.web_base_url.trim_end_matches('/'),
                    location.repository
                ),
            })
        };
        if let Some(preferred) = preferred {
            if let Some(connection) = matches.find(|connection| &connection.id == preferred) {
                return bound(connection);
            }
            continue;
        }
        let Some(first) = matches.next() else {
            continue;
        };
        if matches.next().is_some() {
            return ConnectionBindingResolution::Ambiguous {
                remote: remote.clone(),
                server: location.server,
            };
        }
        return bound(first);
    }
    ConnectionBindingResolution::Unmatched
}

/// How a connection authenticates.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum HostingAuthenticationMethod {
    /// A public OAuth application registered by the user.
    OAuth,
    /// The token of an installed, signed-in `gh`.
    GitHubCli,
    /// The token of an installed, signed-in `glab`.
    GitLabCli,
}

/// The defaults a new connection of one preset starts with.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostingPresetDefaults {
    pub preset: HostingPreset,
    /// Normalized `https://host[:port]` web origin.
    pub web_base_url: String,
    pub api_base_url: String,
    /// The web origin is a GitHub Enterprise Cloud data-residency tenant (`<tenant>.ghe.com`)
    /// whose API lives on `api.<tenant>.ghe.com`.
    pub github_tenant: bool,
    pub capabilities: HostingCapabilitySet,
    /// Allowed methods; the first is the default.
    pub authentications: Vec<HostingAuthenticationMethod>,
}

/// Normalizes a user-entered web origin: a missing scheme means `https://`, the host is
/// lowercased, port `443` and trailing slashes are removed. Only `https` is accepted (also for
/// `localhost`), and the origin must have no path, user info, query or fragment.
#[must_use]
pub fn normalize_web_base(value: &str) -> Option<String> {
    let value = value.trim().trim_end_matches('/');
    let value = if value.contains("://") {
        value.to_owned()
    } else {
        format!("https://{value}")
    };
    if value.len() > 2048 || value.contains(['@', '?', '#']) {
        return None;
    }
    let authority = value.strip_prefix("https://")?;
    if authority.contains('/') {
        return None;
    }
    Some(format!(
        "https://{}",
        normalize_authority(authority, AuthorityScheme::Https)?
    ))
}

/// The defaults for a new connection of `preset`. Public presets (GitHub, GitLab, Codeberg) use
/// their fixed origins and ignore `web_base`; self-hosted presets require it and derive the API
/// base from it (`/api/v4` for GitLab, `/api/v1` for Forgejo, `/api/v3` for GitHub Enterprise
/// Server, `https://api.<tenant>.ghe.com` for GitHub Enterprise Cloud tenants).
///
/// # Errors
/// Returns [`HostingError::InvalidAccount`] when a self-hosted preset has no valid `https`
/// web origin.
pub fn hosting_preset_defaults(
    preset: HostingPreset,
    web_base: Option<&str>,
) -> Result<HostingPresetDefaults, HostingError> {
    let web_base_url = match preset.default_web_base() {
        Some(fixed) => fixed.to_owned(),
        None => web_base
            .and_then(normalize_web_base)
            .ok_or(HostingError::InvalidAccount)?,
    };
    let api_base_url = preset
        .api_base_for(&web_base_url)
        .ok_or(HostingError::InvalidAccount)?;
    let github_tenant = preset == HostingPreset::GitHubEnterprise
        && connection_server_with_port(&api_base_url)
            .is_some_and(|server| server.starts_with("api.") && server.ends_with(".ghe.com"));
    let provider = preset.provider();
    let mut authentications = vec![HostingAuthenticationMethod::OAuth];
    match provider {
        HostingProvider::GitHub => authentications.push(HostingAuthenticationMethod::GitHubCli),
        HostingProvider::GitLab => authentications.push(HostingAuthenticationMethod::GitLabCli),
        HostingProvider::Forgejo => {}
    }
    Ok(HostingPresetDefaults {
        preset,
        web_base_url,
        api_base_url,
        github_tenant,
        capabilities: HostingCapabilitySet::baseline(provider),
        authentications,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosting::model::HostingCapability;

    fn connection(id: &str, web: &str) -> HostingConnection {
        HostingConnection {
            id: HostingConnectionId::new(id).unwrap(),
            preset: HostingPreset::Forgejo,
            login: "me".into(),
            web_base_url: web.into(),
            api_base_url: format!("{web}/api/v1"),
            oauth_client_id: String::new(),
            enabled: true,
            capabilities: HostingCapabilitySet::baseline(HostingProvider::Forgejo),
        }
    }

    fn remote() -> RemoteId {
        RemoteId::from_bytes(b"origin".to_vec()).unwrap()
    }

    #[test]
    fn remote_locations_keep_ports_and_mark_ssh() {
        let parsed = parse_remote_location("https://Forge.Test:8443/team/project.git").unwrap();
        assert_eq!(parsed.server, "forge.test:8443");
        assert_eq!(parsed.repository, "team/project");
        assert!(!parsed.ssh_transport);
        assert_eq!(
            parse_remote_location("https://forge.test:443/a/b")
                .unwrap()
                .server,
            "forge.test"
        );
        let ssh = parse_remote_location("ssh://git@forge.test:2222/a/b").unwrap();
        assert_eq!(ssh.server, "forge.test:2222");
        assert!(ssh.ssh_transport);
        assert!(
            parse_remote_location("git@forge.test:a/b.git")
                .unwrap()
                .ssh_transport
        );
        for invalid in [
            "https://forge.test/a",
            "https://forge.test/a/../b",
            "https://forge.test/a/b?x=1",
            "ftp://forge.test/a/b",
            "https://forge.test/a/b:c",
        ] {
            assert_eq!(parse_remote_location(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn binding_matches_ports_for_http_and_hosts_for_ssh() {
        let plain = connection("plain", "https://forge.test");
        let ported = connection("ported", "https://forge.test:8443");
        let connections = [plain.clone(), ported.clone()];
        let bind = |url: &str, preferred: Option<&HostingConnectionId>| {
            let remote = remote();
            resolve_connection_binding(&connections, [(&remote, url)], preferred)
        };
        let ConnectionBindingResolution::Bound(binding) =
            bind("https://forge.test:8443/team/project.git", None)
        else {
            panic!("ported server binds");
        };
        assert_eq!(binding.connection, ported.id);
        assert_eq!(binding.server, "forge.test:8443");
        assert_eq!(binding.web_url, "https://forge.test:8443/team/project");
        assert!(matches!(
            bind("ssh://git@forge.test:2222/team/project", None),
            ConnectionBindingResolution::Ambiguous { .. }
        ));
        let ConnectionBindingResolution::Bound(binding) =
            bind("git@forge.test:team/project", Some(&plain.id))
        else {
            panic!("preferred connection binds");
        };
        assert_eq!(binding.connection, plain.id);
        assert_eq!(
            bind("https://other.test/team/project", None),
            ConnectionBindingResolution::Unmatched
        );
        let mut disabled = plain;
        disabled.enabled = false;
        let remote = remote();
        assert_eq!(
            resolve_connection_binding(
                &[disabled],
                [(&remote, "https://forge.test/team/project")],
                None
            ),
            ConnectionBindingResolution::Unmatched
        );
    }

    #[test]
    fn preset_defaults_derive_api_bases_and_authentication() {
        let github = hosting_preset_defaults(HostingPreset::GitHub, Some("ignored")).unwrap();
        assert_eq!(github.web_base_url, "https://github.com");
        assert_eq!(github.api_base_url, "https://api.github.com");
        assert_eq!(
            github.authentications,
            [
                HostingAuthenticationMethod::OAuth,
                HostingAuthenticationMethod::GitHubCli
            ]
        );
        assert!(github.capabilities.supports(HostingCapability::CheckRuns));
        let tenant =
            hosting_preset_defaults(HostingPreset::GitHubEnterprise, Some("Octo.GHE.com/"))
                .unwrap();
        assert_eq!(tenant.web_base_url, "https://octo.ghe.com");
        assert_eq!(tenant.api_base_url, "https://api.octo.ghe.com");
        assert!(tenant.github_tenant);
        let server = hosting_preset_defaults(
            HostingPreset::GitHubEnterprise,
            Some("https://ghe.test:443"),
        )
        .unwrap();
        assert_eq!(server.api_base_url, "https://ghe.test/api/v3");
        assert!(!server.github_tenant);
        let gitlab = hosting_preset_defaults(
            HostingPreset::GitLabSelfManaged,
            Some("https://git.test:8443"),
        )
        .unwrap();
        assert_eq!(gitlab.api_base_url, "https://git.test:8443/api/v4");
        assert_eq!(
            gitlab.authentications,
            [
                HostingAuthenticationMethod::OAuth,
                HostingAuthenticationMethod::GitLabCli
            ]
        );
        let forgejo = hosting_preset_defaults(HostingPreset::Forgejo, Some("forge.test")).unwrap();
        assert_eq!(forgejo.api_base_url, "https://forge.test/api/v1");
        assert_eq!(
            forgejo.authentications,
            [HostingAuthenticationMethod::OAuth]
        );
        assert!(!forgejo.capabilities.supports(HostingCapability::CheckRuns));
        for invalid in [
            None,
            Some("http://localhost:3000"),
            Some("https://forge.test/sub"),
            Some("https://user@forge.test"),
        ] {
            assert!(hosting_preset_defaults(HostingPreset::Forgejo, invalid).is_err());
        }
    }
}
