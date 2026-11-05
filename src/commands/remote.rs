//! Typed remote commands, refspecs, URL redaction, and transfer argv.

use crate::activity::{ActivityArgument, ActivityCommand, ActivityEnvironment};
use crate::transfer::MAX_TRANSFER_DETAIL_BYTES;
use crate::types::{ObjectId, RefId};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};

/// Remote names become a config subsection and the `refs/remotes/<name>/` namespace.
pub const MAX_REMOTE_NAME_BYTES: usize = 255;
/// Configured remote URLs stay bounded; credential material is never required in-band.
pub const MAX_REMOTE_URL_BYTES: usize = 4 * 1024;
/// Refspecs carried by one push.
pub const MAX_PUSH_REFSPECS: usize = 64;
/// Bound on the argument vector of one remote command.
pub const MAX_REMOTE_ARGUMENTS: usize = 96;
/// Bound on the environment overrides of one remote command.
pub const MAX_REMOTE_ENVIRONMENT: usize = 16;

/// A validated remote name. Validation is deliberately stricter than Git's: names are ASCII
/// alphanumerics plus `.`, `_`, `-`, never start with `.` or `-`, and never contain `..` or
/// end in `.lock`, so the name is always safe as argv and as a ref namespace component.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RemoteName(String);

impl RemoteName {
    /// # Errors
    ///
    /// Returns [`RemoteOperationErrorKind::InvalidName`] for an empty, oversized, or unsafe name.
    pub fn new(value: impl Into<String>) -> Result<Self, RemoteOperationError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= MAX_REMOTE_NAME_BYTES
            && !value.starts_with(['.', '-'])
            && !value.contains("..")
            && !std::path::Path::new(&value)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("lock"))
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        if valid {
            Ok(Self(value))
        } else {
            Err(RemoteOperationError::new(
                RemoteOperationErrorKind::InvalidName,
                "remote name is empty, oversized, or contains unsafe characters",
            ))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RemoteName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A validated remote URL carrying its credential-free display form. The raw value reaches the
/// Git process verbatim; only `display` is published to activity when the URL embeds
/// credentials, mirroring the redaction rules of the shared reference parser.
#[derive(Clone, Eq, PartialEq)]
pub struct RemoteUrl {
    raw: String,
    display: String,
}

impl fmt::Debug for RemoteUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteUrl")
            .field("display", &self.display)
            .finish_non_exhaustive()
    }
}

impl RemoteUrl {
    /// # Errors
    ///
    /// Returns [`RemoteOperationErrorKind::InvalidUrl`] for an empty, oversized, or
    /// control-character URL.
    pub fn new(value: impl Into<String>) -> Result<Self, RemoteOperationError> {
        let raw = value.into();
        if raw.is_empty()
            || raw.len() > MAX_REMOTE_URL_BYTES
            || raw.bytes().any(|byte| byte < b' ' || byte == 0x7f)
        {
            return Err(RemoteOperationError::new(
                RemoteOperationErrorKind::InvalidUrl,
                "remote URL is empty, oversized, or contains control characters",
            ));
        }
        let display = credential_free_url(&raw);
        Ok(Self { raw, display })
    }

    /// The configured URL exactly as Git receives it. Never publish this value when
    /// [`RemoteUrl::is_sensitive`] holds.
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// Credential-free display form safe for activity and preflight surfaces.
    #[must_use]
    pub fn display(&self) -> &str {
        &self.display
    }

    /// Whether the URL embeds credentials and must be redacted in activity.
    #[must_use]
    pub fn is_sensitive(&self) -> bool {
        self.raw != self.display
    }
}

/// The pull integration strategy, typed so argv and preflight provably agree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PullStrategy {
    /// `--ff-only`: refuse any non-fast-forward integration.
    FastForwardOnly,
    /// `--ff`: fast-forward when possible, otherwise create a merge commit.
    FastForwardIfPossible,
    /// `--rebase`: replay local commits on top of the fetched tip.
    Rebase,
}

impl PullStrategy {
    const fn flag(self) -> &'static str {
        match self {
            Self::FastForwardOnly => "--ff-only",
            Self::FastForwardIfPossible => "--ff",
            Self::Rebase => "--rebase",
        }
    }
}

/// How a push handles a non-fast-forward update. Plain `--force` does not exist in this
/// family: the only forced mode is `--force-with-lease`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PushForce {
    /// Refuse non-fast-forward updates.
    Never,
    /// Force with a lease anchored at the observed remote OID (the default forced mode).
    WithLease,
}

