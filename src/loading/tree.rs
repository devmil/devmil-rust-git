//! Bounded line history and reusable committed-directory snapshots.
use super::CommandExecutor;
use crate::activity::{ActivityRecord, ActivityStatus};
use crate::diff::model::DiffPath;
use crate::process::{GitProcessCommand, ProcessArgument};
use crate::types::{ObjectId, RepositoryPath};
use std::{ffi::OsString, fmt};
pub const READ_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
pub const DEFAULT_RANGE_HISTORY_LIMIT: usize = 100;
pub const MAX_RANGE_HISTORY_LIMIT: usize = 500;
pub const DEFAULT_COMMIT_TREE_LIMIT: usize = 250;
pub const MAX_COMMIT_TREE_LIMIT: usize = 1_000;
pub const MAX_RANGE_LINES: u32 = 20_000;

#[derive(Clone, Debug)]
pub struct RangeHistoryQuery {
    pub repository: RepositoryPath,
    pub revision: ObjectId,
    pub path: DiffPath,
    pub first_line: u32,
    pub last_line: u32,
    pub limit: usize,
}

impl RangeHistoryQuery {
    /// # Errors
    ///
    /// Refuses reversed, zero, oversized line ranges and result limits.
    pub fn validate(&self) -> Result<(), RichHistoryError> {
        if self.first_line == 0
            || self.last_line < self.first_line
            || self.last_line.saturating_sub(self.first_line) >= MAX_RANGE_LINES
        {
            return Err(invalid("selected history range is outside its line bound"));
        }
        if self.limit == 0 || self.limit > MAX_RANGE_HISTORY_LIMIT {
            return Err(invalid(
                "selected history result limit is outside its bound",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RangeHistoryEntry {
    pub commit: ObjectId,
    pub author: Vec<u8>,
    pub author_time_unix: i64,
    pub summary: Vec<u8>,
}

impl RangeHistoryEntry {
    #[must_use]
    pub fn stable_identity(&self) -> String {
        format!("range-history:{}", self.commit.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RangeHistoryResult {
    pub entries: Vec<RangeHistoryEntry>,
    pub incomplete: bool,
    pub activities: Vec<ActivityRecord>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitTreeEntryKind {
    Directory,
    File,
    Symlink,
    Submodule,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitTreeEntry {
    pub path: DiffPath,
    pub object: ObjectId,
    pub kind: CommitTreeEntryKind,
    pub mode: u32,
    pub size: Option<u64>,
}

impl CommitTreeEntry {
    #[must_use]
    pub fn stable_identity(&self, commit: &ObjectId) -> String {
        format!(
            "commit-tree:{}:{}:{}",
            commit.as_str(),
            hex_path(self.path.as_bytes()),
            self.object.as_str()
        )
    }
}

#[derive(Clone, Debug)]
pub struct CommitTreeQuery {
    pub repository: RepositoryPath,
    pub commit: ObjectId,
    /// `None` names the root. Directories are loaded one level at a time.
    pub directory: Option<DiffPath>,
    pub offset: usize,
    pub limit: usize,
}

impl CommitTreeQuery {
    /// # Errors
    ///
    /// Refuses an empty or oversized page.
    pub fn validate(&self) -> Result<(), RichHistoryError> {
        if self.limit == 0 || self.limit > MAX_COMMIT_TREE_LIMIT {
            return Err(invalid("commit tree page size is outside its bound"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitTreePage {
    pub entries: Vec<CommitTreeEntry>,
    pub next_offset: Option<usize>,
    pub activities: Vec<ActivityRecord>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RichHistoryErrorKind {
    InvalidRequest,
    GitFailed,
    Parse,
    TooLarge,
    Canceled,
    Snapshot,
    InvalidResult,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RichHistoryError {
    kind: RichHistoryErrorKind,
    detail: String,
}

impl RichHistoryError {
    #[must_use]
    pub fn new(kind: RichHistoryErrorKind, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        if detail.len() > 512 {
            let mut boundary = 512;
            while !detail.is_char_boundary(boundary) {
                boundary -= 1;
            }
            detail.truncate(boundary);
        }
        Self { kind, detail }
    }

    pub const fn kind(&self) -> RichHistoryErrorKind {
        self.kind
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for RichHistoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for RichHistoryError {}

fn invalid(detail: &'static str) -> RichHistoryError {
    RichHistoryError::new(RichHistoryErrorKind::InvalidRequest, detail)
}

fn hex_path(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn run(
    executable: &std::ffi::OsStr,
    repository: &RepositoryPath,
    arguments: Vec<ProcessArgument>,
    executor: &mut dyn CommandExecutor,
    output_limit: usize,
) -> Result<(ActivityRecord, Vec<u8>), RichHistoryError> {
    let command = GitProcessCommand::git(executable.to_os_string())
        .current_dir(repository.as_path())
        .args(arguments)
        .output_limit(output_limit)
        .env("LC_ALL", "C")
        .expect("constant environment name is valid")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .expect("constant environment name is valid")
        .redact_output();
    let mut run = executor.execute(command);
    let activity = run.activity;
    let truncated = run.stdout.truncated;
    let stdout = run.stdout.take_bytes();
    let stderr = run.stderr.take_bytes();
    match &activity.status {
        ActivityStatus::Succeeded { .. } if !truncated => Ok((activity, stdout)),
        ActivityStatus::Canceled { .. } => Err(RichHistoryError::new(
            RichHistoryErrorKind::Canceled,
            "history operation was canceled",
        )),
        ActivityStatus::Succeeded { .. } => Err(RichHistoryError::new(
            RichHistoryErrorKind::TooLarge,
            "history output exceeds its configured byte bound",
        )),
        _ => Err(RichHistoryError::new(
            RichHistoryErrorKind::GitFailed,
            bounded_stderr(&stderr),
        )),
    }
}

/// Loads bounded history for a committed line range.
/// # Errors
/// Rejects invalid bounds, failed Git, and malformed output.
pub fn load_range(
    executable: &std::ffi::OsStr,
    request: &RangeHistoryQuery,
    executor: &mut dyn CommandExecutor,
) -> Result<RangeHistoryResult, RichHistoryError> {
    request.validate()?;
    let range = range_argument(request)?;
    let probe_limit = request.limit.saturating_add(1);
    let (activity, bytes) = run(
        executable,
        &request.repository,
        vec![
            ProcessArgument::visible("log"),
            ProcessArgument::visible("--no-patch"),
            ProcessArgument::visible("--format=%x1e%H%x00%an%x00%at%x00%s%x00"),
            ProcessArgument::visible(format!("--max-count={probe_limit}")),
            ProcessArgument::visible(range),
            ProcessArgument::visible(request.revision.as_str()),
        ],
        executor,
        READ_OUTPUT_LIMIT,
    )?;
    let mut entries = parse_range_history(&bytes)?;
    let incomplete = entries.len() > request.limit;
    entries.truncate(request.limit);
    Ok(RangeHistoryResult {
        entries,
        incomplete,
        activities: vec![activity],
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitTreeSnapshot {
    pub entries: Vec<CommitTreeEntry>,
    pub activities: Vec<ActivityRecord>,
}
impl CommitTreeSnapshot {
    /// Slices a previously loaded directory without invoking Git again.
    /// # Errors
    /// Rejects zero or oversized pages.
    pub fn page(&self, offset: usize, limit: usize) -> Result<CommitTreePage, RichHistoryError> {
        if limit == 0 || limit > MAX_COMMIT_TREE_LIMIT {
            return Err(invalid("commit tree page size is outside its bound"));
        }
        let end = offset.saturating_add(limit).min(self.entries.len());
        let entries = self.entries.get(offset..end).unwrap_or_default().to_vec();
        Ok(CommitTreePage {
            entries,
            next_offset: (end < self.entries.len()).then_some(end),
            activities: Vec::new(),
        })
    }
}
/// Loads a committed directory once for subsequent local pagination.
/// # Errors
/// Rejects failed commands, oversized output and malformed tree entries.
pub fn load_tree(
    executable: &std::ffi::OsStr,
    repository: &RepositoryPath,
    commit: &ObjectId,
    directory: Option<&DiffPath>,
    executor: &mut dyn CommandExecutor,
) -> Result<CommitTreeSnapshot, RichHistoryError> {
    let treeish = treeish_argument(commit, directory)?;
    let (activity, bytes) = run(
        executable,
        repository,
        vec![
            ProcessArgument::visible("ls-tree"),
            ProcessArgument::visible("-z"),
            ProcessArgument::visible("-l"),
            ProcessArgument::visible(treeish),
        ],
        executor,
        READ_OUTPUT_LIMIT,
    )?;
    Ok(CommitTreeSnapshot {
        entries: parse_commit_tree(&bytes, directory)?,
        activities: vec![activity],
    })
}
/// Parses record-separated range history.
///
/// # Errors
/// Rejects malformed commit identities, timestamps, and framing.
pub fn parse_range_history(bytes: &[u8]) -> Result<Vec<RangeHistoryEntry>, RichHistoryError> {
    crate::history::tree::parse_range_history(bytes)
        .map(|entries| {
            entries
                .into_iter()
                .map(|entry| RangeHistoryEntry {
                    commit: entry.commit,
                    author: entry.author,
                    author_time_unix: entry.author_time_unix,
                    summary: entry.summary,
                })
                .collect()
        })
        .map_err(history_parse_error)
}
/// Parses NUL-framed tree entries.
///
/// # Errors
/// Rejects malformed entry metadata and paths.
pub fn parse_commit_tree(
    bytes: &[u8],
    directory: Option<&DiffPath>,
) -> Result<Vec<CommitTreeEntry>, RichHistoryError> {
    crate::history::tree::parse_commit_tree(bytes, directory)
        .map(|entries| entries.into_iter().map(tree_entry).collect())
        .map_err(history_parse_error)
}
#[cfg(test)]
fn parse_commit_tree_page(
    bytes: &[u8],
    directory: Option<&DiffPath>,
    offset: usize,
    limit: usize,
) -> Result<(Vec<CommitTreeEntry>, bool), RichHistoryError> {
    crate::history::tree::parse_commit_tree_page(bytes, directory, offset, limit)
        .map(|(entries, more)| (entries.into_iter().map(tree_entry).collect(), more))
        .map_err(history_parse_error)
}
fn tree_entry(entry: crate::history::tree::CommitTreeEntry) -> CommitTreeEntry {
    use crate::history::tree::CommitTreeEntryKind as Kind;
    CommitTreeEntry {
        path: entry.path,
        object: entry.object,
        mode: entry.mode,
        size: entry.size,
        kind: match entry.kind {
            Kind::Directory => CommitTreeEntryKind::Directory,
            Kind::File => CommitTreeEntryKind::File,
            Kind::Symlink => CommitTreeEntryKind::Symlink,
            Kind::Submodule => CommitTreeEntryKind::Submodule,
        },
    }
}
fn history_parse_error(error: crate::history::ParseError) -> RichHistoryError {
    RichHistoryError::new(RichHistoryErrorKind::Parse, error.to_string())
}

fn bounded_stderr(stderr: &[u8]) -> String {
    let value = String::from_utf8_lossy(stderr);
    let value = value.trim();
    if value.is_empty() {
        "installed Git history command failed".to_owned()
    } else {
        value.chars().take(512).collect()
    }
}

#[cfg(unix)]
// The fallible signature is shared with non-Unix targets, where native path conversion can fail.
#[allow(clippy::unnecessary_wraps)]
fn path_argument(path: &DiffPath) -> Result<(Vec<u8>, OsString), RichHistoryError> {
    use std::os::unix::ffi::OsStringExt as _;
    Ok((
        path.as_bytes().to_vec(),
        OsString::from_vec(path.as_bytes().to_vec()),
    ))
}

#[cfg(not(unix))]
fn path_argument(path: &DiffPath) -> Result<(Vec<u8>, OsString), RichHistoryError> {
    let value = String::from_utf8(path.as_bytes().to_vec()).map_err(|_| {
        RichHistoryError::new(
            RichHistoryErrorKind::InvalidRequest,
            "this platform cannot represent the selected path for installed Git",
        )
    })?;
    Ok((value.as_bytes().to_vec(), OsString::from(value)))
}

fn range_argument(request: &RangeHistoryQuery) -> Result<OsString, RichHistoryError> {
    let (path_bytes, _) = path_argument(&request.path)?;
    let mut bytes = format!("-L{},{}:", request.first_line, request.last_line).into_bytes();
    bytes.extend_from_slice(&path_bytes);
    native_argument(bytes)
}

fn treeish_argument(
    commit: &ObjectId,
    directory: Option<&DiffPath>,
) -> Result<OsString, RichHistoryError> {
    let mut bytes = commit.as_str().as_bytes().to_vec();
    if let Some(directory) = directory {
        bytes.push(b':');
        bytes.extend_from_slice(directory.as_bytes());
    }
    native_argument(bytes)
}

#[cfg(unix)]
// The fallible signature is shared with non-Unix targets, where native argument conversion can fail.
#[allow(clippy::unnecessary_wraps)]
fn native_argument(bytes: Vec<u8>) -> Result<OsString, RichHistoryError> {
    use std::os::unix::ffi::OsStringExt as _;
    Ok(OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn native_argument(bytes: Vec<u8>) -> Result<OsString, RichHistoryError> {
    String::from_utf8(bytes).map(OsString::from).map_err(|_| {
        RichHistoryError::new(
            RichHistoryErrorKind::InvalidRequest,
            "this platform cannot represent the selected path for installed Git",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_parser_retains_only_the_requested_window() {
        let oid = "1".repeat(40);
        let input = (0..4)
            .flat_map(|index| format!("100644 blob {oid} 1\tf{index}\0").into_bytes())
            .collect::<Vec<_>>();
        let (entries, has_more) = parse_commit_tree_page(&input, None, 1, 2).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.path.as_bytes())
                .collect::<Vec<_>>(),
            vec![b"f1".as_slice(), b"f2".as_slice()]
        );
        assert!(has_more);
    }
}
