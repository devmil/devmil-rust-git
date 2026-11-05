//! Git credential protocol and SSH output parsing. These APIs never store credentials.

use crate::clone::{CloneSource, CloneSourceKind};
use std::fmt;
use zeroize::Zeroize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SshParseError;
impl fmt::Display for SshParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid SSH endpoint or key output")
    }
}
impl std::error::Error for SshParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_reject_protocol_injection_and_debug_never_exposes_secrets() {
        for invalid in ["", "bad\nfield", "bad\rfield", "bad\0field"] {
            assert!(Credentials::new(None, Some(invalid.to_owned()), None).is_none());
        }
        assert!(Credentials::new(None, None, None).is_none());
        assert!(Credentials::new(
            None,
            Some("x".repeat(Credentials::MAX_FIELD_BYTES + 1)),
            None
        )
        .is_none());
        let credentials = Credentials::new(
            Some("private-user".into()),
            Some("private-pass".into()),
            None,
        )
        .unwrap();
        assert_eq!(format!("{credentials:?}"), "Credentials([REDACTED])");
    }

    #[test]
    fn credential_fill_recognizes_bearer_and_keeps_first_values() {
        let credentials = parse_credential_fill(
            "username=first\nusername=second\npassword=secret\nauthtype=Bearer\n",
        )
        .unwrap();
        assert_eq!(credentials.username_for_askpass(), Some("first"));
        assert_eq!(credentials.password_for_askpass(), None);
        assert_eq!(credentials.token_for_askpass(), Some("secret"));
        assert_eq!(
            parse_credential_secret(b"protocol=https\npassword=opaque\n"),
            Some(b"opaque".to_vec())
        );
        assert!(parse_credential_secret(b"password=\n").is_none());
    }

    #[test]
    fn ssh_endpoints_accept_ports_and_ipv6_and_reject_option_hosts() {
        let source = CloneSource::parse("ssh://git@[2001:db8::1]:2222/repo").unwrap();
        assert_eq!(
            ssh_endpoint(&source, 255).unwrap(),
            SshEndpoint {
                host: "2001:db8::1".into(),
                port: 2222
            }
        );
        let source = CloneSource::parse("git@example.test:repo").unwrap();
        assert_eq!(ssh_endpoint(&source, 255).unwrap().port, 22);
        for source in [
            "ssh://git@-bad/repo",
            "ssh://git@host:0/repo",
            "https://host/repo",
        ] {
            assert!(ssh_endpoint(&CloneSource::parse(source).unwrap(), 255).is_err());
        }
        assert!(ssh_endpoint(&CloneSource::parse("ssh://host/repo").unwrap(), 2).is_err());
    }

    #[test]
    fn ssh_key_output_matches_endpoint_and_preserves_bounds() {
        let endpoint = SshEndpoint {
            host: "host".into(),
            port: 2222,
        };
        let output = b"# banner\nother ssh-ed25519 AAAA\n[host]:2222 ssh-ed25519 BBBB\n";
        let line = select_key_line(output, &endpoint, 1024).unwrap();
        assert_eq!(key_algorithm(&line, 64, 1024).unwrap(), "ssh-ed25519");
        assert!(select_key_line(output, &endpoint, 4).is_err());
        assert!(key_algorithm(b"host ssh-ed25519 BBBB unexpected", 64, 1024).is_err());
        assert_eq!(
            parse_fingerprint(b"256 SHA256:abcd host (ED25519)\n", 64).unwrap(),
            "SHA256:abcd"
        );
        assert!(parse_fingerprint(b"256 SHA256:abcd host\n", 5).is_err());
    }

    #[test]
    fn transfer_configuration_keeps_configured_helpers_unless_an_answer_is_supplied() {
        let settings = |authentication| {
            transfer_configuration(authentication)
                .iter()
                .map(|argument| argument.display().to_owned())
                .collect::<Vec<_>>()
        };
        let silent = ["-c", "core.askPass=", "-c", "credential.interactive=false"];
        assert_eq!(settings(TransferAuthentication::Default), silent);
        assert_eq!(
            settings(TransferAuthentication::AcceptHostKey {
                ssh_wrapper: Path::new("/private/wrapper"),
            }),
            silent
        );
        assert_eq!(
            settings(TransferAuthentication::ExternalAskpass {
                executable: Path::new("/private/askpass"),
            }),
            [
                "-c",
                "credential.helper=",
                "-c",
                "core.askPass=",
                "-c",
                "credential.interactive=true"
            ]
        );
    }

    #[test]
    fn transfer_environment_keeps_private_helpers_redacted() {
        let env = transfer_environment(
            true,
            Path::new("/private/reject"),
            TransferAuthentication::Default,
        )
        .unwrap();
        assert!(env
            .iter()
            .any(|entry| entry.name() == "SSH_ASKPASS_REQUIRE" && entry.value() == "never"));
        assert!(!format!("{env:?}").contains("/private/reject"));
        let env = transfer_environment(
            false,
            Path::new("/private/reject"),
            TransferAuthentication::Default,
        )
        .unwrap();
        assert_eq!(env.len(), 1);
    }
}