/// One pushed ref update. `expected_remote` is the remote OID observed when the request was
/// accepted; with [`PushForce::WithLease`] it becomes the `--force-with-lease=<ref>:<oid>`
/// anchor. When it is `None` the lease falls back to the local remote-tracking ref.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushRefspec {
    source: Option<RefId>,
    target: RefId,
    expected_remote: Option<ObjectId>,
}

impl PushRefspec {
    /// # Errors
    ///
    /// Returns [`RemoteOperationErrorKind::InvalidRefspec`] when either ref is not UTF-8
    /// (argv must carry the names) or the source ref is not under `refs/`.
    pub fn new(
        source: RefId,
        target: RefId,
        expected_remote: Option<ObjectId>,
    ) -> Result<Self, RemoteOperationError> {
        let valid = utf8_ref(&source).is_some_and(|name| name.starts_with("refs/"))
            && utf8_ref(&target).is_some_and(|name| name.starts_with("refs/"));
        if valid {
            Ok(Self {
                source: Some(source),
                target,
                expected_remote,
            })
        } else {
            Err(RemoteOperationError::new(
                RemoteOperationErrorKind::InvalidRefspec,
                "push refspecs need UTF-8 refs under refs/",
            ))
        }
    }

    #[must_use]
    pub const fn source(&self) -> Option<&RefId> {
        self.source.as_ref()
    }

    /// Creates a deletion refspec (`:<target>`). The expected remote object is retained for
    /// an optional force-with-lease guard.
    ///
    /// # Errors
    ///
    /// Returns [`RemoteOperationErrorKind::InvalidRefspec`] when the target is not a UTF-8 ref
    /// under `refs/`.
    pub fn delete(
        target: RefId,
        expected_remote: Option<ObjectId>,
    ) -> Result<Self, RemoteOperationError> {
        if utf8_ref(&target).is_some_and(|name| name.starts_with("refs/")) {
            Ok(Self {
                source: None,
                target,
                expected_remote,
            })
        } else {
            Err(RemoteOperationError::new(
                RemoteOperationErrorKind::InvalidRefspec,
                "push deletion targets need UTF-8 refs under refs/",
            ))
        }
    }

    #[must_use]
    pub const fn target(&self) -> &RefId {
        &self.target
    }

    #[must_use]
    pub const fn expected_remote(&self) -> Option<&ObjectId> {
        self.expected_remote.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteAddRequest {
    pub name: RemoteName,
    pub url: RemoteUrl,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteEditRequest {
    pub name: RemoteName,
    pub url: RemoteUrl,
}

/// Renames a remote, moving its remote-tracking refs and the upstreams that use it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteRenameRequest {
    pub name: RemoteName,
    pub new_name: RemoteName,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteRemoveRequest {
    pub name: RemoteName,
}

/// Which remotes one fetch covers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FetchScope {
    Remote(RemoteName),
    All,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchRequest {
    pub scope: FetchScope,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequest {
    pub remote: RemoteName,
    /// Local branch ref updated by the pull and captured for recovery.
    pub local_branch: RefId,
    /// Optional remote branch refspec (for example `main`); `None` uses the configured upstream.
    pub branch: Option<RefId>,
    pub strategy: PullStrategy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushRequest {
    pub remote: RemoteName,
    pub refspecs: Vec<PushRefspec>,
    pub force: PushForce,
    pub set_upstream: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetUpstreamRequest {
    /// Local branch receiving the upstream, for example `refs/heads/main`.
    pub branch: RefId,
    /// Upstream as `branch --set-upstream-to` accepts it, for example `origin/main`.
    pub upstream: RefId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PruneRequest {
    pub remote: RemoteName,
}

/// The validated operations of the remotes and transfer family.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteOperationRequest {
    Add(RemoteAddRequest),
    Edit(RemoteEditRequest),
    Rename(RemoteRenameRequest),
    Remove(RemoteRemoveRequest),
    Fetch(FetchRequest),
    Pull(PullRequest),
    Push(PushRequest),
    SetUpstream(SetUpstreamRequest),
    Prune(PruneRequest),
}

/// One process argument plus the exact display the activity log publishes.
#[derive(Clone, Eq, PartialEq)]
pub struct RemoteArgument {
    value: OsString,
    display: String,
    redacted: bool,
}

impl fmt::Debug for RemoteArgument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteArgument")
            .field("display", &self.display)
            .field("redacted", &self.redacted)
            .finish_non_exhaustive()
    }
}

impl RemoteArgument {
    /// Publishes the argument verbatim. The value must be valid UTF-8.
    #[must_use]
    pub fn visible(value: impl Into<OsString>) -> Self {
        let value = value.into();
        let display = value.to_string_lossy().into_owned();
        Self {
            value,
            display,
            redacted: false,
        }
    }

    /// Passes `value` to the process but publishes only `replacement`.
    #[must_use]
    pub fn redacted(value: impl Into<OsString>, replacement: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            display: replacement.into(),
            redacted: true,
        }
    }

    #[must_use]
    pub fn value(&self) -> &OsStr {
        &self.value
    }

    #[must_use]
    pub fn display(&self) -> &str {
        &self.display
    }

    #[must_use]
    pub const fn is_redacted(&self) -> bool {
        self.redacted
    }
}

/// One environment override. `GIT_ASKPASS`/`SSH_ASKPASS` values are paths, never credentials,
/// but the process runner always publishes askpass-named variables as `[REDACTED]`; this type
/// mirrors that rule so the preflight preview and the executed activity record agree byte for
/// byte.
#[derive(Clone, Eq, PartialEq)]
pub struct RemoteEnvironment {
    name: OsString,
    value: OsString,
    redacted: bool,
}

impl fmt::Debug for RemoteEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let display = if self.redacted {
            "[REDACTED]".into()
        } else {
            self.value.to_string_lossy()
        };
        formatter
            .debug_struct("RemoteEnvironment")
            .field("name", &self.name)
            .field("display", &display)
            .field("redacted", &self.redacted)
            .finish()
    }
}

impl RemoteEnvironment {
    /// # Errors
    ///
    /// Returns [`RemoteOperationErrorKind::InvalidEnvironment`] for an empty name, a name
    /// containing `=`, or non-UTF-8 values (activity display must be lossless).
    pub fn new(
        name: impl Into<OsString>,
        value: impl Into<OsString>,
    ) -> Result<Self, RemoteOperationError> {
        let name = name.into();
        let value = value.into();
        let Some(name_text) = name.to_str() else {
            return Err(RemoteOperationError::new(
                RemoteOperationErrorKind::InvalidEnvironment,
                "environment name is not UTF-8",
            ));
        };
        if name_text.is_empty()
            || name_text.contains(['=', '\0'])
            || value.to_str().is_none_or(|text| text.contains('\0'))
        {
            return Err(RemoteOperationError::new(
                RemoteOperationErrorKind::InvalidEnvironment,
                "environment name is empty or contains '=', or the value is not UTF-8",
            ));
        }
        Ok(Self {
            redacted: name_text.to_ascii_uppercase().contains("ASKPASS"),
            name,
            value,
        })
    }

