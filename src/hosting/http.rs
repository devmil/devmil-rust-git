//! Hosting HTTP command preparation. Consumers execute commands through their activity adapter.
use super::model::HostingError;
use super::providers::{provider_plugin, HostingHttpRequest, HostingHttpResponse};
use crate::activity::ActivityStatus;
use crate::process::{GitProcessCommand, PrivateActivityResult, ProcessArgument, SemanticTimeout};
use std::ffi::OsStr;
use std::time::Duration;
pub const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

/// The marker every hosting curl command appends after the body with `--write-out`
/// (`\n<STATUS_MARKER>:<http code>`), so the status can be split from arbitrary body bytes.
pub const STATUS_MARKER: &str = "DEVMIL_GIT_STATUS";
/// The marker review commands wrote before [`STATUS_MARKER`] was shared. Parsers still accept it
/// so recorded fixtures and fake transports keep working.
pub const LEGACY_REVIEW_STATUS_MARKER: &str = "QSPLINE_STATUS";

/// Finds the last `\n<marker>:<status>` trailer and returns the body length and the status.
#[must_use]
pub fn split_status_marker(output: &[u8]) -> Option<(usize, u16)> {
    [STATUS_MARKER, LEGACY_REVIEW_STATUS_MARKER]
        .into_iter()
        .filter_map(|marker| {
            let marker = format!("\n{marker}:");
            let marker = marker.as_bytes();
            let split = output
                .windows(marker.len())
                .rposition(|window| window == marker)?;
            Some((split, marker.len()))
        })
        .max_by_key(|(split, _)| *split)
        .and_then(|(split, length)| {
            std::str::from_utf8(&output[split + length..])
                .ok()?
                .trim()
                .parse::<u16>()
                .ok()
                .map(|status| (split, status))
        })
}
/// Builds a private-output curl request with credentials on stdin.
/// # Errors
/// Rejects invalid endpoints and header credentials.
pub fn curl_get_command(
    executable: &OsStr,
    request: &HostingHttpRequest,
    credential: &[u8],
) -> Result<GitProcessCommand, HostingError> {
    if !valid_api_base(&request.base_url)
        || !request.path.starts_with('/')
        || credential.is_empty()
        || credential
            .iter()
            .any(|byte| !byte.is_ascii_graphic() || matches!(byte, b'"' | b'\\'))
    {
        return Err(HostingError::InvalidCredential);
    }
    let mut config = b"silent\nshow-error\nheader = \"".to_vec();
    config.extend_from_slice(
        provider_plugin(request.provider)
            .api_authorization_header_prefix()
            .as_bytes(),
    );
    config.extend_from_slice(credential);
    config.extend_from_slice(b"\"\n");
    let url = format!("{}{}", request.base_url.trim_end_matches('/'), request.path);
    let command = GitProcessCommand::new(executable)
        .args([
            ProcessArgument::visible("--config"),
            ProcessArgument::visible("-"),
            ProcessArgument::visible("--connect-timeout"),
            ProcessArgument::visible("5"),
            ProcessArgument::visible("--max-time"),
            ProcessArgument::visible("15"),
            ProcessArgument::visible("--url"),
            ProcessArgument::visible(url),
            ProcessArgument::visible("--write-out"),
            ProcessArgument::visible(format!("\n{STATUS_MARKER}:%{{http_code}}")),
        ])
        .stdin_bytes(config)
        .redact_output()
        .output_limit(MAX_RESPONSE_BYTES + 64)
        .semantic_timeout(
            SemanticTimeout::new(Duration::from_secs(20), "hosting API request")
                .map_err(|_| HostingError::InvalidAccount)?,
        );
    Ok(command)
}
/// # Errors
/// Rejects malformed, unsafe, oversized, or unauthorized provider output.
pub fn parse_curl_response(output: &[u8]) -> Result<HostingHttpResponse, HostingError> {
    let (split, status) = split_status_marker(output).ok_or(HostingError::MalformedResponse)?;
    if split > MAX_RESPONSE_BYTES {
        return Err(HostingError::MalformedResponse);
    }
    Ok(HostingHttpResponse {
        status,
        body: output[..split].to_vec(),
    })
}