/// Secret credential material consumed only by an askpass adapter.
///
/// This value is never retained by the orchestrator or included in a request, snapshot,
/// activity, environment variable, or UI publication.
pub struct Credentials {
    username: Option<String>,
    password: Option<String>,
    token: Option<String>,
}

impl Credentials {
    pub const MAX_FIELD_BYTES: usize = 16_384;

    pub fn new(
        mut username: Option<String>,
        mut password: Option<String>,
        mut token: Option<String>,
    ) -> Option<Self> {
        let valid = |value: &Option<String>| {
            value.as_ref().is_none_or(|value| {
                !value.is_empty()
                    && value.len() <= Self::MAX_FIELD_BYTES
                    && !value.contains(['\0', '\r', '\n'])
            })
        };
        let valid = valid(&username)
            && valid(&password)
            && valid(&token)
            && (username.is_some() || password.is_some() || token.is_some());
        if !valid {
            username.zeroize();
            password.zeroize();
            token.zeroize();
            return None;
        }
        Some(Self {
            username,
            password,
            token,
        })
    }

    pub fn username_for_askpass(&self) -> Option<&str> {
        self.username.as_deref()
    }

    pub fn password_for_askpass(&self) -> Option<&str> {
        self.password.as_deref()
    }

    pub fn token_for_askpass(&self) -> Option<&str> {
        self.token.as_deref()
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Credentials([REDACTED])")
    }
}

impl Drop for Credentials {
    fn drop(&mut self) {
        self.username.zeroize();
        self.password.zeroize();
        self.token.zeroize();
    }
}

/// Parses Git credential-fill fields. Returned credentials zeroize on drop.
pub fn parse_credential_fill(output: &str) -> Option<Credentials> {
    let mut username = None;
    let mut password = None;
    let mut token = None;
    let mut bearer = false;
    for line in output.lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        match name {
            "username" if username.is_none() => username = Some(value.to_owned()),
            "password" if password.is_none() => password = Some(value.to_owned()),
            "oauth_token" | "token" if token.is_none() => token = Some(value.to_owned()),
            "authtype" if value.eq_ignore_ascii_case("bearer") => bearer = true,
            _ => {}
        }
    }
    if bearer && token.is_none() {
        token = password.take();
    }
    Credentials::new(username, password, token)
}