    #[must_use]
    pub fn name(&self) -> &OsStr {
        &self.name
    }

    #[must_use]
    pub fn value(&self) -> &OsStr {
        &self.value
    }

    #[must_use]
    pub const fn is_redacted(&self) -> bool {
        self.redacted
    }
}

/// The exact command a remote operation executes. The preflight preview is derived from this
/// value and the infrastructure executor consumes it unchanged, so preview and executed argv
/// are identical by construction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteCommand {
    arguments: Vec<RemoteArgument>,
    environment: Vec<RemoteEnvironment>,
    working_directory: PathBuf,
    parse_progress: bool,
}

impl RemoteCommand {
    /// # Errors
    ///
    /// Returns [`RemoteOperationErrorKind::TooManyArguments`] or
    /// [`RemoteOperationErrorKind::TooManyEnvironment`] beyond the command bounds.
    pub fn new(
        arguments: Vec<RemoteArgument>,
        environment: Vec<RemoteEnvironment>,
        working_directory: PathBuf,
        parse_progress: bool,
    ) -> Result<Self, RemoteOperationError> {
        if arguments.is_empty() || arguments.len() > MAX_REMOTE_ARGUMENTS {
            return Err(RemoteOperationError::new(
                RemoteOperationErrorKind::TooManyArguments,
                "remote command argument vector is empty or exceeds its bound",
            ));
        }
        if environment.len() > MAX_REMOTE_ENVIRONMENT {
            return Err(RemoteOperationError::new(
                RemoteOperationErrorKind::TooManyEnvironment,
                "remote command environment exceeds its bound",
            ));
        }
        Ok(Self {
            arguments,
            environment,
            working_directory,
            parse_progress,
        })
    }

    #[must_use]
    pub fn arguments(&self) -> &[RemoteArgument] {
        &self.arguments
    }

    #[must_use]
    pub fn environment(&self) -> &[RemoteEnvironment] {
        &self.environment
    }

    #[must_use]
    pub fn working_directory(&self) -> &Path {
        &self.working_directory
    }

    #[must_use]
    pub const fn parse_progress(&self) -> bool {
        self.parse_progress
    }