pub fn valid_api_base(base: &str) -> bool {
    let Some(rest) = base.strip_prefix("https://") else {
        return false;
    };
    let rest = rest.trim_end_matches('/');
    !rest.is_empty()
        && base.len() <= 2_048
        && !rest.starts_with('-')
        && !rest.contains(['@', '?', '#'])
        && rest
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// Builds a private curl `POST` of an `application/x-www-form-urlencoded` body (for OAuth token,
/// refresh and device-authorization requests). The form travels on stdin (`--data-binary @-`), so
/// secrets never appear in arguments; output is redacted from activity and bounded.
///
/// # Errors
/// Rejects a URL that is not `https`, is longer than 8 KiB or contains a line break.
pub fn curl_form_command(
    executable: &OsStr,
    url: &str,
    form: &[u8],
) -> Result<GitProcessCommand, HostingError> {
    if !url.starts_with("https://") || url.len() > 8192 || url.contains(['\r', '\n']) {
        return Err(HostingError::InvalidAccount);
    }
    form_command(executable, url, form)
}

/// [`curl_form_command`] for a plain-HTTP endpoint on the loopback interface
/// (`http://127.0.0.1:<port>/…` or `http://localhost:<port>/…`), such as a local test server or
/// a development provider. Requests to real providers always use [`curl_form_command`].
///
/// # Errors
/// Rejects any other URL, one longer than 8 KiB, or one containing a line break or user info.
pub fn curl_form_loopback_command(
    executable: &OsStr,
    url: &str,
    form: &[u8],
) -> Result<GitProcessCommand, HostingError> {
    let rest = url
        .strip_prefix("http://127.0.0.1:")
        .or_else(|| url.strip_prefix("http://localhost:"))
        .ok_or(HostingError::InvalidAccount)?;
    let port = rest.split('/').next().unwrap_or_default();
    if port.is_empty()
        || !port.bytes().all(|byte| byte.is_ascii_digit())
        || url.len() > 8192
        || url.contains(['\r', '\n', '@'])
    {
        return Err(HostingError::InvalidAccount);
    }
    form_command(executable, url, form)
}

fn form_command(
    executable: &OsStr,
    url: &str,
    form: &[u8],
) -> Result<GitProcessCommand, HostingError> {
    Ok(GitProcessCommand::new(executable)
        .args([
            ProcessArgument::visible("--silent"),
            ProcessArgument::visible("--show-error"),
            ProcessArgument::visible("--connect-timeout"),
            ProcessArgument::visible("5"),
            ProcessArgument::visible("--max-time"),
            ProcessArgument::visible("15"),
            ProcessArgument::visible("--header"),
            ProcessArgument::visible("Content-Type: application/x-www-form-urlencoded"),
            ProcessArgument::visible("--data-binary"),
            ProcessArgument::visible("@-"),
            ProcessArgument::visible("--url"),
            ProcessArgument::visible(url),
            ProcessArgument::visible("--write-out"),
            ProcessArgument::visible(format!("\n{STATUS_MARKER}:%{{http_code}}")),
        ])
        .stdin_bytes(form.to_vec())
        .redact_output()
        .output_limit(MAX_RESPONSE_BYTES + 64)
        .semantic_timeout(
            SemanticTimeout::new(Duration::from_secs(20), "hosting OAuth request")
                .map_err(|_| HostingError::InvalidAccount)?,
        ))
}

/// Why a provider request produced no usable answer. [`Self::into_error`] carries only the fixed
/// [`Self::reason`], never provider or user content.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TransportFailure {
    NotFound,
    Redirected,
    ServerError,
    Network,
    Certificate,
    TimedOut,
}

impl TransportFailure {
    pub const ALL: [Self; 6] = [
        Self::NotFound,
        Self::Redirected,
        Self::ServerError,
        Self::Network,
        Self::Certificate,
        Self::TimedOut,
    ];

    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::NotFound => "provider address not found",
            Self::Redirected => "provider redirected the request",
            Self::ServerError => "provider server error",
            Self::Network => "provider not reachable",
            Self::Certificate => "provider certificate rejected",
            Self::TimedOut => "provider request timed out",
        }
    }

    /// Recovers the failure from [`HostingError::Unavailable`] text produced by
    /// [`Self::into_error`].
    #[must_use]
    pub fn from_reason(reason: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|failure| failure.reason() == reason)
    }

    /// Classifies HTTP statuses that are not authentication or rate-limit answers.
    #[must_use]
    pub const fn from_status(status: u16) -> Option<Self> {
        match status {
            300..=399 => Some(Self::Redirected),
            404 | 410 => Some(Self::NotFound),
            500..=599 => Some(Self::ServerError),
            _ => None,
        }
    }

    /// Classifies curl's documented exit codes.
    #[must_use]
    pub const fn from_curl_exit(code: i32) -> Option<Self> {
        match code {
            // Proxy or host not resolved, connection refused, empty or broken reply.
            5..=7 | 52 | 55 | 56 => Some(Self::Network),
            28 => Some(Self::TimedOut),
            // Handshake, peer certificate, CA bundle and pinning failures.
            35 | 51 | 53 | 54 | 58..=60 | 66 | 77 | 80 | 82 | 83 | 90 | 91 | 98 => {
                Some(Self::Certificate)
            }
            _ => None,
        }
    }

    #[must_use]
    pub fn into_error(self) -> HostingError {
        HostingError::Unavailable(self.reason().into())
    }
}

