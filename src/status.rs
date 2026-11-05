//! Byte-safe repository status and operation-state values.

use crate::types::{ObjectId, RepositoryRelativePath};
#[cfg(test)]
use crate::version::GitVersion;
#[cfg(unix)]
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ChangeKind {
    Unmodified,
    Modified,
    TypeChanged,
    Added,
    Deleted,
    Renamed,
    Copied,
    UpdatedButUnmerged,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FileState {
    pub index: ChangeKind,
    pub worktree: ChangeKind,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SubmoduleKind {
    #[default]
    NotSubmodule,
    Submodule,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SubmoduleState {
    pub kind: SubmoduleKind,
    pub commit_changed: bool,
    pub tracked_changes: bool,
    pub untracked_changes: bool,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OrdinaryStatus {
    pub path: RepositoryRelativePath,
    pub state: FileState,
    pub submodule: SubmoduleState,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RenameStatus {
    pub path: RepositoryRelativePath,
    pub source: RepositoryRelativePath,
    pub state: FileState,
    pub submodule: SubmoduleState,
    pub score: u16,
    pub copy: bool,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UnmergedStatus {
    pub path: RepositoryRelativePath,
    pub state: FileState,
    pub submodule: SubmoduleState,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum StatusEntry {
    Ordinary(OrdinaryStatus),
    Rename(RenameStatus),
    Unmerged(UnmergedStatus),
    Untracked(RepositoryRelativePath),
    Ignored(RepositoryRelativePath),
}

impl StatusEntry {
    pub fn path(&self) -> &RepositoryRelativePath {
        match self {
            Self::Ordinary(item) => &item.path,
            Self::Rename(item) => &item.path,
            Self::Unmerged(item) => &item.path,
            Self::Untracked(path) | Self::Ignored(path) => path,
        }
    }

    pub const fn is_conflicted(&self) -> bool {
        matches!(self, Self::Unmerged(_))
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StatusRefName(Vec<u8>);

impl StatusRefName {
    /// # Errors
    ///
    /// Empty values and values containing NUL are not stable Git status names.
    pub fn from_bytes(value: impl Into<Vec<u8>>) -> Result<Self, StatusValueError> {
        let value = value.into();
        if value.is_empty() || value.contains(&0) {
            Err(StatusValueError)
        } else {
            Ok(Self(value))
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatusValueError;

impl fmt::Display for StatusValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("status reference name is empty or contains NUL")
    }
}

impl std::error::Error for StatusValueError {}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum BranchHead {
    Attached(StatusRefName),
    Detached,
    Unborn { branch: StatusRefName },
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BranchStatus {
    pub oid: Option<ObjectId>,
    pub head: BranchHead,
    pub upstream: Option<StatusRefName>,
    pub ahead: u64,
    pub behind: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum OperationKind {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OperationState {
    pub kind: OperationKind,
    pub current: Option<u32>,
    pub total: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum WorkingTreeState {
    Clean,
    Dirty,
    Conflicted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryStatus {
    pub branch: BranchStatus,
    pub entries: Vec<StatusEntry>,
    pub working_tree: WorkingTreeState,
    pub operations: Vec<OperationState>,
}

impl RepositoryStatus {
    pub fn from_parts(
        branch: BranchStatus,
        entries: Vec<StatusEntry>,
        operations: Vec<OperationState>,
    ) -> Self {
        let working_tree = if entries.iter().any(StatusEntry::is_conflicted) {
            WorkingTreeState::Conflicted
        } else if entries.is_empty() {
            WorkingTreeState::Clean
        } else {
            WorkingTreeState::Dirty
        };
        Self {
            branch,
            entries,
            working_tree,
            operations,
        }
    }
}

#[cfg(test)]
mod model_tests {
    use super::*;

    fn branch() -> BranchStatus {
        BranchStatus {
            oid: None,
            head: BranchHead::Unborn {
                branch: StatusRefName::from_bytes(b"main".to_vec()).unwrap(),
            },
            upstream: None,
            ahead: 0,
            behind: 0,
        }
    }

    #[test]
    fn working_tree_state_is_derived_from_entries() {
        assert_eq!(
            RepositoryStatus::from_parts(branch(), Vec::new(), Vec::new()).working_tree,
            WorkingTreeState::Clean
        );
        let path = RepositoryRelativePath::new("file").unwrap();
        let ordinary = StatusEntry::Ordinary(OrdinaryStatus {
            path: path.clone(),
            state: FileState {
                index: ChangeKind::Modified,
                worktree: ChangeKind::Unmodified,
            },
            submodule: SubmoduleState::default(),
        });
        assert_eq!(
            RepositoryStatus::from_parts(branch(), vec![ordinary], Vec::new()).working_tree,
            WorkingTreeState::Dirty
        );
        let unmerged = StatusEntry::Unmerged(UnmergedStatus {
            path,
            state: FileState {
                index: ChangeKind::UpdatedButUnmerged,
                worktree: ChangeKind::UpdatedButUnmerged,
            },
            submodule: SubmoduleState::default(),
        });
        assert_eq!(
            RepositoryStatus::from_parts(branch(), vec![unmerged], Vec::new()).working_tree,
            WorkingTreeState::Conflicted
        );
    }
}

#[derive(Debug)]
pub struct StatusParseError;

impl fmt::Display for StatusParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid Git porcelain-v2 status output")
    }
}
impl std::error::Error for StatusParseError {}

/// Parses one complete NUL-delimited `git status --porcelain=v2 --branch -z` response.
///
/// # Errors
///
/// Returns [`StatusParseError`] for missing branch identity, malformed fields, invalid object IDs,
/// invalid native paths, truncated rename records, and unknown record or state codes.
pub fn parse_porcelain_v2(
    bytes: &[u8],
) -> Result<(BranchStatus, Vec<StatusEntry>), StatusParseError> {
    if !bytes.ends_with(&[0]) {
        return Err(StatusParseError);
    }
    let mut oid = None;
    let mut initial = false;
    let mut head = None;
    let mut upstream = None;
    let mut ahead = 0;
    let mut behind = 0;
    let mut entries = Vec::new();
    let mut records = bytes.split(|byte| *byte == 0).peekable();
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        if let Some(value) = record.strip_prefix(b"# branch.oid ") {
            if value == b"(initial)" {
                initial = true;
            } else {
                oid = Some(parse_oid(value)?);
            }
        } else if let Some(value) = record.strip_prefix(b"# branch.head ") {
            head = Some(value.to_vec());
        } else if let Some(value) = record.strip_prefix(b"# branch.upstream ") {
            upstream = Some(value.to_vec());
        } else if let Some(value) = record.strip_prefix(b"# branch.ab ") {
            let fields = split_fields(value, 2)?;
            ahead = parse_prefixed_u64(fields[0], b'+')?;
            behind = parse_prefixed_u64(fields[1], b'-')?;
        } else {
            match record.first() {
                Some(b'1') => entries.push(parse_ordinary(record)?),
                Some(b'2') => {
                    let source = records.next().ok_or(StatusParseError)?;
                    entries.push(parse_rename(record, source)?);
                }
                Some(b'u') => entries.push(parse_unmerged(record)?),
                Some(b'?') => entries.push(StatusEntry::Untracked(parse_record_path(record)?)),
                Some(b'!') => entries.push(StatusEntry::Ignored(parse_record_path(record)?)),
                _ => return Err(StatusParseError),
            }
        }
    }
    let head = head.ok_or(StatusParseError)?;
    let head = if head == b"(detached)" {
        if oid.is_none() {
            return Err(StatusParseError);
        }
        BranchHead::Detached
    } else if initial {
        BranchHead::Unborn {
            branch: StatusRefName::from_bytes(head).map_err(|_| StatusParseError)?,
        }
    } else {
        if oid.is_none() {
            return Err(StatusParseError);
        }
        BranchHead::Attached(StatusRefName::from_bytes(head).map_err(|_| StatusParseError)?)
    };
    Ok((
        BranchStatus {
            oid,
            head,
            upstream: upstream
                .map(StatusRefName::from_bytes)
                .transpose()
                .map_err(|_| StatusParseError)?,
            ahead,
            behind,
        },
        entries,
    ))
}

fn parse_ordinary(record: &[u8]) -> Result<StatusEntry, StatusParseError> {
    let fields = split_fields(record, 9)?;
    validate_modes_and_oids(&fields[3..=5], &fields[6..=7])?;
    Ok(StatusEntry::Ordinary(OrdinaryStatus {
        state: parse_xy(fields[1])?,
        submodule: parse_submodule(fields[2])?,
        path: native_path(fields[8])?,
    }))
}

fn parse_rename(record: &[u8], source: &[u8]) -> Result<StatusEntry, StatusParseError> {
    let fields = split_fields(record, 10)?;
    validate_modes_and_oids(&fields[3..=5], &fields[6..=7])?;
    let score_field = fields[8];
    let (copy, digits) = match score_field.first() {
        Some(b'R') => (false, &score_field[1..]),
        Some(b'C') => (true, &score_field[1..]),
        _ => return Err(StatusParseError),
    };
    let score = parse_u16(digits)?;
    if score > 100 {
        return Err(StatusParseError);
    }
    Ok(StatusEntry::Rename(RenameStatus {
        state: parse_xy(fields[1])?,
        submodule: parse_submodule(fields[2])?,
        score,
        copy,
        path: native_path(fields[9])?,
        source: native_path(source)?,
    }))
}

fn parse_unmerged(record: &[u8]) -> Result<StatusEntry, StatusParseError> {
    let fields = split_fields(record, 11)?;
    validate_modes_and_oids(&fields[3..=6], &fields[7..=9])?;
    Ok(StatusEntry::Unmerged(UnmergedStatus {
        state: parse_xy(fields[1])?,
        submodule: parse_submodule(fields[2])?,
        path: native_path(fields[10])?,
    }))
}

fn parse_record_path(record: &[u8]) -> Result<RepositoryRelativePath, StatusParseError> {
    if record.get(1) != Some(&b' ') {
        return Err(StatusParseError);
    }
    native_path(&record[2..])
}

fn split_fields(bytes: &[u8], count: usize) -> Result<Vec<&[u8]>, StatusParseError> {
    let fields: Vec<_> = bytes.splitn(count, |byte| *byte == b' ').collect();
    if fields.len() == count && fields.iter().all(|field| !field.is_empty()) {
        Ok(fields)
    } else {
        Err(StatusParseError)
    }
}

fn parse_xy(value: &[u8]) -> Result<FileState, StatusParseError> {
    if value.len() != 2 {
        return Err(StatusParseError);
    }
    Ok(FileState {
        index: parse_change(value[0])?,
        worktree: parse_change(value[1])?,
    })
}

fn parse_change(value: u8) -> Result<ChangeKind, StatusParseError> {
    match value {
        b'.' => Ok(ChangeKind::Unmodified),
        b'M' => Ok(ChangeKind::Modified),
        b'T' => Ok(ChangeKind::TypeChanged),
        b'A' => Ok(ChangeKind::Added),
        b'D' => Ok(ChangeKind::Deleted),
        b'R' => Ok(ChangeKind::Renamed),
        b'C' => Ok(ChangeKind::Copied),
        b'U' => Ok(ChangeKind::UpdatedButUnmerged),
        _ => Err(StatusParseError),
    }
}

fn validate_modes_and_oids(modes: &[&[u8]], oids: &[&[u8]]) -> Result<(), StatusParseError> {
    if modes
        .iter()
        .any(|mode| mode.len() != 6 || !mode.iter().all(|byte| matches!(byte, b'0'..=b'7')))
        || oids.iter().any(|oid| parse_oid(oid).is_err())
    {
        Err(StatusParseError)
    } else {
        Ok(())
    }
}

fn parse_submodule(value: &[u8]) -> Result<SubmoduleState, StatusParseError> {
    if value == b"N..." {
        return Ok(SubmoduleState::default());
    }
    if value.len() != 4 || value[0] != b'S' {
        return Err(StatusParseError);
    }
    Ok(SubmoduleState {
        kind: SubmoduleKind::Submodule,
        commit_changed: flag(value[1], b'C')?,
        tracked_changes: flag(value[2], b'M')?,
        untracked_changes: flag(value[3], b'U')?,
    })
}

fn flag(value: u8, set: u8) -> Result<bool, StatusParseError> {
    match value {
        b'.' => Ok(false),
        value if value == set => Ok(true),
        _ => Err(StatusParseError),
    }
}

fn parse_oid(value: &[u8]) -> Result<ObjectId, StatusParseError> {
    std::str::from_utf8(value)
        .map_err(|_| StatusParseError)
        .and_then(|value| ObjectId::parse(value).map_err(|_| StatusParseError))
}

fn parse_prefixed_u64(value: &[u8], prefix: u8) -> Result<u64, StatusParseError> {
    if value.first() != Some(&prefix) {
        return Err(StatusParseError);
    }
    std::str::from_utf8(&value[1..])
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or(StatusParseError)
}

fn parse_u16(value: &[u8]) -> Result<u16, StatusParseError> {
    std::str::from_utf8(value)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or(StatusParseError)
}

#[cfg(test)]
fn parse_git_version(bytes: &[u8]) -> Option<GitVersion> {
    let version = crate::version::GitVersion::parse(bytes)?;
    Some(GitVersion::new(version.major, version.minor, version.patch))
}

/// Detects active sequencer operations from Git administration files.
/// Missing or unreadable progress counters are left unknown.
///
/// This lenient form never fails: symlinked state is followed, an ambiguous pair of rebase
/// directories reads as the merge backend, and malformed markers are ignored. Prefer
/// [`discover_operations_checked`], which reports those states as errors. Both use
/// `BISECT_START` as the bisect marker (see [`discover_operations_checked`]).
pub fn discover_operations(git_dir: &Path) -> Vec<OperationState> {
    scan_operations(git_dir, false).map_or_else(|_| Vec::new(), |found| found.operations)
}

/// Bytes read at most from one operation state file.
pub const MAX_OPERATION_FILE_BYTES: u64 = 64 * 1024;

/// Why [`discover_operations_checked`] could not describe the in-progress operations.
/// Values never contain file contents or paths.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OperationDiscoveryError {
    /// A state file or directory is a symbolic link, or has the wrong file type.
    UnsafeState,
    /// Git state contradicts itself, such as both rebase backends or two sequencers at once.
    Ambiguous,
    /// A present state file is oversized or does not have Git's format.
    Malformed,
    /// A state file exists but could not be read.
    Unreadable,
}

impl fmt::Display for OperationDiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnsafeState => "a Git operation state entry is a link or has the wrong type",
            Self::Ambiguous => "the Git operation state is ambiguous",
            Self::Malformed => "a Git operation state file is malformed",
            Self::Unreadable => "a Git operation state file could not be read",
        })
    }
}

impl std::error::Error for OperationDiscoveryError {}

/// The in-progress operations of one worktree's Git directory.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiscoveredOperations {
    /// The same operation list [`discover_operations`] returns for a well-formed directory.
    pub operations: Vec<OperationState>,
    /// The typed sequencer state of an in-progress rebase, from either backend.
    pub rebase: Option<crate::rebase::RebaseSequencerState>,
    /// `rebase-apply` belongs to `git am` rather than to a rebase (`rebase-apply/applying`).
    /// The operation list still reports it as [`OperationKind::Rebase`], as Git's own status
    /// line for the shared directory does not distinguish them either.
    pub mailbox_apply: bool,
}

/// Detects in-progress operations and fails closed on suspicious state.
///
/// * Every state entry is inspected with `symlink_metadata`: a symlinked or wrongly typed
///   `rebase-merge`, `rebase-apply`, `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD` or
///   `BISECT_START` is [`OperationDiscoveryError::UnsafeState`].
/// * Both rebase directories at once, or more than one of `MERGE_HEAD`, `CHERRY_PICK_HEAD`
///   and `REVERT_HEAD` outside a rebase, is [`OperationDiscoveryError::Ambiguous`]. Git
///   itself refuses to start a second sequencer while one is unfinished. A merge stop inside
///   a rebase (`--rebase-merges`) and a bisect alongside anything are legitimate.
/// * Head markers must hold full object IDs (`MERGE_HEAD` one per line), and rebase state
///   files are parsed by [`crate::rebase::parse_sequencer_state`] for both backends.
///
/// Bisect is detected by `BISECT_START`. `git bisect start` writes it first, and the bisect
/// command itself treats its presence as "a bisect session exists" (`git bisect reset` reports
/// "We are not bisecting" without it, and `git bisect good/bad` auto-start from it). `BISECT_LOG`
/// is only the replayable journal; the two are created and removed together by `git bisect`.
///
/// # Errors
/// Returns [`OperationDiscoveryError`] for unsafe, ambiguous, malformed or unreadable state.
pub fn discover_operations_checked(
    git_dir: &Path,
) -> Result<DiscoveredOperations, OperationDiscoveryError> {
    scan_operations(git_dir, true)
}

fn entry_exists(
    path: &Path,
    directory: bool,
    strict: bool,
) -> Result<bool, OperationDiscoveryError> {
    if !strict {
        return Ok(if directory {
            path.is_dir()
        } else {
            path.is_file()
        });
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(OperationDiscoveryError::Unreadable),
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink()
        || (directory && !file_type.is_dir())
        || (!directory && !file_type.is_file())
    {
        return Err(OperationDiscoveryError::UnsafeState);
    }
    Ok(true)
}

fn read_state_file(path: &Path, strict: bool) -> Result<Option<Vec<u8>>, OperationDiscoveryError> {
    if !entry_exists(path, false, strict)? {
        return Ok(None);
    }
    let read = || -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        fs::File::open(path)?
            .take(MAX_OPERATION_FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    match read() {
        Ok(bytes) if bytes.len() as u64 > MAX_OPERATION_FILE_BYTES => {
            if strict {
                Err(OperationDiscoveryError::Malformed)
            } else {
                Ok(None)
            }
        }
        Ok(bytes) => Ok(Some(bytes)),
        Err(_) if !strict => Ok(None),
        Err(_) => Err(OperationDiscoveryError::Unreadable),
    }
}

fn read_done_tail(path: &Path, strict: bool) -> Result<Option<Vec<u8>>, OperationDiscoveryError> {
    if !entry_exists(path, false, strict)? {
        return Ok(None);
    }
    let read = || -> std::io::Result<Vec<u8>> {
        let mut file = fs::File::open(path)?;
        let length = file.metadata()?.len();
        file.seek(SeekFrom::Start(
            length.saturating_sub(MAX_OPERATION_FILE_BYTES),
        ))?;
        let mut bytes = Vec::new();
        file.take(MAX_OPERATION_FILE_BYTES)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    match read() {
        Ok(bytes) => Ok(Some(bytes)),
        Err(_) if !strict => Ok(None),
        Err(_) => Err(OperationDiscoveryError::Unreadable),
    }
}

fn head_marker_is_valid(bytes: &[u8]) -> bool {
    let mut lines = bytes
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.is_empty())
        .peekable();
    lines.peek().is_some()
        && lines
            .all(|line| std::str::from_utf8(line).is_ok_and(|text| ObjectId::parse(text).is_ok()))
}

fn lenient_number(bytes: Option<&[u8]>) -> Option<u32> {
    std::str::from_utf8(bytes?).ok()?.trim().parse().ok()
}

fn scan_operations(
    git_dir: &Path,
    strict: bool,
) -> Result<DiscoveredOperations, OperationDiscoveryError> {
    use crate::rebase::{parse_sequencer_state, RebaseSequencerFiles, RebaseSequencerMode};
    let mut found = DiscoveredOperations::default();
    let rebase_merge = git_dir.join("rebase-merge");
    let rebase_apply = git_dir.join("rebase-apply");
    let merge_backend = entry_exists(&rebase_merge, true, strict)?;
    let apply_backend = entry_exists(&rebase_apply, true, strict)?;
    if strict && merge_backend && apply_backend {
        return Err(OperationDiscoveryError::Ambiguous);
    }
    if merge_backend || apply_backend {
        let (directory, mode, step, total) = if merge_backend {
            (&rebase_merge, RebaseSequencerMode::Merge, "msgnum", "end")
        } else {
            (&rebase_apply, RebaseSequencerMode::Apply, "next", "last")
        };
        let read = |name: &str| read_state_file(&directory.join(name), strict);
        let files = RebaseSequencerFiles {
            current_step: read(step)?,
            total_steps: read(total)?,
            onto: read("onto")?,
            orig_head: read("orig-head")?,
            head_name: read("head-name")?,
            stopped_sha: if merge_backend {
                read("stopped-sha")?
            } else {
                None
            },
            done_tail: if merge_backend {
                read_done_tail(&directory.join("done"), strict)?
            } else {
                None
            },
        };
        if strict {
            let state = parse_sequencer_state(mode, &files)
                .map_err(|_| OperationDiscoveryError::Malformed)?;
            found.operations.push(OperationState {
                kind: OperationKind::Rebase,
                current: state.current_step(),
                total: state.total_steps(),
            });
            found.rebase = Some(state);
            found.mailbox_apply =
                apply_backend && entry_exists(&rebase_apply.join("applying"), false, true)?;
        } else {
            // The legacy reader tried both counter names in either directory.
            let other = |name: &str| read_state_file(&directory.join(name), false).ok().flatten();
            found.operations.push(OperationState {
                kind: OperationKind::Rebase,
                current: lenient_number(files.current_step.as_deref()).or_else(|| {
                    lenient_number(other(if merge_backend { "next" } else { "msgnum" }).as_deref())
                }),
                total: lenient_number(files.total_steps.as_deref()).or_else(|| {
                    lenient_number(other(if merge_backend { "last" } else { "end" }).as_deref())
                }),
            });
            found.rebase = parse_sequencer_state(mode, &files).ok();
        }
    }
    let mut sequencers = 0;
    for (marker, kind) in [
        ("MERGE_HEAD", OperationKind::Merge),
        ("CHERRY_PICK_HEAD", OperationKind::CherryPick),
        ("REVERT_HEAD", OperationKind::Revert),
        ("BISECT_START", OperationKind::Bisect),
    ] {
        let path = git_dir.join(marker);
        let present = if kind == OperationKind::Bisect {
            entry_exists(&path, false, strict)?
        } else if strict {
            match read_state_file(&path, true)? {
                Some(bytes) if head_marker_is_valid(&bytes) => true,
                Some(_) => return Err(OperationDiscoveryError::Malformed),
                None => false,
            }
        } else {
            path.is_file()
        };
        if present {
            if kind != OperationKind::Bisect {
                sequencers += 1;
            }
            found.operations.push(OperationState {
                kind,
                current: None,
                total: None,
            });
        }
    }
    if strict && sequencers > 1 && !(merge_backend || apply_backend) {
        return Err(OperationDiscoveryError::Ambiguous);
    }
    Ok(found)
}

/// The commits an in-progress merge, cherry-pick or revert is applying: the object IDs in
/// `MERGE_HEAD` (one per line, for an octopus merge), `CHERRY_PICK_HEAD` or `REVERT_HEAD`.
/// Returns an empty list when the marker is absent and for [`OperationKind::Rebase`] and
/// [`OperationKind::Bisect`], which have no head marker. The marker is read with the checks of
/// [`discover_operations_checked`].
///
/// # Errors
/// Returns [`OperationDiscoveryError`] for a linked, wrongly typed, oversized, malformed or
/// unreadable marker.
pub fn operation_heads(
    git_dir: &Path,
    kind: OperationKind,
) -> Result<Vec<ObjectId>, OperationDiscoveryError> {
    let marker = match kind {
        OperationKind::Merge => "MERGE_HEAD",
        OperationKind::CherryPick => "CHERRY_PICK_HEAD",
        OperationKind::Revert => "REVERT_HEAD",
        OperationKind::Rebase | OperationKind::Bisect => return Ok(Vec::new()),
    };
    let Some(bytes) = read_state_file(&git_dir.join(marker), true)? else {
        return Ok(Vec::new());
    };
    if !head_marker_is_valid(&bytes) {
        return Err(OperationDiscoveryError::Malformed);
    }
    bytes
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.is_empty())
        .map(|line| {
            std::str::from_utf8(line)
                .ok()
                .and_then(|text| ObjectId::parse(text).ok())
                .ok_or(OperationDiscoveryError::Malformed)
        })
        .collect()
}

#[cfg(unix)]
fn native_path(bytes: &[u8]) -> Result<RepositoryRelativePath, StatusParseError> {
    use std::os::unix::ffi::OsStringExt;
    RepositoryRelativePath::new(std::path::PathBuf::from(OsString::from_vec(bytes.to_vec())))
        .map_err(|_| StatusParseError)
}

#[cfg(not(unix))]
fn native_path(bytes: &[u8]) -> Result<RepositoryRelativePath, StatusParseError> {
    let path = std::str::from_utf8(bytes).map_err(|_| StatusParseError)?;
    RepositoryRelativePath::new(path).map_err(|_| StatusParseError)
}

#[cfg(test)]
mod p02_status_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const OID: &str = "1111111111111111111111111111111111111111";
    static NEXT: AtomicU64 = AtomicU64::new(1);

    fn fixture() -> Vec<u8> {
        let mut bytes = format!(
            "# branch.oid {OID}\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +3 -2\0\
             1 M. SCMU 100644 100644 100644 {OID} {OID} ordinary name\0\
             2 R. N... 100644 100644 100644 {OID} {OID} R087 renamed path\0source path\0\
             2 C. N... 100644 100644 100644 {OID} {OID} C100 copied path\0copy source\0\
             u UU N... 100644 100644 100644 100644 {OID} {OID} {OID} conflict\0\
             ? untracked name\0! ignored name\0"
        )
        .into_bytes();
        bytes.retain(|byte| *byte != b'\n');
        bytes
    }

    #[test]
    fn parses_every_porcelain_record_branch_tracking_and_submodule_bits() {
        let (branch, entries) = parse_porcelain_v2(&fixture()).unwrap();
        assert_eq!(branch.ahead, 3);
        assert_eq!(branch.behind, 2);
        assert_eq!(branch.upstream.unwrap().as_bytes(), b"origin/main");
        assert!(
            matches!(branch.head, BranchHead::Attached(ref name) if name.as_bytes() == b"main")
        );
        assert_eq!(entries.len(), 6);
        let StatusEntry::Ordinary(ordinary) = &entries[0] else {
            panic!("ordinary record changed kind");
        };
        assert_eq!(ordinary.submodule.kind, SubmoduleKind::Submodule);
        assert!(ordinary.submodule.commit_changed);
        assert!(ordinary.submodule.tracked_changes);
        assert!(ordinary.submodule.untracked_changes);
        assert!(matches!(&entries[1], StatusEntry::Rename(item) if !item.copy && item.score == 87));
        assert!(matches!(&entries[2], StatusEntry::Rename(item) if item.copy && item.score == 100));
        assert!(entries[3].is_conflicted());
        assert!(matches!(entries[4], StatusEntry::Untracked(_)));
        assert!(matches!(entries[5], StatusEntry::Ignored(_)));
    }

    #[test]
    fn unborn_detached_and_malformed_outputs_are_distinct() {
        assert!(parse_porcelain_v2(b"# branch.oid (initial)\0# branch.head topic").is_err());
        let (unborn, _) =
            parse_porcelain_v2(b"# branch.oid (initial)\0# branch.head topic\0").unwrap();
        assert!(matches!(unborn.head, BranchHead::Unborn { .. }));
        let detached = format!("# branch.oid {OID}\0# branch.head (detached)\0");
        assert!(matches!(
            parse_porcelain_v2(detached.as_bytes()).unwrap().0.head,
            BranchHead::Detached
        ));
        assert!(parse_porcelain_v2(b"# branch.oid nope\0# branch.head main\0").is_err());
        assert!(parse_porcelain_v2(b"2 R. N... truncated\0").is_err());
        assert!(parse_porcelain_v2(b"# branch.oid (initial)\0").is_err());
    }

    #[test]
    fn capability_version_parser_accepts_vendor_suffixes() {
        assert_eq!(
            parse_git_version(b"git version 2.47.1.windows.1\n"),
            Some(GitVersion {
                major: 2,
                minor: 47,
                patch: 1
            })
        );
        assert_eq!(
            parse_git_version(b"git version 3.0\n"),
            Some(GitVersion {
                major: 3,
                minor: 0,
                patch: 0
            })
        );
        assert_eq!(parse_git_version(b"not git"), None);
    }

    #[test]
    fn every_sequencer_marker_and_rebase_progress_is_detected() {
        let path = std::env::temp_dir().join(format!(
            "devmil-git-status-markers-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join("rebase-merge")).unwrap();
        fs::write(path.join("rebase-merge/msgnum"), "2\n").unwrap();
        fs::write(path.join("rebase-merge/end"), "7\n").unwrap();
        for marker in [
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "BISECT_START",
        ] {
            fs::write(path.join(marker), b"marker").unwrap();
        }
        let operations = discover_operations(&path);
        assert_eq!(operations.len(), 5);
        assert_eq!(operations[0].kind, OperationKind::Rebase);
        assert_eq!(operations[0].current, Some(2));
        assert_eq!(operations[0].total, Some(7));
        assert_eq!(operations[1].kind, OperationKind::Merge);
        assert_eq!(operations[2].kind, OperationKind::CherryPick);
        assert_eq!(operations[3].kind, OperationKind::Revert);
        assert_eq!(operations[4].kind, OperationKind::Bisect);
        fs::remove_dir_all(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_are_native_and_stable() {
        use std::os::unix::ffi::OsStrExt;
        let mut bytes = format!("# branch.oid {OID}\0# branch.head main\0? path-").into_bytes();
        bytes.extend_from_slice(b"\xff\0");
        let (_, entries) = parse_porcelain_v2(&bytes).unwrap();
        assert_eq!(
            entries[0].path().as_path().as_os_str().as_bytes(),
            b"path-\xff"
        );
    }
}