    /// The preview command, identical to the activity record the infrastructure runner
    /// publishes for the same executable: visible arguments are valid UTF-8 by construction,
    /// redacted arguments publish their replacement, and askpass-named environment values
    /// publish `[REDACTED]`, exactly matching the process runner's redaction discipline.
    #[must_use]
    pub fn activity_command(&self, executable: &OsStr) -> ActivityCommand {
        ActivityCommand {
            executable: executable.to_string_lossy().into_owned(),
            arguments: self
                .arguments
                .iter()
                .map(|argument| ActivityArgument {
                    display: argument.display.clone(),
                    redacted: argument.redacted,
                    native_lossless: argument.value.to_str().is_some(),
                })
                .collect(),
            environment: self
                .environment
                .iter()
                .map(|variable| ActivityEnvironment {
                    name: variable.name.to_string_lossy().into_owned(),
                    display: if variable.redacted {
                        "[REDACTED]".to_owned()
                    } else {
                        variable.value.to_string_lossy().into_owned()
                    },
                    redacted: variable.redacted,
                })
                .collect(),
            working_directory: Some(self.working_directory.to_string_lossy().into_owned()),
            stdin_bytes: 0,
        }
    }
}

/// Command arguments and whether execution needs a transfer environment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandShape {
    pub arguments: Vec<RemoteArgument>,
    /// Whether the command can contact a remote and needs the noninteractive transfer
    /// environment plus the `credential.interactive` override.
    pub transfer: bool,
    pub parse_progress: bool,
}

/// Builds exact argv without choosing consumer authentication or confirmation policy.
///
/// # Errors
/// Rejects invalid refspecs or a push exceeding the configured batch bound.
#[allow(clippy::too_many_lines)]
pub fn command_shape(
    request: &RemoteOperationRequest,
) -> Result<CommandShape, RemoteOperationError> {
    let shape = match request {
        RemoteOperationRequest::Add(request) => CommandShape {
            arguments: vec![
                RemoteArgument::visible("remote"),
                RemoteArgument::visible("add"),
                RemoteArgument::visible(request.name.as_str()),
                url_argument(&request.url),
            ],
            transfer: false,
            parse_progress: false,
        },
        RemoteOperationRequest::Edit(request) => CommandShape {
            arguments: vec![
                RemoteArgument::visible("remote"),
                RemoteArgument::visible("set-url"),
                RemoteArgument::visible(request.name.as_str()),
                url_argument(&request.url),
            ],
            transfer: false,
            parse_progress: false,
        },
        RemoteOperationRequest::Rename(request) => CommandShape {
            arguments: vec![
                RemoteArgument::visible("remote"),
                RemoteArgument::visible("rename"),
                RemoteArgument::visible(request.name.as_str()),
                RemoteArgument::visible(request.new_name.as_str()),
            ],
            transfer: false,
            parse_progress: false,
        },
        RemoteOperationRequest::Remove(request) => CommandShape {
            arguments: vec![
                RemoteArgument::visible("remote"),
                RemoteArgument::visible("remove"),
                RemoteArgument::visible(request.name.as_str()),
            ],
            transfer: false,
            parse_progress: false,
        },
        RemoteOperationRequest::Fetch(request) => {
            let mut arguments = vec![
                RemoteArgument::visible("fetch"),
                RemoteArgument::visible("--progress"),
            ];
            match &request.scope {
                FetchScope::Remote(remote) => {
                    arguments.push(RemoteArgument::visible(remote.as_str()));
                }
                FetchScope::All => {
                    arguments.push(RemoteArgument::visible("--all"));
                }
            }
            CommandShape {
                arguments,
                transfer: true,
                parse_progress: true,
            }
        }
        RemoteOperationRequest::Pull(request) => {
            let mut arguments = vec![
                RemoteArgument::visible("pull"),
                RemoteArgument::visible("--progress"),
                RemoteArgument::visible(request.strategy.flag()),
                RemoteArgument::visible(request.remote.as_str()),
            ];
            if let Some(branch) = &request.branch {
                arguments.push(RemoteArgument::visible(pull_branch_argument(branch)?));
            }
            CommandShape {
                arguments,
                transfer: true,
                parse_progress: true,
            }
        }
        RemoteOperationRequest::Push(request) => {
            if request.refspecs.is_empty() || request.refspecs.len() > MAX_PUSH_REFSPECS {
                return Err(RemoteOperationError::new(
                    RemoteOperationErrorKind::InvalidRefspec,
                    "push needs between one and its bound of refspecs",
                ));
            }
            let mut arguments = vec![
                RemoteArgument::visible("push"),
                RemoteArgument::visible("--progress"),
            ];
            if request.refspecs.len() > 1 {
                arguments.push(RemoteArgument::visible("--atomic"));
            }
            if request.set_upstream {
                arguments.push(RemoteArgument::visible("--set-upstream"));
            }
            if request.force == PushForce::WithLease {
                for refspec in &request.refspecs {
                    let target = ref_argument(refspec.target(), "push target")?;
                    let lease = match refspec.expected_remote() {
                        Some(expected) => format!("--force-with-lease={target}:{expected}"),
                        None => format!("--force-with-lease={target}"),
                    };
                    arguments.push(RemoteArgument::visible(lease));
                }
            }
            arguments.push(RemoteArgument::visible(request.remote.as_str()));
            for refspec in &request.refspecs {
                let target = ref_argument(refspec.target(), "push target")?;
                let source = refspec
                    .source()
                    .map(|source| ref_argument(source, "push source"))
                    .transpose()?
                    .unwrap_or_default();
                arguments.push(RemoteArgument::visible(format!("{source}:{target}")));
            }
            CommandShape {
                arguments,
                transfer: true,
                parse_progress: true,
            }
        }
        RemoteOperationRequest::SetUpstream(request) => CommandShape {
            arguments: vec![
                RemoteArgument::visible("branch"),
                RemoteArgument::visible(format!(
                    "--set-upstream-to={}",
                    upstream_argument(&request.upstream)?
                )),
                RemoteArgument::visible(local_branch_argument(&request.branch)?),
            ],
            transfer: false,
            parse_progress: false,
        },
        RemoteOperationRequest::Prune(request) => CommandShape {
            arguments: vec![
                RemoteArgument::visible("remote"),
                RemoteArgument::visible("prune"),
                RemoteArgument::visible(request.remote.as_str()),
            ],
            transfer: true,
            parse_progress: false,
        },
    };
    Ok(shape)
}

