//! Transfer stderr classification and bounded diagnostics.

use crate::diagnosis::{diagnose, shows, DiagnosisKind};

pub const MAX_TRANSFER_DETAIL_BYTES: usize = 512;

/// Why one transfer command failed. Carries no credential material.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferFailureKind {
    /// The Git process could not be launched.
    Launch,
    /// Git ran and reported a failure that is neither authentication nor host-key related.
    Git,
    /// Git could not read an object needed to complete the transfer.
    MissingObject,
    /// A credential was required and could not be obtained noninteractively.
    Authentication,
    /// SSH host-key verification failed.
    HostKey,
    /// Execution observed the cancellation token; the child tree was killed and reaped.
    Canceled,
}

/// Classifies a failed transfer from its bounded stderr through the
/// [diagnosis table](crate::diagnosis::RULES). The patterns assume `LC_ALL=C`,
/// which callers must set on their Git commands.
#[must_use]
pub fn classify_transfer_stderr(stderr: &[u8]) -> TransferFailureKind {
    match diagnose(stderr).map(|diagnosis| diagnosis.kind) {
        Some(DiagnosisKind::HostKey) => TransferFailureKind::HostKey,
        Some(DiagnosisKind::MissingObject) => TransferFailureKind::MissingObject,
        Some(DiagnosisKind::Authentication) => TransferFailureKind::Authentication,
        _ => TransferFailureKind::Git,
    }
}

/// Whether a failed push was refused because a `--force-with-lease` expectation no longer
/// matched the remote (`[rejected] ... (stale info)`). The remote ref may have moved or may
/// already be gone; callers must query the remote before treating either as the outcome.
/// The pattern assumes `LC_ALL=C`.
#[must_use]
pub fn is_stale_lease_rejection(stderr: &[u8]) -> bool {
    shows(DiagnosisKind::StaleLease, stderr)
}

/// Extracts a bounded, control-free, nonempty failure detail from transfer stderr.
/// Input must already be redacted before publishing the returned text to logs or UI.
#[must_use]
pub fn transfer_failure_detail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let candidate = text
        .lines()
        .find(|line| {
            let line = line.trim_start();
            line.starts_with("fatal:") || line.starts_with("error:")
        })
        .or_else(|| text.lines().rfind(|line| !line.trim().is_empty()));
    let mut detail = String::new();
    for character in candidate.unwrap_or_default().chars() {
        let character = if character.is_control() && character != '\t' {
            '\u{fffd}'
        } else {
            character
        };
        if detail.len() + character.len_utf8() > MAX_TRANSFER_DETAIL_BYTES {
            break;
        }
        detail.push(character);
    }
    let detail = detail.trim().to_owned();
    if detail.is_empty() {
        "Git reported a failure".to_owned()
    } else {
        detail
    }
}

/// Classifies clone diagnostics, including Git's clone-specific credential prompts.
/// Callers must set `LC_ALL=C`. This reads private output but returns no input text.
#[must_use]
pub fn classify_clone_stderr(private_stderr: &[u8]) -> TransferFailureKind {
    if contains_ascii_case_insensitive(private_stderr, b"host key verification failed")
        || contains_ascii_case_insensitive(private_stderr, b"authenticity of host")
    {
        TransferFailureKind::HostKey
    } else if contains_ascii_case_insensitive(private_stderr, b"authentication failed")
        || contains_ascii_case_insensitive(private_stderr, b"permission denied")
        || contains_ascii_case_insensitive(private_stderr, b"could not read username")
        || contains_ascii_case_insensitive(private_stderr, b"unable to get password")
        || contains_ascii_case_insensitive(private_stderr, b"unable to get username")
    {
        TransferFailureKind::Authentication
    } else {
        TransferFailureKind::Git
    }
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_and_transfer_diagnostics_preserve_their_prompt_patterns() {
        for diagnostic in [
            b"Authentication failed".as_slice(),
            b"Permission DENIED",
            b"could not read Username",
            b"unable to get password",
        ] {
            assert_eq!(
                classify_clone_stderr(diagnostic),
                TransferFailureKind::Authentication
            );
            assert_eq!(
                classify_transfer_stderr(diagnostic),
                TransferFailureKind::Authentication
            );
        }
        assert_eq!(
            classify_clone_stderr(b"unable to get password"),
            TransferFailureKind::Authentication
        );
        assert_eq!(
            classify_clone_stderr(b"authenticity of host"),
            TransferFailureKind::HostKey
        );
        assert_eq!(
            classify_transfer_stderr(b"Host key verification failed"),
            TransferFailureKind::HostKey
        );
        assert_eq!(
            classify_transfer_stderr(b"terminal prompts disabled"),
            TransferFailureKind::Authentication
        );
        for diagnostic in [
            b"error: The requested URL returned error: 401 while accessing".as_slice(),
            b"error: RPC failed; HTTP 401 curl 22 The requested URL returned error: 401",
            b"fatal: HTTP request failed: 401 Unauthorized",
        ] {
            assert_eq!(
                classify_transfer_stderr(diagnostic),
                TransferFailureKind::Authentication
            );
        }
        for diagnostic in [
            b"unable to read askpass response".as_slice(),
            b"could not read askpass response",
        ] {
            assert_eq!(
                classify_transfer_stderr(diagnostic),
                TransferFailureKind::Authentication
            );
        }
        assert_eq!(
            classify_clone_stderr(b"fatal: disk full"),
            TransferFailureKind::Git
        );
    }

    #[test]
    fn failure_detail_prefers_diagnostic_and_bounds_utf8() {
        assert_eq!(
            transfer_failure_detail(b"warning: x\nfatal: failure\n"),
            "fatal: failure"
        );
        assert_eq!(transfer_failure_detail(b""), "Git reported a failure");
        let detail = transfer_failure_detail("€".repeat(300).as_bytes());
        assert_eq!(detail.len(), 510);
        assert!(!transfer_failure_detail(b"fatal: bad\x07ring").contains('\x07'));
    }

    #[test]
    fn stale_lease_rejection_is_distinct_from_other_rejections() {
        assert!(is_stale_lease_rejection(
            b"To ssh://example.test/repo.git\n ! [rejected]        (delete) -> topic (stale info)\nerror: failed to push some refs\n"
        ));
        assert!(is_stale_lease_rejection(
            b" ! [rejected]        main -> main (stale info)\r\n"
        ));
        for diagnostic in [
            b" ! [rejected]        main -> main (fetch first)\n".as_slice(),
            b" ! [rejected]        main -> main (non-fast-forward)\n",
            b" ! [remote rejected] main -> main (stale info)\n",
            b"remote: stale info\n",
        ] {
            assert!(!is_stale_lease_rejection(diagnostic));
        }
    }

    #[test]
    fn missing_local_objects_are_distinct_from_remote_rejection() {
        for diagnostic in [
            b"error: unable to read sha1 file of VERSION (a464f20c)".as_slice(),
            b"error: missing blob a464f20c",
            b"error: object file .git/objects/aa/bb is empty",
        ] {
            assert_eq!(
                classify_transfer_stderr(diagnostic),
                TransferFailureKind::MissingObject
            );
        }
        assert_eq!(
            classify_transfer_stderr(b"fatal: bad object HEAD"),
            TransferFailureKind::Git
        );
        assert_eq!(
            classify_transfer_stderr(b"remote: error: branch update rejected"),
            TransferFailureKind::Git
        );
    }
}
