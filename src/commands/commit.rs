//! `git commit` invocation and failure classification.
//!
//! The caller composes the message, owns confirmation and preconditions, and decides how to
//! present a failure. [`commit_invocation`] returns the single value used for preview and
//! execution. A failed commit is classified from Git's trace2 event stream
//! ([`CommitTrace`], [`classify_commit_trace`]) and, when the trace is unavailable, from its
//! `LC_ALL=C` diagnostics ([`classify_commit_stderr`]).

use super::invocation::{CommandArgument, GitInvocation};
use super::CommandValidationError;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Whether the commit is signed.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum CommitSigning {
    /// Follow `commit.gpgSign` and related configuration.
    #[default]
    UseGitConfiguration,
    /// `--gpg-sign`: sign with the configured key and format.
    Require,
    /// `--no-gpg-sign`.
    Disable,
}

/// Where Git reads the message.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum CommitMessageSource {
    /// `--file=-`: the caller passes the message on standard input
    /// ([`commit_invocation_with_message`] attaches it).
    #[default]
    Stdin,
    /// `--file=<path>`: Git reads the message from a file.
    File(PathBuf),
}

/// A commit request. The default is a plain commit of the index with the message on stdin.
/// Each flag maps to one Git option, so independent booleans are the clearest shape.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommitRequest {
    pub message: CommitMessageSource,
    pub amend: bool,
    /// `--no-verify`: skip the pre-commit and commit-msg hooks.
    pub no_verify: bool,
    pub signing: CommitSigning,
    /// `--signoff`.
    pub sign_off: bool,
    /// `--allow-empty`.
    pub allow_empty: bool,
    /// `--author=<author>`; the activity display shows `--author=<author>` only.
    pub author: Option<String>,
}

/// Builds `git commit` exactly as it runs. Arguments appear in the order
/// `commit --file=… [--amend] [--no-verify] [--gpg-sign|--no-gpg-sign] [--signoff]
/// [--allow-empty] [--author=…]`, and the environment pins `LC_ALL=C` so diagnostics are
/// classifiable. Git's editor is never opened because `--file` is always given.
///
/// # Errors
/// Rejects an empty author, or one containing NUL or a line break; rejects a relative message
/// file path.
pub fn commit_invocation(request: &CommitRequest) -> Result<GitInvocation, CommandValidationError> {
    let mut arguments = vec![CommandArgument::visible("commit")];
    match &request.message {
        CommitMessageSource::Stdin => arguments.push(CommandArgument::visible("--file=-")),
        CommitMessageSource::File(path) => {
            if !path.is_absolute() {
                return Err(CommandValidationError(
                    "commit message file must be an absolute path".into(),
                ));
            }
            let mut value = OsString::from("--file=");
            value.push(path.as_os_str());
            arguments.push(CommandArgument::redacted(value, "--file=<message>"));
        }
    }
    if request.amend {
        arguments.push(CommandArgument::visible("--amend"));
    }
    if request.no_verify {
        arguments.push(CommandArgument::visible("--no-verify"));
    }
    match request.signing {
        CommitSigning::UseGitConfiguration => {}
        CommitSigning::Require => arguments.push(CommandArgument::visible("--gpg-sign")),
        CommitSigning::Disable => arguments.push(CommandArgument::visible("--no-gpg-sign")),
    }
    if request.sign_off {
        arguments.push(CommandArgument::visible("--signoff"));
    }
    if request.allow_empty {
        arguments.push(CommandArgument::visible("--allow-empty"));
    }
    if let Some(author) = &request.author {
        if author.trim().is_empty() || author.contains(['\0', '\n', '\r']) {
            return Err(CommandValidationError(
                "commit author is empty or contains a control separator".into(),
            ));
        }
        arguments.push(CommandArgument::redacted(
            format!("--author={author}"),
            "--author=<author>",
        ));
    }
    Ok(GitInvocation::new(arguments).env("LC_ALL", "C"))
}

/// [`commit_invocation`] with `message` attached as standard input.
///
/// # Errors
/// As [`commit_invocation`]; also rejects a request whose message source is not stdin.
pub fn commit_invocation_with_message(
    request: &CommitRequest,
    message: Vec<u8>,
) -> Result<GitInvocation, CommandValidationError> {
    if request.message != CommitMessageSource::Stdin {
        return Err(CommandValidationError(
            "a commit message on stdin needs the stdin message source".into(),
        ));
    }
    Ok(commit_invocation(request)?.stdin_bytes(message))
}

