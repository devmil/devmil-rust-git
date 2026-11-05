//! Stable repository reference, stash, remote, and worktree values.

use crate::types::{ObjectId, RefId, RepositoryPath, WorktreeId};
#[cfg(unix)]
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ReferenceKind {
    LocalBranch,
    RemoteBranch,
    Tag,
    Stash,
    Other,
}

/// `for-each-ref` patterns selecting exactly the references [`RepositoryReference::classify`]
/// does not classify as [`ReferenceKind::Other`]: branches, remote-tracking branches, tags and
/// the stash. Tools that write their own references (notes, pull request mirrors, bisect,
/// application checkpoints) keep them outside these namespaces.
pub const STANDARD_REFERENCE_PATTERNS: [&str; 4] =
    ["refs/heads", "refs/remotes", "refs/tags", "refs/stash"];

impl ReferenceKind {
    /// Whether references of this kind lie in [`STANDARD_REFERENCE_PATTERNS`].
    #[must_use]
    pub const fn is_standard(self) -> bool {
        !matches!(self, Self::Other)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpstreamStatus {
    pub reference: RefId,
    pub ahead: u64,
    pub behind: u64,
    pub gone: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryReference {
    pub id: RefId,
    pub kind: ReferenceKind,
    pub target: ObjectId,
    pub peeled_target: Option<ObjectId>,
    pub symbolic_target: Option<RefId>,
    pub upstream: Option<UpstreamStatus>,
    pub is_head: bool,
}

impl RepositoryReference {
    #[must_use]
    pub fn classify(id: &RefId) -> ReferenceKind {
        let bytes = id.as_bytes();
        if bytes.starts_with(b"refs/heads/") {
            ReferenceKind::LocalBranch
        } else if bytes.starts_with(b"refs/remotes/") {
            ReferenceKind::RemoteBranch
        } else if bytes.starts_with(b"refs/tags/") {
            ReferenceKind::Tag
        } else if bytes == b"refs/stash" {
            ReferenceKind::Stash
        } else {
            ReferenceKind::Other
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RemoteId(Vec<u8>);

impl RemoteId {
    pub fn from_bytes(value: impl Into<Vec<u8>>) -> Option<Self> {
        let value = value.into();
        (!value.is_empty() && !value.contains(&0) && !value.contains(&b'\n')).then_some(Self(value))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteUrlKind {
    LocalPath,
    ScpLike,
    Url,
    Invalid,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Remote {
    pub id: RemoteId,
    /// Credential-free bytes suitable for display. The original configured URL never enters the
    /// domain or presentation layers.
    pub display_url: Vec<u8>,
    pub url_kind: RemoteUrlKind,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StashId(pub ObjectId);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StashEntry {
    pub id: StashId,
    pub selector: Vec<u8>,
    pub subject: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkedWorktree {
    pub id: WorktreeId,
    pub head: Option<ObjectId>,
    pub branch: Option<RefId>,
    pub detached: bool,
    pub locked_reason: Option<Vec<u8>>,
    pub prunable_reason: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RefsSnapshot {
    pub head_symbolic: Option<RefId>,
    pub references: Vec<RepositoryReference>,
    pub remotes: Vec<Remote>,
    pub stashes: Vec<StashEntry>,
    pub worktrees: Vec<LinkedWorktree>,
}

/// Worktree porcelain fields before resolving a Git administration directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeListEntry {
    pub path: PathBuf,
    pub head: Option<ObjectId>,
    pub branch: Option<RefId>,
    pub locked: bool,
    pub prunable: bool,
}

/// Invalid worktree inventory fields or a consumer-selected count bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorktreeListError {
    TooManyWorktrees,
    InvalidPath,
    MissingPath,
    InvalidObjectId,
    InvalidRef,
    InvalidRecord,
}

impl std::fmt::Display for WorktreeListError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::TooManyWorktrees => "worktree count exceeds its bound",
            Self::InvalidPath => "Git returned an invalid worktree path",
            Self::MissingPath => "Git worktree output has no path",
            Self::InvalidObjectId => "Git returned an invalid object ID",
            Self::InvalidRef => "Git returned an invalid ref",
            Self::InvalidRecord => "Git worktree output is malformed",
        })
    }
}

impl std::error::Error for WorktreeListError {}

/// Parses worktree porcelain inventory without filesystem access.
///
/// Unlike [`parse_worktrees`], this strict inventory parser rejects unknown fields
/// and requires UTF-8 branch refs. Native paths retain their bytes on Unix. Empty
/// records are ignored, and the final record may omit its empty separator.
///
/// # Errors
/// Rejects unknown records, invalid identities, missing paths, and count overflow.
pub fn parse_worktree_list(
    raw: &[u8],
    maximum_worktrees: usize,
) -> Result<Vec<WorktreeListEntry>, WorktreeListError> {
    let mut result = Vec::new();
    let fields = raw.split(|byte| *byte == 0).collect::<Vec<_>>();
    for record in fields
        .split(|field| field.is_empty())
        .filter(|record| !record.is_empty())
    {
        if result.len() == maximum_worktrees {
            return Err(WorktreeListError::TooManyWorktrees);
        }
        let mut path = None;
        let mut head = None;
        let mut branch = None;
        let mut locked = false;
        let mut prunable = false;
        for field in record {
            if let Some(value) = field.strip_prefix(b"worktree ") {
                path = Some(native_path(value).map_err(|_| WorktreeListError::InvalidPath)?);
            } else if let Some(value) = field.strip_prefix(b"HEAD ") {
                head = Some(
                    std::str::from_utf8(value)
                        .ok()
                        .and_then(|value| ObjectId::parse(value).ok())
                        .ok_or(WorktreeListError::InvalidObjectId)?,
                );
            } else if let Some(value) = field.strip_prefix(b"branch ") {
                branch = Some(
                    std::str::from_utf8(value)
                        .ok()
                        .and_then(|value| RefId::parse(value).ok())
                        .ok_or(WorktreeListError::InvalidRef)?,
                );
            } else if field.starts_with(b"locked") {
                locked = true;
            } else if field.starts_with(b"prunable") {
                prunable = true;
            } else if !matches!(*field, b"bare" | b"detached") {
                return Err(WorktreeListError::InvalidRecord);
            }
        }
        result.push(WorktreeListEntry {
            path: path.ok_or(WorktreeListError::MissingPath)?,
            head,
            branch,
            locked,
            prunable,
        });
    }
    Ok(result)
}

#[cfg(test)]
mod model_tests {
    use super::*;

    #[test]
    fn classifies_canonical_reference_names_without_display_text() {
        for (name, kind) in [
            ("refs/heads/main", ReferenceKind::LocalBranch),
            ("refs/remotes/origin/main", ReferenceKind::RemoteBranch),
            ("refs/tags/v1", ReferenceKind::Tag),
            ("refs/stash", ReferenceKind::Stash),
            ("refs/notes/review", ReferenceKind::Other),
        ] {
            let id = RefId::parse(name).unwrap();
            assert_eq!(RepositoryReference::classify(&id), kind);
        }
    }

    #[test]
    fn standard_patterns_cover_exactly_the_standard_kinds() {
        for (name, standard) in [
            ("refs/heads/main", true),
            ("refs/remotes/origin/main", true),
            ("refs/tags/v1", true),
            ("refs/stash", true),
            ("refs/notes/commits", false),
            ("refs/t3/checkpoints/1", false),
            ("refs/pull/1/head", false),
            ("refs/bisect/bad", false),
            ("refs/stashes/x", false),
            ("refs/headsx/main", false),
        ] {
            let id = RefId::parse(name).unwrap();
            // `for-each-ref` matches a pattern exactly or as a whole leading path.
            let matched = STANDARD_REFERENCE_PATTERNS
                .iter()
                .any(|pattern| name == *pattern || name.starts_with(&format!("{pattern}/")));
            assert_eq!(matched, standard, "{name}");
            assert_eq!(
                RepositoryReference::classify(&id).is_standard(),
                standard,
                "{name}"
            );
        }
    }

    #[test]
    fn remote_identity_rejects_record_delimiters() {
        assert!(RemoteId::from_bytes(b"origin".to_vec()).is_some());
        assert!(RemoteId::from_bytes(b"bad\0name".to_vec()).is_none());
        assert!(RemoteId::from_bytes(b"bad\nname".to_vec()).is_none());
    }
}

/// NUL-separated format consumed by [`parse_references`].
pub const REF_FORMAT: &str = "%(refname)%00%(objectname)%00%(*objectname)%00%(symref)%00%(upstream)%00%(upstream:track)%00%(HEAD)%00";
/// NUL-separated format consumed by [`parse_stashes`].
pub const STASH_FORMAT: &str = "%H%x00%gd%x00%gs%x00";

/// NUL-separated format consumed by [`parse_branch_upstreams`]: the branch, its
/// remote-tracking ref, and the configured remote name and remote-side ref
/// (`branch.<name>.remote` and `branch.<name>.merge`). Requires Git 2.16 or newer.
pub const UPSTREAM_REMOTE_FORMAT: &str =
    "%(refname)%00%(upstream)%00%(upstream:remotename)%00%(upstream:remoteref)%00";

/// A branch's configured upstream, without splitting a short name at `/`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchUpstream {
    /// The local branch, such as `refs/heads/topic`.
    pub branch: RefId,
    /// The local ref that tracks the upstream (`refs/remotes/origin/topic`, or a local branch
    /// for a `.` remote). Git reports no upstream at all when no fetch refspec maps the remote
    /// ref, so this is present for every row Git produces today; it stays optional so a future
    /// Git that reports the remote alone still parses.
    pub tracking: Option<RefId>,
    /// The configured remote name, which may contain `/`, or `.` for a local upstream.
    pub remote: RemoteId,
    /// The ref on the remote, such as `refs/heads/topic`.
    pub remote_ref: RefId,
}

/// Parses `for-each-ref` output using [`UPSTREAM_REMOTE_FORMAT`]. Branches without a configured
/// upstream are omitted.
///
/// # Errors
/// Returns [`RefsParseError`] for malformed records.
pub fn parse_branch_upstreams(bytes: &[u8]) -> Result<Vec<BranchUpstream>, RefsParseError> {
    let fields = exact_nul_fields(bytes, 4)?;
    let mut rows = Vec::new();
    for record in fields.chunks_exact(4) {
        let name = trim_record_prefix(record[0]);
        if name.is_empty() || (record[2].is_empty() && record[3].is_empty()) {
            continue;
        }
        rows.push(BranchUpstream {
            branch: RefId::from_bytes(name.to_vec()).map_err(|_| RefsParseError)?,
            tracking: parse_optional_ref(record[1])?,
            remote: RemoteId::from_bytes(record[2].to_vec()).ok_or(RefsParseError)?,
            remote_ref: RefId::from_bytes(record[3].to_vec()).map_err(|_| RefsParseError)?,
        });
    }
    Ok(rows)
}

/// Malformed reference, remote, stash, or linked-worktree output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RefsParseError;
impl std::fmt::Display for RefsParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("invalid Git reference output or inaccessible worktree metadata")
    }
}
impl std::error::Error for RefsParseError {}

/// Parses complete `for-each-ref` output using [`REF_FORMAT`].
///
/// # Errors
/// Returns [`RefsParseError`] for malformed records or unavailable required worktree metadata.
pub fn parse_references(bytes: &[u8]) -> Result<Vec<RepositoryReference>, RefsParseError> {
    let fields = exact_nul_fields(bytes, 7)?;
    let mut rows = Vec::new();
    for record in fields.chunks_exact(7) {
        let name = trim_record_prefix(record[0]);
        if name.is_empty() {
            continue;
        }
        let id = RefId::from_bytes(name.to_vec()).map_err(|_| RefsParseError)?;
        let target = ObjectId::parse(str::from_utf8(record[1]).map_err(|_| RefsParseError)?)
            .map_err(|_| RefsParseError)?;
        let peeled_target = parse_optional_object(record[2])?;
        let symbolic_target = parse_optional_ref(record[3])?;
        let upstream_ref = parse_optional_ref(record[4])?;
        let (ahead, behind, gone) = parse_tracking(record[5])?;
        let upstream = upstream_ref.map(|reference| UpstreamStatus {
            reference,
            ahead,
            behind,
            gone,
        });
        rows.push(RepositoryReference {
            kind: RepositoryReference::classify(&id),
            id,
            target,
            peeled_target,
            symbolic_target,
            upstream,
            is_head: record[6] == b"*",
        });
    }
    Ok(rows)
}

use std::str;

fn trim_record_prefix(mut value: &[u8]) -> &[u8] {
    while value
        .first()
        .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
    {
        value = &value[1..];
    }
    value
}

fn trim_line(mut value: &[u8]) -> &[u8] {
    while value
        .last()
        .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
    {
        value = &value[..value.len() - 1];
    }
    value
}

fn parse_optional_object(value: &[u8]) -> Result<Option<ObjectId>, RefsParseError> {
    if value.is_empty() {
        Ok(None)
    } else {
        ObjectId::parse(str::from_utf8(value).map_err(|_| RefsParseError)?)
            .map(Some)
            .map_err(|_| RefsParseError)
    }
}

fn parse_optional_ref(value: &[u8]) -> Result<Option<RefId>, RefsParseError> {
    if value.is_empty() {
        Ok(None)
    } else {
        RefId::from_bytes(value.to_vec())
            .map(Some)
            .map_err(|_| RefsParseError)
    }
}

fn parse_tracking(value: &[u8]) -> Result<(u64, u64, bool), RefsParseError> {
    if value.is_empty() {
        return Ok((0, 0, false));
    }
    let text = str::from_utf8(value).map_err(|_| RefsParseError)?;
    if text == "[gone]" {
        return Ok((0, 0, true));
    }
    let text = text
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .ok_or(RefsParseError)?;
    let mut ahead = 0;
    let mut behind = 0;
    for part in text.split(", ") {
        if let Some(value) = part.strip_prefix("ahead ") {
            ahead = value.parse().map_err(|_| RefsParseError)?;
        } else if let Some(value) = part.strip_prefix("behind ") {
            behind = value.parse().map_err(|_| RefsParseError)?;
        } else {
            return Err(RefsParseError);
        }
    }
    Ok((ahead, behind, false))
}

/// Parses NUL-delimited remote URL configuration into credential-redacted display records.
///
/// # Errors
/// Returns [`RefsParseError`] for malformed records or unavailable required worktree metadata.
pub fn parse_remotes(bytes: &[u8]) -> Result<Vec<Remote>, RefsParseError> {
    if !bytes.is_empty() && !bytes.ends_with(&[0]) {
        return Err(RefsParseError);
    }
    bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let (key, url) =
                SliceSplitOnce::split_once(entry, |byte| *byte == b'\n').ok_or(RefsParseError)?;
            let name = key
                .strip_prefix(b"remote.")
                .and_then(|value| value.strip_suffix(b".url"))
                .ok_or(RefsParseError)?;
            let id = RemoteId::from_bytes(name.to_vec()).ok_or(RefsParseError)?;
            let url_kind = classify_remote_url(url);
            Ok(Remote {
                id,
                display_url: credential_free_remote_url(url, url_kind),
                url_kind,
            })
        })
        .collect()
}

fn credential_free_remote_url(value: &[u8], kind: RemoteUrlKind) -> Vec<u8> {
    if matches!(kind, RemoteUrlKind::Invalid) {
        return b"[invalid URL]".to_vec();
    }
    let Ok(text) = str::from_utf8(value) else {
        return if matches!(kind, RemoteUrlKind::LocalPath) {
            value.to_vec()
        } else {
            b"[invalid URL]".to_vec()
        };
    };
    let mut display = text.to_owned();
    match kind {
        RemoteUrlKind::Url => {
            if let Some(scheme) = display.find("://") {
                let authority_start = scheme + 3;
                let authority_end = display[authority_start..]
                    .find(['/', '?', '#'])
                    .map_or(display.len(), |index| authority_start + index);
                if let Some(at) = display[authority_start..authority_end].rfind('@') {
                    let user = &display[authority_start..authority_start + at];
                    // An SSH login is not a secret; any other user part may be a token.
                    let ssh = matches!(
                        display[..scheme].to_ascii_lowercase().as_str(),
                        "ssh" | "git+ssh" | "ssh+git"
                    );
                    if !ssh || user.contains(':') {
                        display.replace_range(authority_start..authority_start + at, "[REDACTED]");
                    }
                }
            }
            redact_url_query_values(&mut display);
        }
        RemoteUrlKind::ScpLike => {
            // `login@host:path` names an SSH login, which is kept. A colon before the `@`
            // could be a password, so that user part is hidden.
            if let Some(at) = display.find('@').filter(|at| display[..*at].contains(':')) {
                display.replace_range(..at, "[REDACTED]");
            }
        }
        RemoteUrlKind::LocalPath | RemoteUrlKind::Invalid => {}
    }
    display.into_bytes()
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

fn classify_remote_url(value: &[u8]) -> RemoteUrlKind {
    if value.is_empty() || value.iter().any(|byte| *byte < b' ' || *byte == 0x7f) {
        RemoteUrlKind::Invalid
    } else if value.starts_with(b"/") || value.starts_with(b"./") || value.starts_with(b"../") {
        RemoteUrlKind::LocalPath
    } else if value.windows(3).any(|window| window == b"://") {
        RemoteUrlKind::Url
    } else if value.contains(&b':') && !value.contains(&b' ') {
        RemoteUrlKind::ScpLike
    } else {
        RemoteUrlKind::Invalid
    }
}

/// Parses stash list output using [`STASH_FORMAT`].
///
/// # Errors
/// Returns [`RefsParseError`] for malformed records or unavailable required worktree metadata.
pub fn parse_stashes(bytes: &[u8]) -> Result<Vec<StashEntry>, RefsParseError> {
    let fields = exact_nul_fields(bytes, 3)?;
    let mut rows = Vec::new();
    for record in fields.chunks_exact(3) {
        let object = trim_record_prefix(record[0]);
        if object.is_empty() {
            continue;
        }
        rows.push(StashEntry {
            id: StashId(
                ObjectId::parse(str::from_utf8(object).map_err(|_| RefsParseError)?)
                    .map_err(|_| RefsParseError)?,
            ),
            selector: record[1].to_vec(),
            subject: record[2].to_vec(),
        });
    }
    Ok(rows)
}

fn exact_nul_fields(bytes: &[u8], record_width: usize) -> Result<Vec<&[u8]>, RefsParseError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let valid_terminator =
        bytes.ends_with(&[0]) || bytes.ends_with(b"\0\n") || bytes.ends_with(b"\0\r\n");
    if !valid_terminator {
        return Err(RefsParseError);
    }
    let mut fields = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    if fields
        .last()
        .is_some_and(|field| field.is_empty() || *field == b"\n" || *field == b"\r\n")
    {
        fields.pop();
    }
    if fields.len() % record_width != 0 {
        return Err(RefsParseError);
    }
    Ok(fields)
}

/// Parses NUL-delimited worktree porcelain output and resolves administration directories from disk.
///
/// # Errors
/// Returns [`RefsParseError`] for malformed records or unavailable required worktree metadata.
pub fn parse_worktrees(
    bytes: &[u8],
    current: &WorktreeId,
) -> Result<Vec<LinkedWorktree>, RefsParseError> {
    if !bytes.is_empty() && !bytes.ends_with(&[0, 0]) {
        return Err(RefsParseError);
    }
    let mut rows = Vec::new();
    let mut fields = Vec::new();
    for field in bytes.split(|byte| *byte == 0) {
        if field.is_empty() {
            if !fields.is_empty() {
                rows.push(parse_worktree_record(&fields, current)?);
                fields.clear();
            }
        } else {
            fields.push(field);
        }
    }
    if !fields.is_empty() {
        rows.push(parse_worktree_record(&fields, current)?);
    }
    Ok(rows)
}

fn parse_worktree_record(
    fields: &[&[u8]],
    current: &WorktreeId,
) -> Result<LinkedWorktree, RefsParseError> {
    let mut path = None;
    let mut head = None;
    let mut branch = None;
    let mut detached = false;
    let mut locked = None;
    let mut prunable = None;
    for field in fields {
        if let Some(value) = field.strip_prefix(b"worktree ") {
            path = Some(native_path(value)?);
        } else if let Some(value) = field.strip_prefix(b"HEAD ") {
            head = parse_optional_object(value)?;
        } else if let Some(value) = field.strip_prefix(b"branch ") {
            branch = parse_optional_ref(value)?;
        } else if *field == b"detached" {
            detached = true;
        } else if let Some(value) = field.strip_prefix(b"locked") {
            locked = Some(value.strip_prefix(b" ").unwrap_or_default().to_vec());
        } else if let Some(value) = field.strip_prefix(b"prunable") {
            prunable = Some(value.strip_prefix(b" ").unwrap_or_default().to_vec());
        } else if field.starts_with(b"bare") {
            detached = true;
        }
    }
    let root = RepositoryPath::new(path.ok_or(RefsParseError)?).map_err(|_| RefsParseError)?;
    let git_dir = if root == *current.worktree_root() {
        current.git_dir().clone()
    } else {
        resolve_git_dir(&root)
            .or_else(|RefsParseError| resolve_registered_git_dir(&root, current))?
    };
    Ok(LinkedWorktree {
        id: WorktreeId::new(current.repository().clone(), root, git_dir),
        head,
        branch,
        detached,
        locked_reason: locked,
        prunable_reason: prunable,
    })
}

// Prunable worktrees can outlive their checkout. Their administration directory
// still owns their identity, even when the checkout's .git file is unavailable.
fn resolve_registered_git_dir(
    root: &RepositoryPath,
    current: &WorktreeId,
) -> Result<RepositoryPath, RefsParseError> {
    let directory = current
        .repository()
        .common_git_dir()
        .as_path()
        .join("worktrees");
    let marker = root.as_path().join(".git");
    for entry in fs::read_dir(directory).map_err(|_| RefsParseError)? {
        let entry = entry.map_err(|_| RefsParseError)?;
        let Ok(bytes) = fs::read(entry.path().join("gitdir")) else {
            continue;
        };
        if crate::filesystem::same_worktree_path(&native_path(trim_line(&bytes))?, &marker) {
            return RepositoryPath::new(
                fs::canonicalize(entry.path()).map_err(|_| RefsParseError)?,
            )
            .map_err(|_| RefsParseError);
        }
    }
    Err(RefsParseError)
}

fn resolve_git_dir(root: &RepositoryPath) -> Result<RepositoryPath, RefsParseError> {
    let marker = root.as_path().join(".git");
    if marker.is_dir() {
        return RepositoryPath::new(fs::canonicalize(marker).map_err(|_| RefsParseError)?)
            .map_err(|_| RefsParseError);
    }
    let bytes = fs::read(marker).map_err(|_| RefsParseError)?;
    let value = trim_line(bytes.strip_prefix(b"gitdir: ").ok_or(RefsParseError)?);
    let path = native_path(value)?;
    let path = if path.is_absolute() {
        path
    } else {
        root.as_path().join(path)
    };
    RepositoryPath::new(fs::canonicalize(path).map_err(|_| RefsParseError)?)
        .map_err(|_| RefsParseError)
}

#[cfg(unix)]
fn native_path(value: &[u8]) -> Result<PathBuf, RefsParseError> {
    use std::os::unix::ffi::OsStringExt;
    if value.contains(&0) {
        return Err(RefsParseError);
    }
    Ok(PathBuf::from(OsString::from_vec(value.to_vec())))
}

#[cfg(not(unix))]
fn native_path(value: &[u8]) -> Result<PathBuf, RefsParseError> {
    Ok(PathBuf::from(
        std::str::from_utf8(value).map_err(|_| RefsParseError)?,
    ))
}

trait SliceSplitOnce {
    fn split_once(&self, predicate: impl FnMut(&u8) -> bool) -> Option<(&[u8], &[u8])>;
}

impl SliceSplitOnce for [u8] {
    fn split_once(&self, mut predicate: impl FnMut(&u8) -> bool) -> Option<(&[u8], &[u8])> {
        self.iter()
            .position(&mut predicate)
            .map(|index| (&self[..index], &self[index + 1..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_inventory_preserves_strict_fields_and_caller_bound() {
        let raw = b"worktree /main\0HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\0branch refs/heads/main\0\0worktree /other\0detached\0locked reason\0prunable stale\0\0";
        let rows = parse_worktree_list(raw, 2).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].branch.as_ref().unwrap().as_bytes(),
            b"refs/heads/main"
        );
        assert!(rows[1].locked && rows[1].prunable);
        assert_eq!(
            parse_worktree_list(raw, 1),
            Err(WorktreeListError::TooManyWorktrees)
        );
        assert_eq!(
            parse_worktree_list(b"worktree /main\0unknown\0", 1),
            Err(WorktreeListError::InvalidRecord)
        );
        assert_eq!(
            parse_worktree_list(b"HEAD invalid\0", 1),
            Err(WorktreeListError::InvalidObjectId)
        );
        assert_eq!(
            parse_worktree_list(b"detached\0", 1),
            Err(WorktreeListError::MissingPath)
        );
    }

    #[cfg(unix)]
    #[test]
    fn worktree_inventory_preserves_native_path_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let rows = parse_worktree_list(b"worktree /checkout-\xff\0\0", 1).unwrap();
        assert_eq!(rows[0].path.as_os_str().as_bytes(), b"/checkout-\xff");
    }

    #[test]
    fn invalid_remote_urls_do_not_expose_credentials() {
        let rows =
            parse_remotes(b"remote.origin.url\nhttps://user:secret@example.test/repo\ninvalid\0")
                .unwrap();
        assert_eq!(rows[0].url_kind, RemoteUrlKind::Invalid);
        assert_eq!(rows[0].display_url, b"[invalid URL]");
    }

    #[test]
    fn current_worktree_identity_and_record_boundaries_are_preserved() {
        let root = RepositoryPath::new("/checkout").unwrap();
        let git_dir = RepositoryPath::new("/checkout/.git").unwrap();
        let current = WorktreeId::new(
            crate::types::RepositoryId::new(git_dir.clone()),
            root,
            git_dir,
        );
        let bytes = b"worktree /checkout\0HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\0branch refs/heads/main\0locked maintenance\0\0";
        let rows = parse_worktrees(bytes, &current).unwrap();
        assert_eq!(rows[0].id, current);
        assert_eq!(
            rows[0].locked_reason.as_deref(),
            Some(b"maintenance".as_slice())
        );
        assert_eq!(
            rows[0].branch.as_ref().unwrap().as_bytes(),
            b"refs/heads/main"
        );
        assert!(parse_worktrees(&bytes[..bytes.len() - 1], &current).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn local_remote_and_worktree_paths_preserve_non_utf8_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let rows = parse_remotes(b"remote.origin.url\n/local/path-\xff\0").unwrap();
        assert_eq!(rows[0].display_url, b"/local/path-\xff");
        assert_eq!(
            native_path(b"/checkout-\xff")
                .unwrap()
                .as_os_str()
                .as_bytes(),
            b"/checkout-\xff"
        );
    }

    #[test]
    fn branch_upstreams_keep_remote_names_whole() {
        let rows = parse_branch_upstreams(
            b"refs/heads/main\0\0\0\0\nrefs/heads/topic\0refs/remotes/a/b/feature\0a/b\0refs/heads/feature\0\n",
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].remote.as_bytes(), b"a/b");
        assert_eq!(rows[0].remote_ref.as_bytes(), b"refs/heads/feature");
        assert!(parse_branch_upstreams(b"refs/heads/x\0\0origin\0not a ref\0").is_err());
    }

    #[test]
    fn reference_parser_handles_chunks_independently_of_nul_boundaries() {
        let bytes = b"refs/heads/main\0aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\0\0\0refs/remotes/origin/main\0[ahead 2, behind 1]\0*\0\nrefs/tags/v1\0bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\0cccccccccccccccccccccccccccccccccccccccc\0\0\0\0 \0";
        let rows = parse_references(bytes).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].upstream.as_ref().unwrap().ahead, 2);
        assert_eq!(rows[0].upstream.as_ref().unwrap().behind, 1);
        assert!(rows[0].is_head);
        assert_eq!(
            rows[1].peeled_target.as_ref().unwrap().as_str(),
            "cccccccccccccccccccccccccccccccccccccccc"
        );
    }