/// Optional behaviour of one fetch beyond its scope.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FetchOptions {
    /// Removes remote-tracking refs whose branch no longer exists on the remote.
    pub prune: bool,
    /// Requests reachable objects again even when refs have not moved.
    pub refetch: bool,
}

/// Builds a fetch of `request` with `options`.
///
/// # Errors
/// Returns the same validation errors as [`command_shape`].
pub fn fetch_command_shape(
    request: &FetchRequest,
    options: FetchOptions,
) -> Result<CommandShape, RemoteOperationError> {
    let mut shape = command_shape(&RemoteOperationRequest::Fetch(request.clone()))?;
    let mut flags = Vec::new();
    if options.prune {
        flags.push(RemoteArgument::visible("--prune"));
    }
    if options.refetch {
        flags.push(RemoteArgument::visible("--refetch"));
    }
    shape.arguments.splice(2..2, flags);
    Ok(shape)
}

/// Builds a fetch that requests reachable objects again even when refs have not moved.
/// This can restore locally missing objects without touching the index or worktree.
///
/// # Errors
/// Returns the same validation errors as [`command_shape`].
pub fn refetch_command_shape(request: &FetchRequest) -> Result<CommandShape, RemoteOperationError> {
    fetch_command_shape(
        request,
        FetchOptions {
            prune: false,
            refetch: true,
        },
    )
}

/// Builds a read-only query of one exact ref on `remote`. Parse its stdout with
/// [`parse_remote_ref_probe`]; `ls-remote` patterns match ref suffixes, so the output may
/// also list other refs ending in the same name.
///
/// # Errors
/// Returns [`RemoteOperationErrorKind::InvalidRefspec`] for a ref outside `refs/` or one that
/// is not UTF-8.
pub fn remote_ref_probe_command_shape(
    remote: &RemoteName,
    reference: &RefId,
) -> Result<CommandShape, RemoteOperationError> {
    let reference = ref_argument(reference, "probed")?;
    if !reference.starts_with("refs/") {
        return Err(RemoteOperationError::new(
            RemoteOperationErrorKind::InvalidRefspec,
            "probed ref must be a full ref under refs/",
        ));
    }
    Ok(CommandShape {
        arguments: vec![
            RemoteArgument::visible("ls-remote"),
            RemoteArgument::visible("--refs"),
            RemoteArgument::visible(remote.as_str()),
            RemoteArgument::visible(reference),
        ],
        transfer: true,
        parse_progress: false,
    })
}

/// Returns the object `reference` points to on the remote, or `None` when the remote does not
/// have it, from the stdout of [`remote_ref_probe_command_shape`].
///
/// # Errors
/// Returns [`RemoteOperationErrorKind::Preflight`] for output that is not `ls-remote` lines.
pub fn parse_remote_ref_probe(
    stdout: &[u8],
    reference: &RefId,
) -> Result<Option<ObjectId>, RemoteOperationError> {
    let malformed = || {
        RemoteOperationError::new(
            RemoteOperationErrorKind::Preflight,
            "remote ref probe output is not ls-remote lines",
        )
    };
    let mut found = None;
    for line in stdout.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let tab = line
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(malformed)?;
        let (object, name) = (&line[..tab], &line[tab + 1..]);
        let object = std::str::from_utf8(object)
            .ok()
            .and_then(|object| ObjectId::parse(object).ok())
            .ok_or_else(malformed)?;
        if name == reference.as_bytes() {
            if found.is_some() {
                return Err(malformed());
            }
            found = Some(object);
        }
    }
    Ok(found)
}