/// Why a commit failed, as far as Git's own evidence shows.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CommitFailureKind {
    /// A hook (`pre-commit`, `commit-msg`, …) exited non-zero.
    Hook,
    /// The signing program (`gpg`, `gpgsm` or `ssh-keygen -Y sign`) failed.
    Signing,
    /// Anything else.
    Other,
}

/// A classified failure and the failing child's exit code when the trace recorded it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CommitFailure {
    pub kind: CommitFailureKind,
    pub exit_code: Option<i32>,
}

/// Bytes read at most from a trace file.
pub const MAX_COMMIT_TRACE_BYTES: u64 = 1024 * 1024;

/// A private, uniquely named trace2 event file that is removed on drop.
///
/// Pass [`Self::invocation`] (or add [`Self::path`] as `GIT_TRACE2_EVENT`) before execution and
/// call [`Self::failure`] after a failed commit. The trace path is redacted in activity records.
#[derive(Debug)]
pub struct CommitTrace {
    path: PathBuf,
}

impl CommitTrace {
    /// Creates an empty owner-only trace file inside `directory`.
    ///
    /// # Errors
    /// Returns the I/O error when no unique file could be created.
    pub fn create(directory: &Path) -> std::io::Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        for _ in 0..100 {
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!(
                "devmil-git-commit-trace-{}-{sequence}",
                std::process::id()
            ));
            match create_private(&path) {
                Ok(_) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "no unique commit trace name",
        ))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Adds `GIT_TRACE2_EVENT=<trace>` (redacted) to `invocation`.
    #[must_use]
    pub fn invocation(&self, invocation: GitInvocation) -> GitInvocation {
        invocation.env_redacted("GIT_TRACE2_EVENT", self.path.as_os_str())
    }

    /// Reads the bounded trace and classifies it. Read bytes are cleared before return.
    #[must_use]
    pub fn failure(&self) -> Option<CommitFailure> {
        use std::io::Read as _;
        let mut bytes = Vec::new();
        std::fs::File::open(&self.path)
            .ok()?
            .take(MAX_COMMIT_TRACE_BYTES)
            .read_to_end(&mut bytes)
            .ok()?;
        let failure = classify_commit_trace(&bytes);
        bytes.fill(0);
        failure
    }
}

impl Drop for CommitTrace {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
}