/// Extracts the first credential secret as bytes. The caller must clear the returned bytes.
pub fn parse_credential_secret(output: &[u8]) -> Option<Vec<u8>> {
    output
        .split(|byte| *byte == b'\n')
        .find_map(|line| {
            line.strip_prefix(b"oauth_token=")
                .or_else(|| line.strip_prefix(b"token="))
                .or_else(|| line.strip_prefix(b"password="))
        })
        .filter(|value| !value.is_empty())
        .map(<[u8]>::to_vec)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SshEndpoint {
    pub host: String,
    pub port: u16,
}

/// Parses bounded SSH protocol data.
///
/// # Errors
/// Rejects missing, malformed, or oversized fields.
pub fn ssh_endpoint(
    source: &CloneSource,
    maximum_host_bytes: usize,
) -> Result<SshEndpoint, SshParseError> {
    let value = source.as_os_str().to_str().ok_or(SshParseError)?;
    let (host, port) = match source.kind() {
        CloneSourceKind::Ssh => parse_ssh_url(value)?,
        CloneSourceKind::ScpLike => {
            let authority = value
                .split_once(':')
                .map(|(authority, _)| authority)
                .ok_or(SshParseError)?;
            (strip_user(authority), 22)
        }
        CloneSourceKind::Http
        | CloneSourceKind::Git
        | CloneSourceKind::File
        | CloneSourceKind::Local => return Err(SshParseError),
    };
    validate_host(host, maximum_host_bytes)?;
    Ok(SshEndpoint {
        host: host.to_owned(),
        port,
    })
}

fn parse_ssh_url(value: &str) -> Result<(&str, u16), SshParseError> {
    let authority = value
        .strip_prefix("ssh://")
        .and_then(|rest| rest.split('/').next())
        .ok_or(SshParseError)?;
    let authority = strip_user(authority);
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, suffix) = rest.split_once(']').ok_or(SshParseError)?;
        let port = if suffix.is_empty() {
            22
        } else {
            suffix
                .strip_prefix(':')
                .and_then(|value| value.parse::<u16>().ok())
                .filter(|value| *value != 0)
                .ok_or(SshParseError)?
        };
        Ok((host, port))
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        let port = port
            .parse::<u16>()
            .ok()
            .filter(|value| *value != 0)
            .ok_or(SshParseError)?;
        Ok((host, port))
    } else {
        Ok((authority, 22))
    }
}

fn strip_user(authority: &str) -> &str {
    authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
}

fn validate_host(host: &str, maximum_host_bytes: usize) -> Result<(), SshParseError> {
    if host.is_empty()
        || host.len() > maximum_host_bytes
        || host.starts_with('-')
        || host.chars().any(|character| {
            !(character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ':'))
        })
    {
        Err(SshParseError)
    } else {
        Ok(())
    }
}

/// Parses bounded SSH protocol data.
///
/// # Errors
/// Rejects missing, malformed, or oversized fields.
pub fn select_key_line(
    stdout: &[u8],
    endpoint: &SshEndpoint,
    maximum: usize,
) -> Result<Vec<u8>, SshParseError> {
    let expected = if endpoint.port == 22 {
        endpoint.host.clone()
    } else {
        format!("[{}]:{}", endpoint.host, endpoint.port)
    };
    stdout
        .split(|byte| *byte == b'\n')
        .find(|line| {
            !line.is_empty()
                && !line.starts_with(b"#")
                && line
                    .split(u8::is_ascii_whitespace)
                    .next()
                    .is_some_and(|scope| scope == expected.as_bytes())
        })
        .filter(|line| line.len() <= maximum)
        .map(<[u8]>::to_vec)
        .ok_or(SshParseError)
}

/// Parses bounded SSH protocol data.
///
/// # Errors
/// Rejects missing, malformed, or oversized fields.
pub fn key_algorithm(
    key_line: &[u8],
    maximum_algorithm: usize,
    maximum_key: usize,
) -> Result<String, SshParseError> {
    let mut fields = key_line.split(u8::is_ascii_whitespace);
    let _scope = fields.next();
    let algorithm = fields
        .next()
        .and_then(|field| std::str::from_utf8(field).ok())
        .filter(|field| {
            !field.is_empty()
                && field.len() <= maximum_algorithm
                && field.chars().all(|character| {
                    character.is_ascii_alphanumeric()
                        || matches!(character, '-' | '_' | '.' | '@' | '+')
                })
        })
        .ok_or(SshParseError)?;
    let key = fields
        .next()
        .filter(|field| !field.is_empty())
        .ok_or(SshParseError)?;
    if fields.next().is_some() || key.len() > maximum_key {
        return Err(SshParseError);
    }
    Ok(algorithm.to_owned())
}