/// Converts a finished private curl result (from [`curl_get_command`] or [`curl_form_command`])
/// into a response, classifying curl failures with [`TransportFailure`].
///
/// # Errors
/// Returns [`HostingError::Canceled`], a transport failure, or a malformed-response error.
pub fn curl_result_response(
    mut result: PrivateActivityResult,
) -> Result<HostingHttpResponse, HostingError> {
    match result.activity.status {
        ActivityStatus::Succeeded { .. } => parse_curl_response(&result.stdout.take_bytes()),
        ActivityStatus::Canceled { .. } => Err(HostingError::Canceled),
        ActivityStatus::TimedOut { .. } => Err(TransportFailure::TimedOut.into_error()),
        ActivityStatus::Failed {
            exit_code: Some(code),
        } => Err(TransportFailure::from_curl_exit(code).map_or_else(
            || HostingError::Unavailable("transport unavailable".into()),
            TransportFailure::into_error,
        )),
        ActivityStatus::LaunchError { .. }
        | ActivityStatus::Failed { exit_code: None }
        | ActivityStatus::Running => Err(HostingError::Unavailable("transport unavailable".into())),
    }
}

/// Maps a response status: 2xx passes, 401/403/429 become their errors, and other statuses
/// become a [`TransportFailure`] or a generic unavailable error.
///
/// # Errors
/// Returns the mapped error for every non-2xx status.
pub fn require_success_status(status: u16) -> Result<(), HostingError> {
    match status {
        200..=299 => Ok(()),
        401 => Err(HostingError::Unauthorized),
        403 => Err(HostingError::PermissionDenied),
        429 => Err(HostingError::RateLimited),
        status => Err(TransportFailure::from_status(status).map_or_else(
            || HostingError::Unavailable("provider unavailable".into()),
            TransportFailure::into_error,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_markers_split_bodies_and_legacy_review_output() {
        assert_eq!(
            split_status_marker(b"{}\nDEVMIL_GIT_STATUS:201"),
            Some((2, 201))
        );
        assert_eq!(
            split_status_marker(b"x\nQSPLINE_STATUS:200\n"),
            Some((1, 200))
        );
        // The last trailer wins even when the body contains a marker-like line.
        assert_eq!(
            split_status_marker(b"a\nQSPLINE_STATUS:1\nDEVMIL_GIT_STATUS:404"),
            Some((18, 404))
        );
        assert_eq!(split_status_marker(b"no marker"), None);
        assert_eq!(split_status_marker(b"\nDEVMIL_GIT_STATUS:abc"), None);
    }

    #[test]
    fn transport_failures_round_trip_and_classify_exits_and_statuses() {
        for failure in TransportFailure::ALL {
            assert_eq!(
                TransportFailure::from_reason(failure.reason()),
                Some(failure)
            );
        }
        assert_eq!(
            TransportFailure::from_curl_exit(6),
            Some(TransportFailure::Network)
        );
        assert_eq!(
            TransportFailure::from_curl_exit(28),
            Some(TransportFailure::TimedOut)
        );
        assert_eq!(
            TransportFailure::from_curl_exit(60),
            Some(TransportFailure::Certificate)
        );
        assert_eq!(TransportFailure::from_curl_exit(3), None);
        assert_eq!(require_success_status(204), Ok(()));
        assert_eq!(require_success_status(401), Err(HostingError::Unauthorized));
        assert_eq!(
            require_success_status(502),
            Err(TransportFailure::ServerError.into_error())
        );
        assert!(curl_form_command(OsStr::new("curl"), "http://example.test", b"a=b").is_err());
        let command = curl_form_command(
            OsStr::new("curl"),
            "https://example.test/token",
            b"secret=1",
        )
        .unwrap()
        .activity_command();
        assert!(command
            .arguments
            .iter()
            .all(|argument| !argument.display.contains("secret")));
    }
}