/// Deletes the remote-tracking ref `tracking` only while it still points at `expected`, for
/// example after the remote confirmed that its branch is gone.
///
/// # Errors
/// Returns [`RemoteOperationErrorKind::InvalidRefspec`] for a ref outside `refs/remotes/` or
/// one that is not UTF-8.
pub fn prune_tracking_ref_command_shape(
    tracking: &RefId,
    expected: &ObjectId,
) -> Result<CommandShape, RemoteOperationError> {
    let tracking = ref_argument(tracking, "remote-tracking")?;
    if !tracking.starts_with("refs/remotes/") {
        return Err(RemoteOperationError::new(
            RemoteOperationErrorKind::InvalidRefspec,
            "pruned ref must be a remote-tracking ref",
        ));
    }
    Ok(CommandShape {
        arguments: vec![
            RemoteArgument::visible("update-ref"),
            RemoteArgument::visible("--no-deref"),
            RemoteArgument::visible("-d"),
            RemoteArgument::visible(tracking),
            RemoteArgument::visible(expected.as_str()),
        ],
        transfer: false,
        parse_progress: false,
    })
}

fn url_argument(url: &RemoteUrl) -> RemoteArgument {
    if url.is_sensitive() {
        RemoteArgument::redacted(url.raw(), url.display())
    } else {
        RemoteArgument::visible(url.raw())
    }
}

fn utf8_ref(reference: &RefId) -> Option<&str> {
    std::str::from_utf8(reference.as_bytes()).ok()
}

fn ref_argument<'a>(reference: &'a RefId, role: &str) -> Result<&'a str, RemoteOperationError> {
    utf8_ref(reference).ok_or_else(|| {
        RemoteOperationError::new(
            RemoteOperationErrorKind::InvalidRefspec,
            format!("{role} ref is not UTF-8 and cannot become argv"),
        )
    })
}

fn pull_branch_argument(reference: &RefId) -> Result<&str, RemoteOperationError> {
    let reference = ref_argument(reference, "pull branch")?;
    Ok(reference.strip_prefix("refs/heads/").unwrap_or(reference))
}

fn local_branch_argument(reference: &RefId) -> Result<&str, RemoteOperationError> {
    let reference = ref_argument(reference, "branch")?;
    Ok(reference.strip_prefix("refs/heads/").unwrap_or(reference))
}

fn upstream_argument(reference: &RefId) -> Result<&str, RemoteOperationError> {
    let reference = ref_argument(reference, "upstream")?;
    Ok(reference.strip_prefix("refs/remotes/").unwrap_or(reference))
}

/// The credential-free display form of a configured URL, mirroring
/// the shared reference parser so activity, preflight, and sidebar
/// surfaces agree.
fn credential_free_url(raw: &str) -> String {
    let mut display = raw.to_owned();
    if raw.contains("://") {
        if let Some(scheme) = display.find("://") {
            let authority_start = scheme + 3;
            let authority_end = display[authority_start..]
                .find(['/', '?', '#'])
                .map_or(display.len(), |index| authority_start + index);
            if let Some(at) = display[authority_start..authority_end].rfind('@') {
                display.replace_range(authority_start..authority_start + at, "[REDACTED]");
            }
        }
        redact_url_query_values(&mut display);
    } else if raw.contains(':') && !raw.contains(' ') && !raw.starts_with(['/', '.']) {
        if let Some(at) = display.find('@') {
            display.replace_range(..at, "[REDACTED]");
        }
    }
    display
}

fn redact_url_query_values(value: &mut String) {
    for marker in ["access_token=", "token=", "password=", "credential="] {
        let mut search_start = 0;
        loop {
            let lowercase = value.to_ascii_lowercase();
            let Some(relative) = lowercase[search_start..].find(marker) else {
                break;
            };
            let start = search_start + relative + marker.len();
            let end = value[start..]
                .find(['&', '#'])
                .map_or(value.len(), |index| start + index);
            value.replace_range(start..end, "[REDACTED]");
            search_start = start + "[REDACTED]".len();
        }
    }
}

/// Typed family error. All details are bounded and free of credential material.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteOperationError {
    kind: RemoteOperationErrorKind,
    detail: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteOperationErrorKind {
    InvalidName,
    InvalidUrl,
    InvalidRefspec,
    InvalidEnvironment,
    InvalidDetail,
    TooManyArguments,
    TooManyEnvironment,
    /// The framework preflight rejected the request or facts; the detail carries its reason.
    Preflight,
    /// Recovery anchors could not be captured consistently.
    Anchor,
    /// A ledger record exceeded its bounds.
    Ledger,
}