/// Parses bounded SSH protocol data.
///
/// # Errors
/// Rejects missing, malformed, or oversized fields.
pub fn parse_fingerprint(stdout: &[u8], maximum: usize) -> Result<String, SshParseError> {
    let text = std::str::from_utf8(stdout).map_err(|_| SshParseError)?;
    text.split_ascii_whitespace()
        .find(|field| field.starts_with("SHA256:"))
        .filter(|field| field.len() <= maximum)
        .map(str::to_owned)
        .ok_or(SshParseError)
}

use crate::commands::remote::{RemoteArgument, RemoteEnvironment, RemoteOperationError};
use std::path::Path;

/// Caller-selected authentication transport. Credentials never enter environment values.
#[derive(Clone, Copy, Debug)]
pub enum TransferAuthentication<'a> {
    Default,
    ExternalAskpass { executable: &'a Path },
    AcceptHostKey { ssh_wrapper: &'a Path },
}

/// Builds the command-scoped Git configuration that precedes a transfer subcommand.
///
/// Default and accepted-host-key attempts keep the credential helpers of the user's Git
/// configuration and forbid them to prompt, so a stored credential answers as it does on the
/// command line. An external askpass attempt clears the helpers: the supplied answer must reach
/// the remote instead of a stored credential it has just refused. `core.askPass` is cleared in
/// every case because [`transfer_environment`] selects the askpass route.
pub fn transfer_configuration(authentication: TransferAuthentication<'_>) -> Vec<RemoteArgument> {
    let external = matches!(
        authentication,
        TransferAuthentication::ExternalAskpass { .. }
    );
    let mut settings = Vec::new();
    if external {
        settings.push("credential.helper=");
    }
    settings.push("core.askPass=");
    settings.push(if external {
        "credential.interactive=true"
    } else {
        "credential.interactive=false"
    });
    settings
        .into_iter()
        .flat_map(|setting| {
            [
                RemoteArgument::visible("-c"),
                RemoteArgument::visible(setting),
            ]
        })
        .collect()
}

/// Builds deterministic Git transfer environment without starting a process.
///
/// # Errors
/// Rejects paths that cannot be represented losslessly in the environment contract.
pub fn transfer_environment(
    transfer: bool,
    reject_helper: &Path,
    authentication: TransferAuthentication<'_>,
) -> Result<Vec<RemoteEnvironment>, RemoteOperationError> {
    let mut environment = vec![RemoteEnvironment::new("LC_ALL", "C")?];
    if !transfer {
        return Ok(environment);
    }
    environment.push(RemoteEnvironment::new("GIT_TERMINAL_PROMPT", "0")?);
    environment.push(RemoteEnvironment::new("GCM_INTERACTIVE", "Never")?);
    match authentication {
        TransferAuthentication::Default => {
            reject_askpass(&mut environment, reject_helper)?;
        }
        TransferAuthentication::ExternalAskpass { executable } => {
            environment.push(RemoteEnvironment::new(
                "SSH_ASKPASS",
                executable.as_os_str(),
            )?);
            environment.push(RemoteEnvironment::new("SSH_ASKPASS_REQUIRE", "force")?);
            environment.push(RemoteEnvironment::new(
                "GIT_ASKPASS",
                executable.as_os_str(),
            )?);
        }
        TransferAuthentication::AcceptHostKey { ssh_wrapper } => {
            reject_askpass(&mut environment, reject_helper)?;
            environment.push(RemoteEnvironment::new("GIT_SSH", ssh_wrapper.as_os_str())?);
            environment.push(RemoteEnvironment::new(
                "GIT_SSH_COMMAND",
                shell_command_path(ssh_wrapper),
            )?);
            environment.push(RemoteEnvironment::new("GIT_SSH_VARIANT", "ssh")?);
        }
    }
    Ok(environment)
}

fn reject_askpass(
    environment: &mut Vec<RemoteEnvironment>,
    reject_askpass: &Path,
) -> Result<(), RemoteOperationError> {
    environment.push(RemoteEnvironment::new(
        "GIT_ASKPASS",
        reject_askpass.as_os_str(),
    )?);
    environment.push(RemoteEnvironment::new(
        "SSH_ASKPASS",
        reject_askpass.as_os_str(),
    )?);
    environment.push(RemoteEnvironment::new("SSH_ASKPASS_REQUIRE", "never")?);
    Ok(())
}

#[cfg(unix)]
/// Quotes an executable path for Git's shell-interpreted SSH command setting.
pub fn shell_command_path(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[cfg(windows)]
/// Quotes an executable path for Git's shell-interpreted SSH command setting.
pub fn shell_command_path(path: &Path) -> String {
    format!("\"{}\"", path.to_string_lossy().replace('"', "\"\""))
}

#[cfg(not(any(unix, windows)))]
/// Quotes an executable path for Git's shell-interpreted SSH command setting.
pub fn shell_command_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Bytes an `ssh-keyscan` or `ssh-keygen` probe may print before it is cut off.
pub const SSH_HOST_KEY_OUTPUT_BYTES: usize = 64 * 1024;

/// How long one `ssh-keyscan` or `ssh-keygen` probe may run.
pub const SSH_HOST_KEY_PROBE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// `ssh-keyscan -T <seconds> -p <port> -- <host>`: scans `endpoint`'s host keys. Select the key
/// from the output with [`select_key_line`] and [`key_algorithm`].
#[must_use]
pub fn keyscan_arguments(endpoint: &SshEndpoint, timeout_seconds: u32) -> Vec<String> {
    vec![
        "-T".into(),
        timeout_seconds.to_string(),
        "-p".into(),
        endpoint.port.to_string(),
        "--".into(),
        endpoint.host.clone(),
    ]
}

/// `ssh-keygen -lf - -E sha256`: fingerprints the key line passed on standard input (see
/// [`fingerprint_input`]). Parse the output with [`parse_fingerprint`].
#[must_use]
pub fn fingerprint_arguments() -> [&'static str; 4] {
    ["-lf", "-", "-E", "sha256"]
}

/// The standard input for [`fingerprint_arguments`]: the key line and a newline.
#[must_use]
pub fn fingerprint_input(key_line: &[u8]) -> Vec<u8> {
    let mut input = key_line.to_vec();
    input.push(b'\n');
    input
}

/// A bounded `ssh-keyscan`/`ssh-keygen` probe: untranslated output (`LC_ALL=C`), at most
/// [`SSH_HOST_KEY_OUTPUT_BYTES`] of output and [`SSH_HOST_KEY_PROBE_DEADLINE`]. Host-key output
/// is public, so it stays visible in activity records.
///
/// # Panics
/// Never: the environment name and deadline are constants.
#[must_use]
pub fn ssh_host_key_probe_command<I, S>(
    executable: &std::ffi::OsStr,
    arguments: I,
    stdin: Vec<u8>,
) -> crate::process::GitProcessCommand
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    use crate::process::{GitProcessCommand, ProcessArgument, SemanticTimeout};
    GitProcessCommand::new(executable)
        .args(
            arguments
                .into_iter()
                .map(|argument| ProcessArgument::visible(argument.into())),
        )
        .stdin_bytes(stdin)
        .output_limit(SSH_HOST_KEY_OUTPUT_BYTES)
        .semantic_timeout(
            SemanticTimeout::new(SSH_HOST_KEY_PROBE_DEADLINE, "SSH host-key probe")
                .expect("constant timeout is valid"),
        )
        .env("LC_ALL", "C")
        .expect("constant environment is valid")
}