/// Classifies a failed commit from trace2 event output (`GIT_TRACE2_EVENT`).
///
/// Each line is parsed as JSON; malformed lines are skipped. Only children of the top-level Git
/// process (the `sid` of its `version` event) are considered. A non-zero `child_exit` of a
/// `hook`-class child is a hook failure and wins immediately; a non-zero exit of a signing
/// program (`gpg`/`gpgsm` invoked with `--status-fd=2 -bsau`, or `ssh-keygen -Y sign -n git`) is a
/// signing failure. Returns `None` when the trace shows neither.
#[must_use]
pub fn classify_commit_trace(bytes: &[u8]) -> Option<CommitFailure> {
    use serde_json::Value;
    let events = bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok());
    let mut root = None;
    let mut children = std::collections::BTreeMap::new();
    let mut signing = None;
    for event in events {
        let kind = event.get("event").and_then(Value::as_str);
        let sid = event.get("sid").and_then(Value::as_str);
        if root.is_none() {
            if kind == Some("version") {
                root = sid.map(str::to_owned);
            }
            continue;
        }
        if sid != root.as_deref() {
            continue;
        }
        let Some(id) = event.get("child_id").and_then(Value::as_i64) else {
            continue;
        };
        match kind {
            Some("child_start") => {
                children.remove(&id);
                let class = event.get("child_class").and_then(Value::as_str);
                let argv: Vec<&str> = event
                    .get("argv")
                    .and_then(Value::as_array)
                    .map(|values| values.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                if class == Some("hook") {
                    children.insert(id, CommitFailureKind::Hook);
                } else if is_signing_invocation(&argv) {
                    children.insert(id, CommitFailureKind::Signing);
                }
            }
            Some("child_exit") => {
                let Some(code) = event
                    .get("code")
                    .and_then(Value::as_i64)
                    .and_then(|code| i32::try_from(code).ok())
                else {
                    continue;
                };
                match children.remove(&id) {
                    _ if code == 0 => {}
                    Some(CommitFailureKind::Hook) => {
                        return Some(CommitFailure {
                            kind: CommitFailureKind::Hook,
                            exit_code: Some(code),
                        });
                    }
                    Some(CommitFailureKind::Signing) => {
                        signing = Some(CommitFailure {
                            kind: CommitFailureKind::Signing,
                            exit_code: Some(code),
                        });
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    signing
}

fn is_signing_invocation(argv: &[&str]) -> bool {
    let has = |value: &str| argv.contains(&value);
    let pair = |first: &str, second: &str| argv.windows(2).any(|w| w[0] == first && w[1] == second);
    (has("--status-fd=2") && has("-bsau")) || (pair("-Y", "sign") && pair("-n", "git"))
}

/// Classifies a failed commit from its standard error when no trace evidence exists. The
/// commit must have run under `LC_ALL=C` (as [`commit_invocation`] arranges) because the
/// patterns are Git's untranslated messages. An empty diagnostic with hooks enabled is a hook
/// failure: a failing hook that prints nothing leaves Git silent.
#[must_use]
pub fn classify_commit_stderr(stderr: &[u8], no_verify: bool) -> CommitFailureKind {
    let diagnostic = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if diagnostic.contains("failed to sign")
        || diagnostic.contains("gpg failed")
        || diagnostic.contains("gpg: signing failed")
        || diagnostic.contains("ssh-keygen")
        || diagnostic.contains("signing key")
    {
        CommitFailureKind::Signing
    } else if !no_verify && (diagnostic.contains("hook") || diagnostic.trim().is_empty()) {
        CommitFailureKind::Hook
    } else {
        CommitFailureKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_follow_the_documented_order_and_redact_the_author() {
        let invocation = commit_invocation(&CommitRequest {
            amend: true,
            no_verify: true,
            signing: CommitSigning::Disable,
            sign_off: true,
            allow_empty: true,
            author: Some("A U Thor <author@example.invalid>".into()),
            ..CommitRequest::default()
        })
        .unwrap();
        assert_eq!(
            invocation.display_arguments(),
            [
                "commit",
                "--file=-",
                "--amend",
                "--no-verify",
                "--no-gpg-sign",
                "--signoff",
                "--allow-empty",
                "--author=<author>"
            ]
        );
        assert_eq!(
            invocation.argv().last().unwrap(),
            "--author=A U Thor <author@example.invalid>"
        );
        assert_eq!(invocation.environment()[0].name(), "LC_ALL");
        assert!(commit_invocation(&CommitRequest {
            author: Some("bad\nauthor".into()),
            ..CommitRequest::default()
        })
        .is_err());
        assert!(commit_invocation(&CommitRequest {
            message: CommitMessageSource::File("relative".into()),
            ..CommitRequest::default()
        })
        .is_err());
    }

    #[test]
    fn trace_classifier_uses_root_children_and_tolerates_noise() {
        let trace = br#"{"event":"version","sid":"root","evt":"3"}
not json at all
{"event":"child_start","sid":"root","child_id":0,"child_class":"?","argv":["gpg","--status-fd=2","-bsau","KEY"]}
{"event":"child_exit","sid":"root","child_id":0,"code":2}
{"event":"child_start","sid":"root/other","child_id":1,"child_class":"hook","argv":["x"]}
{"event":"child_exit","sid":"root/other","child_id":1,"code":1}
"#;
        assert_eq!(
            classify_commit_trace(trace),
            Some(CommitFailure {
                kind: CommitFailureKind::Signing,
                exit_code: Some(2)
            })
        );
        let ssh = br#"{"event":"version","sid":"r"}
{"event":"child_start","sid":"r","child_id":3,"child_class":"?","argv":["ssh-keygen","-Y","sign","-n","git","-f","key"]}
{"event":"child_exit","sid":"r","child_id":3,"code":255}
"#;
        assert_eq!(
            classify_commit_trace(ssh).map(|failure| failure.kind),
            Some(CommitFailureKind::Signing)
        );
        assert_eq!(classify_commit_trace(b"{}\n"), None);
        assert_eq!(
            classify_commit_stderr(b"error: gpg failed to sign the data", false),
            CommitFailureKind::Signing
        );
        assert_eq!(classify_commit_stderr(b"", false), CommitFailureKind::Hook);
        assert_eq!(classify_commit_stderr(b"", true), CommitFailureKind::Other);
    }
}