impl RemoteOperationError {
    #[must_use]
    pub fn new(kind: RemoteOperationErrorKind, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        if detail.len() > MAX_TRANSFER_DETAIL_BYTES {
            let mut boundary = MAX_TRANSFER_DETAIL_BYTES;
            while !detail.is_char_boundary(boundary) {
                boundary -= 1;
            }
            detail.truncate(boundary);
        }
        Self { kind, detail }
    }

    #[must_use]
    pub const fn kind(&self) -> RemoteOperationErrorKind {
        self.kind
    }

    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for RemoteOperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for RemoteOperationError {}

/// Pulls while explicitly disabling repository-configured automatic stashing.
/// Use when the consumer owns dirty-tree handling and index restoration.
///
/// # Errors
/// Returns the same validation errors as [`command_shape`].
pub fn pull_without_autostash_command_shape(
    request: &PullRequest,
) -> Result<CommandShape, RemoteOperationError> {
    let mut shape = command_shape(&RemoteOperationRequest::Pull(request.clone()))?;
    shape
        .arguments
        .insert(2, RemoteArgument::visible("--no-autostash"));
    Ok(shape)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_pull_policy_overrides_configured_auto_stash_for_every_strategy() {
        for strategy in [
            PullStrategy::FastForwardOnly,
            PullStrategy::FastForwardIfPossible,
            PullStrategy::Rebase,
        ] {
            let request = PullRequest {
                remote: RemoteName::new("origin").unwrap(),
                local_branch: RefId::parse("refs/heads/main").unwrap(),
                branch: Some(RefId::parse("refs/heads/main").unwrap()),
                strategy,
            };
            let shape = pull_without_autostash_command_shape(&request).unwrap();
            assert_eq!(
                displays(&shape),
                [
                    "pull",
                    "--progress",
                    "--no-autostash",
                    strategy.flag(),
                    "origin",
                    "main"
                ]
            );
            assert!(
                !displays(&command_shape(&RemoteOperationRequest::Pull(request)).unwrap())
                    .contains(&"--no-autostash")
            );
        }
    }

    #[test]
    fn refetch_reuses_fetch_scope_and_requests_objects_again() {
        let shape = refetch_command_shape(&FetchRequest {
            scope: FetchScope::Remote(RemoteName::new("origin").unwrap()),
        })
        .unwrap();
        let arguments = shape
            .arguments
            .iter()
            .map(RemoteArgument::display)
            .collect::<Vec<_>>();
        assert_eq!(arguments, ["fetch", "--progress", "--refetch", "origin"]);
        assert!(shape.transfer);
        assert!(shape.parse_progress);
    }

    fn displays(shape: &CommandShape) -> Vec<&str> {
        shape
            .arguments
            .iter()
            .map(RemoteArgument::display)
            .collect()
    }

    #[test]
    fn fetch_options_add_prune_and_refetch_before_the_scope() {
        let origin = FetchRequest {
            scope: FetchScope::Remote(RemoteName::new("origin").unwrap()),
        };
        let all = FetchRequest {
            scope: FetchScope::All,
        };
        let prune = FetchOptions {
            prune: true,
            refetch: false,
        };
        assert_eq!(
            displays(&fetch_command_shape(&origin, prune).unwrap()),
            ["fetch", "--progress", "--prune", "origin"]
        );
        assert_eq!(
            displays(&fetch_command_shape(&all, prune).unwrap()),
            ["fetch", "--progress", "--prune", "--all"]
        );
        assert_eq!(
            displays(
                &fetch_command_shape(
                    &origin,
                    FetchOptions {
                        prune: true,
                        refetch: true,
                    },
                )
                .unwrap()
            ),
            ["fetch", "--progress", "--prune", "--refetch", "origin"]
        );
        assert_eq!(
            displays(&fetch_command_shape(&origin, FetchOptions::default()).unwrap()),
            ["fetch", "--progress", "origin"]
        );
    }

    #[test]
    fn remote_ref_probe_matches_only_the_exact_ref() {
        let reference = RefId::parse("refs/heads/topic").unwrap();
        let shape = remote_ref_probe_command_shape(&RemoteName::new("origin").unwrap(), &reference)
            .unwrap();
        assert_eq!(
            displays(&shape),
            ["ls-remote", "--refs", "origin", "refs/heads/topic"]
        );
        assert!(shape.transfer && !shape.parse_progress);

        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let output = format!("{a}\trefs/heads/feature/topic\r\n{b}\trefs/heads/topic\n");
        assert_eq!(
            parse_remote_ref_probe(output.as_bytes(), &reference).unwrap(),
            Some(ObjectId::parse(b.clone()).unwrap())
        );
        let suffix_only = format!("{a}\trefs/heads/feature/topic\n");
        assert_eq!(
            parse_remote_ref_probe(suffix_only.as_bytes(), &reference).unwrap(),
            None
        );
        assert_eq!(parse_remote_ref_probe(b"", &reference).unwrap(), None);
        assert!(parse_remote_ref_probe(b"not ls-remote output\n", &reference).is_err());
        let duplicate = format!("{a}\trefs/heads/topic\n{b}\trefs/heads/topic\n");
        assert!(parse_remote_ref_probe(duplicate.as_bytes(), &reference).is_err());
    }

    #[test]
    fn tracking_ref_prune_is_conditional_and_limited_to_remote_refs() {
        let oid = ObjectId::parse("c".repeat(40)).unwrap();
        let shape = prune_tracking_ref_command_shape(
            &RefId::parse("refs/remotes/origin/topic").unwrap(),
            &oid,
        )
        .unwrap();
        assert_eq!(
            displays(&shape),
            [
                "update-ref",
                "--no-deref",
                "-d",
                "refs/remotes/origin/topic",
                oid.as_str()
            ]
        );
        assert!(!shape.transfer);
        assert!(
            prune_tracking_ref_command_shape(&RefId::parse("refs/heads/topic").unwrap(), &oid)
                .is_err()
        );
    }

    #[test]
    fn push_keeps_atomic_updates_and_explicit_lease_anchors() {
        let target = RefId::parse("refs/heads/main").unwrap();
        let oid = ObjectId::parse("a".repeat(40)).unwrap();
        let request = RemoteOperationRequest::Push(PushRequest {
            remote: RemoteName::new("origin").unwrap(),
            refspecs: vec![
                PushRefspec::new(target.clone(), target, Some(oid.clone())).unwrap(),
                PushRefspec::delete(RefId::parse("refs/heads/old").unwrap(), None).unwrap(),
            ],
            force: PushForce::WithLease,
            set_upstream: true,
        });
        let shape = command_shape(&request).unwrap();
        let args = shape
            .arguments
            .iter()
            .map(RemoteArgument::display)
            .collect::<Vec<_>>();
        assert!(args.contains(&"--atomic"));
        assert!(args.contains(&"--set-upstream"));
        assert!(args.contains(&format!("--force-with-lease=refs/heads/main:{oid}").as_str()));
        assert!(args.contains(&":refs/heads/old"));
        assert!(!args.contains(&"--force"));
        assert!(shape.transfer && shape.parse_progress);
    }

    #[test]
    fn rename_names_both_remotes_without_contacting_them() {
        let shape = command_shape(&RemoteOperationRequest::Rename(RemoteRenameRequest {
            name: RemoteName::new("origin").unwrap(),
            new_name: RemoteName::new("upstream").unwrap(),
        }))
        .unwrap();
        let args = shape
            .arguments
            .iter()
            .map(RemoteArgument::display)
            .collect::<Vec<_>>();
        assert_eq!(args, ["remote", "rename", "origin", "upstream"]);
        assert!(!shape.transfer && !shape.parse_progress);
    }

    #[test]
    fn secrets_are_absent_from_previews_and_debug_representations() {
        let url =
            RemoteUrl::new("https://user:private-token@example.test/repo?token=private-token")
                .unwrap();
        let shape = command_shape(&RemoteOperationRequest::Add(RemoteAddRequest {
            name: RemoteName::new("origin").unwrap(),
            url: url.clone(),
        }))
        .unwrap();
        let environment = RemoteEnvironment::new("GIT_ASKPASS", "/private-helper").unwrap();
        let command = RemoteCommand::new(
            shape.arguments,
            vec![environment],
            "/checkout".into(),
            false,
        )
        .unwrap();
        for display in [
            format!("{url:?}"),
            format!("{command:?}"),
            format!("{:?}", command.activity_command(OsStr::new("git"))),
        ] {
            assert!(!display.contains("private-token"));
            assert!(!display.contains("private-helper"));
        }
        assert!(command
            .arguments()
            .last()
            .unwrap()
            .value()
            .to_string_lossy()
            .contains("private-token"));
    }

    #[test]
    fn environment_and_names_reject_process_delimiters() {
        assert!(RemoteEnvironment::new("NAME\0", "value").is_err());
        assert!(RemoteEnvironment::new("NAME", "value\0").is_err());
        for name in ["--all", "../other", "name.lock", "name with spaces"] {
            assert!(RemoteName::new(name).is_err());
        }
    }
}
