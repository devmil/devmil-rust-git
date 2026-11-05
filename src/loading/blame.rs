//! Typed blame loading and bounded contiguous attribution spans.
use super::CommandExecutor;
use crate::activity::{ActivityRecord, ActivityStatus};
use crate::diff::model::DiffPath;
use crate::process::{GitProcessCommand, ProcessArgument};
use crate::types::{ObjectId, RepositoryPath};
use std::ffi::OsString;
use std::fmt;
pub const BLAME_OUTPUT_LIMIT: usize = 64 * 1024 * 1024;
pub const MAX_BLAME_LINES: usize = 2_000_000;
pub const MAX_BLAME_SPANS: usize = 500_000;
pub const DEFAULT_BLAME_CACHE_ENTRIES: usize = 64;
pub const DEFAULT_BLAME_CACHE_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum BlameRevision {
    WorkingTree,
    Head,
    Commit(ObjectId),
}

impl BlameRevision {
    pub fn argument(&self) -> Option<&str> {
        match self {
            Self::WorkingTree => None,
            Self::Head => Some("HEAD"),
            Self::Commit(commit) => Some(commit.as_str()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BlameRange {
    pub first: u64,
    pub last: u64,
}

impl BlameRange {
    /// # Errors
    ///
    /// Refuses zero, reversed, and excessively large line ranges.
    pub fn new(first: u64, last: u64) -> Result<Self, BlameError> {
        if first == 0 || first > last || last - first >= MAX_BLAME_LINES as u64 {
            return Err(BlameError::new(
                BlameErrorKind::InvalidRequest,
                "blame range is invalid or exceeds its bound",
            ));
        }
        Ok(Self { first, last })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct BlameOptions {
    pub detect_moves: bool,
    pub detect_copies: bool,
    pub range: Option<BlameRange>,
}

#[derive(Clone, Debug)]
pub struct BlameQuery {
    pub repository: RepositoryPath,
    pub revision: BlameRevision,
    pub path: DiffPath,
    pub options: BlameOptions,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlameAttribution {
    pub commit: Option<ObjectId>,
    pub author: Vec<u8>,
    pub author_mail: Vec<u8>,
    pub author_time_unix: i64,
    pub summary: Vec<u8>,
    pub original_path: DiffPath,
    pub boundary: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlameLine {
    pub final_line: u64,
    pub original_line: u64,
    pub content: Vec<u8>,
    pub attribution: BlameAttribution,
}

/// A stable contiguous attribution span. UI models expose these spans instead of one `QObject` per
/// source line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlameSpan {
    pub first_line: u64,
    pub last_line: u64,
    pub attribution: BlameAttribution,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlameResult {
    pub lines: Vec<BlameLine>,
    pub spans: Vec<BlameSpan>,
    pub activities: Vec<ActivityRecord>,
}

impl BlameResult {
    /// Builds bounded stable spans from ordered line attributions.
    ///
    /// # Errors
    ///
    /// Refuses unordered, duplicate, or oversized line publications.
    pub fn new(lines: Vec<BlameLine>, activities: Vec<ActivityRecord>) -> Result<Self, BlameError> {
        if lines.len() > MAX_BLAME_LINES {
            return Err(BlameError::new(
                BlameErrorKind::TooLarge,
                "blame line count exceeds its configured bound",
            ));
        }
        let mut spans: Vec<BlameSpan> = Vec::new();
        let mut previous = None;
        for line in &lines {
            if previous.is_some_and(|previous| line.final_line != previous + 1) {
                return Err(BlameError::new(
                    BlameErrorKind::Parse,
                    "blame final line identities are not contiguous",
                ));
            }
            previous = Some(line.final_line);
            if let Some(span) = spans.last_mut() {
                if span.last_line + 1 == line.final_line && span.attribution == line.attribution {
                    span.last_line = line.final_line;
                    continue;
                }
            }
            spans.push(BlameSpan {
                first_line: line.final_line,
                last_line: line.final_line,
                attribution: line.attribution.clone(),
            });
            if spans.len() > MAX_BLAME_SPANS {
                return Err(BlameError::new(
                    BlameErrorKind::TooLarge,
                    "blame span count exceeds its configured bound",
                ));
            }
        }
        Ok(Self {
            lines,
            spans,
            activities,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlameError {
    kind: BlameErrorKind,
    detail: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlameErrorKind {
    InvalidRequest,
    GitFailed,
    Canceled,
    Parse,
    TooLarge,
}

impl BlameError {
    #[must_use]
    pub fn new(kind: BlameErrorKind, detail: impl Into<String>) -> Self {
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

    pub const fn kind(&self) -> BlameErrorKind {
        self.kind
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for BlameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for BlameError {}

/// Loads bounded line attribution from Git.
/// # Panics
/// Only if a constant environment name becomes invalid.
/// # Errors
/// Rejects invalid ranges, failed commands and malformed output.
pub fn load(
    executable: &std::ffi::OsStr,
    request: &BlameQuery,
    executor: &mut dyn CommandExecutor,
) -> Result<BlameResult, BlameError> {
    if let Some(range) = request.options.range {
        BlameRange::new(range.first, range.last)?;
    }
    let mut arguments = vec![
        ProcessArgument::visible("blame"),
        ProcessArgument::visible("--line-porcelain"),
    ];
    if request.options.detect_moves {
        arguments.push(ProcessArgument::visible("-M"));
    }
    if request.options.detect_copies {
        arguments.push(ProcessArgument::visible("-C"));
    }
    if let Some(range) = request.options.range {
        arguments.extend([
            ProcessArgument::visible("-L"),
            ProcessArgument::visible(format!("{},{}", range.first, range.last)),
        ]);
    }
    if let Some(revision) = request.revision.argument() {
        arguments.push(ProcessArgument::visible(revision));
    }
    arguments.extend([
        ProcessArgument::visible("--"),
        ProcessArgument::visible(diff_path_to_os_string(&request.path)?),
    ]);
    let command = GitProcessCommand::git(executable.to_os_string())
        .current_dir(request.repository.as_path())
        .args(arguments)
        .output_limit(BLAME_OUTPUT_LIMIT)
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
            let lines = parse_line_porcelain(&stdout)?;
            BlameResult::new(lines, vec![activity])
        }
        ActivityStatus::Canceled { .. } => Err(BlameError::new(
            BlameErrorKind::Canceled,
            "blame was canceled",
        )),
        ActivityStatus::Succeeded { .. } => Err(BlameError::new(
            BlameErrorKind::TooLarge,
            "blame output exceeds its configured bound",
        )),
        ActivityStatus::LaunchError { message } => Err(BlameError::new(
            BlameErrorKind::GitFailed,
            format!("Git could not be launched: {message}"),
        )),
        _ => Err(BlameError::new(
            BlameErrorKind::GitFailed,
            bounded_stderr(&stderr),
        )),
    }
}

fn parse_line_porcelain(bytes: &[u8]) -> Result<Vec<BlameLine>, BlameError> {
    crate::history::blame::parse_line_porcelain(bytes)
        .map(|lines| {
            lines
                .into_iter()
                .map(|line| BlameLine {
                    final_line: line.final_line,
                    original_line: line.original_line,
                    content: line.content,
                    attribution: BlameAttribution {
                        commit: line.attribution.commit,
                        author: line.attribution.author,
                        author_mail: line.attribution.author_mail,
                        author_time_unix: line.attribution.author_time_unix,
                        summary: line.attribution.summary,
                        original_path: line.attribution.original_path,
                        boundary: line.attribution.boundary,
                    },
                })
                .collect()
        })
        .map_err(|error| BlameError::new(BlameErrorKind::Parse, error.to_string()))
}

fn bounded_stderr(stderr: &[u8]) -> String {
    let detail = String::from_utf8_lossy(stderr);
    let detail = detail.trim();
    if detail.is_empty() {
        "Git blame failed".to_owned()
    } else {
        detail.to_owned()
    }
}

#[cfg(unix)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the cross-platform path conversion has one fallible signature"
)]
fn diff_path_to_os_string(path: &DiffPath) -> Result<OsString, BlameError> {
    use std::os::unix::ffi::OsStringExt;
    Ok(OsString::from_vec(path.as_bytes().to_vec()))
}

#[cfg(not(unix))]
fn diff_path_to_os_string(path: &DiffPath) -> Result<OsString, BlameError> {
    String::from_utf8(path.as_bytes().to_vec())
        .map(OsString::from)
        .map_err(|_| {
            BlameError::new(
                BlameErrorKind::InvalidRequest,
                "non-UTF-8 blame paths are unsupported on this platform",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_boundary_uncommitted_and_quoted_paths_into_spans() {
        let oid = "1111111111111111111111111111111111111111";
        let output = format!(
            "{oid} 4 1 2\nboundary\nauthor A\nauthor-mail <a@example.invalid>\nauthor-time 4\nsummary base\nfilename \"old\\tpath.rs\"\n\tfirst\n{oid} 5 2\nboundary\nauthor A\nauthor-mail <a@example.invalid>\nauthor-time 4\nsummary base\nfilename \"old\\tpath.rs\"\n\tsecond\n{} 3 3\nauthor Not Committed Yet\nauthor-mail <not.committed.yet>\nauthor-time 0\nsummary Version of file.rs from file.rs\nfilename file.rs\n\tlocal\n",
            "0".repeat(40)
        );
        let lines = parse_line_porcelain(output.as_bytes()).unwrap();
        let result = BlameResult::new(lines, Vec::new()).unwrap();
        assert_eq!(result.lines.len(), 3);
        assert_eq!(result.spans.len(), 2);
        assert_eq!(result.spans[0].first_line, 1);
        assert_eq!(result.spans[0].last_line, 2);
        assert_eq!(
            result.spans[0].attribution.original_path.as_bytes(),
            b"old\tpath.rs"
        );
        assert!(result.spans[0].attribution.boundary);
        assert!(result.spans[1].attribution.commit.is_none());
    }
}
