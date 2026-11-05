//! Bounded, host-scoped GitHub CLI credential handoff for desktop consumers.
//!
//! The caller runs these exact commands privately and never publishes parser output. The CLI
//! remains the credential owner; consumers request a fresh token for each operation.

use crate::hosting::model::HostingError;
use crate::process::{GitProcessCommand, ProcessArgument, SemanticTimeout};
use std::time::Duration;
use zeroize::Zeroize;

const MAX_TOKEN_BYTES: usize = 16 * 1024;

#[must_use]
/// # Panics
/// The fixed positive timeout is valid by construction.
pub fn github_cli_probe_command() -> GitProcessCommand {
    GitProcessCommand::new("gh")
        .arg(ProcessArgument::visible("--version"))
        .redact_output()
        .output_limit(1024)
        .semantic_timeout(
            SemanticTimeout::new(Duration::from_secs(3), "GitHub CLI detection")
                .expect("fixed positive timeout"),
        )
}

#[must_use]
/// # Panics
/// The fixed positive timeout is valid by construction.
pub fn gitlab_cli_probe_command() -> GitProcessCommand {
    GitProcessCommand::new("glab")
        .arg(ProcessArgument::visible("--version"))
        .redact_output()
        .output_limit(1024)
        .semantic_timeout(
            SemanticTimeout::new(Duration::from_secs(3), "GitLab CLI detection")
                .expect("fixed positive timeout"),
        )
}

/// Reads the GitLab CLI credential for a specific host through its documented config interface.
///
/// # Errors
/// Returns [`HostingError::InvalidAccount`] for an unsafe host. Stdout must be handled as secret.
/// # Panics
/// The fixed positive timeout is valid by construction.
pub fn gitlab_cli_token_command(host: &str) -> Result<GitProcessCommand, HostingError> {
    if !valid_cli_host(host) {
        return Err(HostingError::InvalidAccount);
    }
    Ok(GitProcessCommand::new("glab")
        .args([
            ProcessArgument::visible("config"),
            ProcessArgument::visible("get"),
            ProcessArgument::visible("token"),
            ProcessArgument::visible("--host"),
            ProcessArgument::visible(host),
        ])
        .redact_output()
        .output_limit(MAX_TOKEN_BYTES + 2)
        .semantic_timeout(
            SemanticTimeout::new(Duration::from_secs(8), "GitLab CLI credential")
                .expect("fixed positive timeout"),
        ))
}

/// Builds a command that asks `gh` for its active credential on exactly one host.
///
/// # Errors
/// Returns [`HostingError::InvalidAccount`] for an unsafe host. The command must run through a
/// private, output-redacted process runner; stdout is a secret.
/// # Panics
/// The fixed positive timeout is valid by construction.
pub fn github_cli_token_command(host: &str) -> Result<GitProcessCommand, HostingError> {
    if !valid_cli_host(host) {
        return Err(HostingError::InvalidAccount);
    }
    Ok(GitProcessCommand::new("gh")
        .args([
            ProcessArgument::visible("auth"),
            ProcessArgument::visible("token"),
            ProcessArgument::visible("--hostname"),
            ProcessArgument::visible(host),
        ])
        .redact_output()
        .output_limit(MAX_TOKEN_BYTES + 2)
        .semantic_timeout(
            SemanticTimeout::new(Duration::from_secs(8), "GitHub CLI credential")
                .expect("fixed positive timeout"),
        ))
}

/// Parses one bounded token line and clears the source buffer on every path.
///
/// # Errors
/// Returns [`HostingError::InvalidCredential`] if the CLI did not produce one safe token.
pub fn parse_github_cli_token(mut output: Vec<u8>) -> Result<String, HostingError> {
    let length = output.len();
    while output
        .last()
        .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
    {
        output.pop();
    }
    let valid = !output.is_empty()
        && length <= MAX_TOKEN_BYTES + 2
        && output.len() <= MAX_TOKEN_BYTES
        && output.iter().all(u8::is_ascii_graphic);
    if !valid {
        output.zeroize();
        return Err(HostingError::InvalidCredential);
    }
    String::from_utf8(output).map_err(|_| HostingError::InvalidCredential)
}

/// Parses the single token emitted by either supported CLI.
///
/// # Errors
/// Returns [`HostingError::InvalidCredential`] for malformed output.
pub fn parse_gitlab_cli_token(output: Vec<u8>) -> Result<String, HostingError> {
    parse_github_cli_token(output)
}

fn valid_cli_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.starts_with(['-', '.', ':'])
        && !host.ends_with(['-', '.', ':'])
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b':'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_command_is_exact_and_rejects_host_injection() {
        let command = github_cli_token_command("enterprise.example.test").unwrap();
        let arguments = command.activity_command().arguments;
        assert_eq!(
            arguments
                .iter()
                .map(|argument| argument.display.as_str())
                .collect::<Vec<_>>(),
            ["auth", "token", "--hostname", "enterprise.example.test"]
        );
        assert!(github_cli_token_command("example.test\n--show-token").is_err());
        assert!(github_cli_token_command("example.test/path").is_err());
    }

    #[test]
    fn token_parser_accepts_one_line_only() {
        assert_eq!(
            parse_github_cli_token(b"opaque-token\n".to_vec()).unwrap(),
            "opaque-token"
        );
        assert!(parse_github_cli_token(b"first\nsecond".to_vec()).is_err());
        assert!(parse_github_cli_token(vec![b'x'; MAX_TOKEN_BYTES + 3]).is_err());
    }

    #[test]
    fn gitlab_command_is_host_scoped() {
        let command = gitlab_cli_token_command("gitlab.example.test").unwrap();
        let arguments = command.activity_command().arguments;
        assert_eq!(
            arguments
                .iter()
                .map(|argument| argument.display.as_str())
                .collect::<Vec<_>>(),
            ["config", "get", "token", "--host", "gitlab.example.test"]
        );
        assert!(gitlab_cli_token_command("gitlab.example.test/other").is_err());
    }
}