    #[test]
    fn invalid_remote_url_is_data_not_a_parse_failure() {
        let rows = parse_remotes(
            b"remote.origin.url\nhttps://example.test/repo\0remote.bad.url\nnot a url\0",
        )
        .unwrap();
        assert_eq!(rows[0].url_kind, RemoteUrlKind::Url);
        assert_eq!(rows[1].url_kind, RemoteUrlKind::Invalid);
    }

    #[test]
    fn authenticated_remote_urls_enter_domain_only_as_credential_free_display() {
        let secret = "token-value";
        let rows = parse_remotes(
            format!(
                "remote.origin.url\nhttps://user:{secret}@example.test/repo?access_token={secret}\0"
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(rows[0].url_kind, RemoteUrlKind::Url);
        let rendered = format!("{:?}", rows[0]);
        assert!(!rendered.contains(secret));
        assert_eq!(
            rows[0].display_url,
            b"https://[REDACTED]@example.test/repo?access_token=[REDACTED]"
        );
    }

    #[test]
    fn ssh_logins_stay_visible_while_possible_passwords_are_hidden() {
        let rows = parse_remotes(
            b"remote.a.url\ngit@github.com:devmil/repo.git\0\
              remote.b.url\nssh://git@example.test/repo.git\0\
              remote.c.url\nssh://git:secret@example.test/repo.git\0\
              remote.d.url\nhttps://token@example.test/repo.git\0\
              remote.e.url\nuser:secret@example.test:repo.git\0",
        )
        .unwrap();
        let displays: Vec<_> = rows.iter().map(|row| row.display_url.as_slice()).collect();
        assert_eq!(
            displays,
            [
                b"git@github.com:devmil/repo.git".as_slice(),
                b"ssh://git@example.test/repo.git",
                b"ssh://[REDACTED]@example.test/repo.git",
                b"https://[REDACTED]@example.test/repo.git",
                b"[REDACTED]@example.test:repo.git",
            ]
        );
    }

    #[test]
    fn tracking_parser_covers_all_canonical_states() {
        assert_eq!(parse_tracking(b"[ahead 12]").unwrap(), (12, 0, false));
        assert_eq!(parse_tracking(b"[behind 3]").unwrap(), (0, 3, false));
        assert_eq!(parse_tracking(b"[gone]").unwrap(), (0, 0, true));
    }

    #[test]
    fn truncated_reference_stash_and_remote_records_are_rejected() {
        assert!(parse_references(
            b"refs/heads/main\0aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\0\0\0\0\0"
        )
        .is_err());
        assert!(parse_stashes(b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\0stash@{0}\0").is_err());
        assert!(parse_remotes(b"remote.origin.url\nhttps://example.test/repo").is_err());
    }
}
