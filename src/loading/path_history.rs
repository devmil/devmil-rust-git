//! Bounded file and directory history queries, independent of UI generations.
use super::CommandExecutor;
use crate::activity::{ActivityRecord, ActivityStatus};
use crate::diff::model::DiffPath;
use crate::process::{GitProcessCommand, ProcessArgument};
use crate::types::{ObjectId, RepositoryPath};
#[cfg(unix)]
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fmt;
pub const PATH_HISTORY_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
pub const DEFAULT_PATH_HISTORY_PAGE_SIZE: usize = 100;
pub const MAX_PATH_HISTORY_PAGE_SIZE: usize = 500;
pub const MAX_PATH_HISTORY_RENAME_HOPS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathHistoryKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathHistoryQuery {
    pub repository: RepositoryPath,
    pub revision: Option<ObjectId>,
    pub path: DiffPath,
    pub kind: PathHistoryKind,
    pub offset: usize,
    pub limit: usize,
    /// Rename transitions accepted by earlier pages of this same traversal.
    pub rename_hops_before: usize,
}

impl PathHistoryQuery {
    /// # Errors
    ///
    /// Refuses an empty or oversized page.
    pub fn validate(&self) -> Result<(), PathHistoryError> {
        if self.limit == 0 || self.limit > MAX_PATH_HISTORY_PAGE_SIZE {
            return Err(PathHistoryError::new(
                PathHistoryErrorKind::InvalidRequest,
                "path history page size is outside its configured bound",
            ));
        }
        if self.rename_hops_before > MAX_PATH_HISTORY_RENAME_HOPS {
            return Err(PathHistoryError::new(
                PathHistoryErrorKind::InvalidRequest,
                "path history rename traversal is beyond its configured bound",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathHistoryEntry {
    pub commit: ObjectId,
    pub author: Vec<u8>,
    pub author_time_unix: i64,
    pub summary: Vec<u8>,
    pub path: DiffPath,
    pub old_path: Option<DiffPath>,
}

impl PathHistoryEntry {
    #[must_use]
    pub fn stable_identity(&self) -> String {
        format!("path-history:{}", self.commit.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathHistoryPage {
    pub entries: Vec<PathHistoryEntry>,
    pub next_offset: Option<usize>,
    pub rename_hops: usize,
    pub incomplete_reason: Option<String>,
    pub activities: Vec<ActivityRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathHistoryError {
    kind: PathHistoryErrorKind,
    detail: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathHistoryErrorKind {
    InvalidRequest,
    GitFailed,
    Canceled,
    Parse,
    TooLarge,
}

impl PathHistoryError {
    #[must_use]
    pub fn new(kind: PathHistoryErrorKind, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        let detail = if detail.len() <= 512 {
            detail
        } else {
            let mut boundary = 512;
            while !detail.is_char_boundary(boundary) {
                boundary -= 1;
            }
            detail[..boundary].to_owned()
        };
        Self { kind, detail }
    }

    pub const fn kind(&self) -> PathHistoryErrorKind {
        self.kind
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for PathHistoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for PathHistoryError {}

/// Loads a bounded page with cross-page rename traversal accounting.
/// # Panics
/// Only if a constant environment name becomes invalid.
/// # Errors
/// Rejects invalid bounds, failed commands, and malformed output.
pub fn load(
    executable: &std::ffi::OsStr,
    request: &PathHistoryQuery,
    executor: &mut dyn CommandExecutor,
) -> Result<PathHistoryPage, PathHistoryError> {
    request.validate()?;
    let mut arguments = vec![
        ProcessArgument::visible("log"),
        ProcessArgument::visible("--format=%x1e%H%x00%an%x00%at%x00%s%x00"),
        ProcessArgument::visible("--name-status"),
        ProcessArgument::visible("-z"),
        ProcessArgument::visible("--find-renames"),
        ProcessArgument::visible(format!("--max-count={}", request.limit)),
        ProcessArgument::visible(format!("--skip={}", request.offset)),
    ];
    if request.kind == PathHistoryKind::File {
        arguments.push(ProcessArgument::visible("--follow"));
    }
    if let Some(revision) = &request.revision {
        arguments.push(ProcessArgument::visible(revision.as_str()));
    }
    arguments.extend([
        ProcessArgument::visible("--"),
        ProcessArgument::visible(diff_path_to_os_string(&request.path)?),
    ]);
    let command = GitProcessCommand::git(executable.to_os_string())
        .current_dir(request.repository.as_path())
        .args(arguments)
        .output_limit(PATH_HISTORY_OUTPUT_LIMIT)
        .env("LC_ALL", "C")
        .expect("constant environment name is valid")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .expect("constant environment name is valid")
        .redact_output();
    let mut run = executor.execute(command);
    let activity = run.activity;
    let truncated = run.stdout.truncated;
    let stderr = run.stderr.take_bytes();
    let stdout = run.stdout.take_bytes();
    match &activity.status {
        ActivityStatus::Succeeded { .. } if !truncated => {
            let mut entries = parse_path_history(&stdout, &request.path)?;
            let (rename_hops, bounded_by_rename) =
                bound_rename_traversal(&mut entries, request.rename_hops_before);
            let next_offset = (!bounded_by_rename && entries.len() == request.limit)
                .then_some(request.offset.saturating_add(entries.len()));
            Ok(PathHistoryPage {
                entries,
                next_offset,
                rename_hops: request.rename_hops_before.saturating_add(rename_hops),
                incomplete_reason: bounded_by_rename.then(|| {
                    format!(
                        "Rename traversal stopped after {MAX_PATH_HISTORY_RENAME_HOPS} transitions"
                    )
                }),
                activities: vec![activity],
            })
        }
        ActivityStatus::Canceled { .. } => Err(PathHistoryError::new(
            PathHistoryErrorKind::Canceled,
            "path history was canceled",
        )),
        ActivityStatus::Succeeded { .. } => Err(PathHistoryError::new(
            PathHistoryErrorKind::TooLarge,
            "path history output exceeds its configured bound",
        )),
        ActivityStatus::LaunchError { message } => Err(PathHistoryError::new(
            PathHistoryErrorKind::GitFailed,
            format!("Git could not be launched: {message}"),
        )),
        _ => Err(PathHistoryError::new(
            PathHistoryErrorKind::GitFailed,
            bounded_stderr(&stderr),
        )),
    }
}

fn bound_rename_traversal(
    entries: &mut Vec<PathHistoryEntry>,
    rename_hops_before: usize,
) -> (usize, bool) {
    let remaining_hops = MAX_PATH_HISTORY_RENAME_HOPS.saturating_sub(rename_hops_before);
    let mut rename_hops = 0usize;
    let mut cutoff = entries.len();
    for (index, entry) in entries.iter().enumerate() {
        if entry.old_path.is_some() {
            if rename_hops == remaining_hops {
                cutoff = index;
                break;
            }
            rename_hops += 1;
        }
    }
    let bounded = cutoff < entries.len();
    entries.truncate(cutoff);
    (rename_hops, bounded)
}

/// Parses the private NUL-delimited log format emitted by this adapter.
///
/// # Errors
///
/// Refuses malformed commit identities, timestamps, and byte paths.
pub fn parse_path_history(
    bytes: &[u8],
    requested_path: &DiffPath,
) -> Result<Vec<PathHistoryEntry>, PathHistoryError> {
    crate::history::path_history::parse_path_history(bytes, requested_path)
        .map(|entries| {
            entries
                .into_iter()
                .map(|entry| PathHistoryEntry {
                    commit: entry.commit,
                    author: entry.author,
                    author_time_unix: entry.author_time_unix,
                    summary: entry.summary,
                    path: entry.path,
                    old_path: entry.old_path,
                })
                .collect()
        })
        .map_err(|error| PathHistoryError::new(PathHistoryErrorKind::Parse, error.to_string()))
}

#[cfg(unix)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the cross-platform path conversion has one fallible signature"
)]
fn diff_path_to_os_string(path: &DiffPath) -> Result<OsString, PathHistoryError> {
    use std::os::unix::ffi::OsStrExt;
    Ok(OsStr::from_bytes(path.as_bytes()).to_os_string())
}

#[cfg(not(unix))]
fn diff_path_to_os_string(path: &DiffPath) -> Result<OsString, PathHistoryError> {
    String::from_utf8(path.as_bytes().to_vec())
        .map(OsString::from)
        .map_err(|_| {
            PathHistoryError::new(
                PathHistoryErrorKind::InvalidRequest,
                "this platform cannot represent the path for installed Git",
            )
        })
}

fn bounded_stderr(stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        "Git path history failed".to_owned()
    } else {
        stderr.chars().take(512).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ObjectId;
    const RECORD_SEPARATOR: u8 = 0x1e;

    #[test]
    fn parses_rename_old_path_and_byte_safe_names() {
        let oid = b"1111111111111111111111111111111111111111";
        let mut bytes = vec![RECORD_SEPARATOR];
        bytes.extend_from_slice(oid);
        bytes.extend_from_slice(
            b"\x00Ada\x001700000000\x00rename\x00\x00R100\x00old\xff.rs\x00new\xff.rs\x00",
        );
        let requested = DiffPath::from_bytes(b"new\xff.rs".to_vec()).unwrap();
        let entries = parse_path_history(&bytes, &requested).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path.as_bytes(), b"new\xff.rs");
        assert_eq!(
            entries[0].old_path.as_ref().unwrap().as_bytes(),
            b"old\xff.rs"
        );
    }

    #[test]
    fn deeper_rename_walk_stops_at_the_explicit_cross_page_bound() {
        let path = |name: u8| DiffPath::from_bytes(vec![name]).unwrap();
        let mut entries = (0..3)
            .map(|index| PathHistoryEntry {
                commit: ObjectId::parse(format!("{index:040x}")).unwrap(),
                author: Vec::new(),
                author_time_unix: 0,
                summary: Vec::new(),
                path: path(b'n'),
                old_path: Some(path(b'o')),
            })
            .collect::<Vec<_>>();
        let (accepted, bounded) =
            bound_rename_traversal(&mut entries, MAX_PATH_HISTORY_RENAME_HOPS - 1);
        assert_eq!(accepted, 1);
        assert_eq!(entries.len(), 1);
        assert!(bounded);
    }
}
