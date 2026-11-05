//! Bounded diff-content search and read-only bisect discovery.
use super::CommandExecutor;
use crate::activity::{ActivityId, ActivityStatus};
use crate::cancellation::Cancellation;
use crate::process::{GitProcessCommand, ProcessArgument, SemanticTimeout};
use crate::types::{ObjectId, WorktreeId};
use std::{cell::RefCell, ffi::OsString, fmt, time::Duration};
const SEARCH_OUTPUT_LIMIT: usize = 2 * 1024 * 1024;
const MAX_BISECT_TIMELINE_ENTRIES: usize = 4096;
pub const MAX_DIFF_SEARCH_QUERY_BYTES: usize = 1_024;
pub const MAX_DIFF_SEARCH_COMMITS: usize = 10_000;
pub const MAX_DIFF_SEARCH_RESULTS: usize = 512;
const MAX_DEEP_GRAPH_DETAIL_BYTES: usize = 512;

#[derive(Clone, Debug)]
pub struct DiffContentSearchRequest {
    pub repository: WorktreeId,
    pub query: String,
    pub maximum_commits: usize,
    pub maximum_results: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DiffContentSearchScope {
    /// Every reference and `HEAD`, like `git log --all`.
    #[default]
    AllRefs,
    CurrentBranch,
    /// `HEAD` and the references in
    /// [`STANDARD_REFERENCE_PATTERNS`](crate::refs::STANDARD_REFERENCE_PATTERNS), matching
    /// [`GraphTraversalScope::StandardReferences`](crate::loading::graph::GraphTraversalScope::StandardReferences).
    StandardRefs,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DiffContentSearchOptions {
    pub case_sensitive: bool,
    pub scope: DiffContentSearchScope,
}

impl DiffContentSearchRequest {
    /// Validates query and scan/result bounds before Git is launched.
    ///
    /// # Errors
    ///
    /// Returns a typed invalid-request error for empty or oversized values.
    pub fn validate(&self) -> Result<(), DeepGraphError> {
        if self.query.is_empty() || self.query.len() > MAX_DIFF_SEARCH_QUERY_BYTES {
            return Err(DeepGraphError::new(
                DeepGraphErrorKind::InvalidRequest,
                "diff-content search text is empty or exceeds its byte bound",
            ));
        }
        if self.maximum_commits == 0
            || self.maximum_commits > MAX_DIFF_SEARCH_COMMITS
            || self.maximum_results == 0
            || self.maximum_results > MAX_DIFF_SEARCH_RESULTS
        {
            return Err(DeepGraphError::new(
                DeepGraphErrorKind::InvalidRequest,
                "diff-content search limits are outside the production bounds",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchCompleteness {
    Complete,
    CommitLimitReached,
    ResultLimitReached,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffContentSearchMatch {
    pub commit: ObjectId,
    pub summary: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffContentSearchResult {
    pub matches: Vec<DiffContentSearchMatch>,
    pub examined_commits: usize,
    pub completeness: SearchCompleteness,
    pub activities: Vec<ActivityId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeepGraphErrorKind {
    InvalidRequest,
    Launch,
    Git,
    MalformedOutput,
    ResourceLimit,
    Canceled,
    NotBisecting,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeepGraphError {
    kind: DeepGraphErrorKind,
    detail: String,
}

impl DeepGraphError {
    #[must_use]
    pub fn new(kind: DeepGraphErrorKind, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        if detail.len() > MAX_DEEP_GRAPH_DETAIL_BYTES {
            let mut boundary = MAX_DEEP_GRAPH_DETAIL_BYTES;
            while !detail.is_char_boundary(boundary) {
                boundary -= 1;
            }
            detail.truncate(boundary);
        }
        Self { kind, detail }
    }

    #[must_use]
    pub const fn kind(&self) -> DeepGraphErrorKind {
        self.kind
    }

    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for DeepGraphError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for DeepGraphError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BisectMark {
    Good,
    Bad,
    Skip,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BisectTimelineEntry {
    pub revision: ObjectId,
    pub mark: BisectMark,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BisectState {
    pub active: bool,
    pub current: Option<ObjectId>,
    pub original_head: Option<ObjectId>,
    pub timeline: Vec<BisectTimelineEntry>,
    pub remaining_steps: Option<usize>,
    pub next_step_summary: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DeepGraphLoader {
    executable: OsString,
}
impl DeepGraphLoader {
    pub fn new(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
        }
    }
    /// Searches a bounded frozen set of commits for literal diff content.
    /// # Errors
    /// Returns validation, process, parse, resource or cancellation errors.
    pub fn search(
        &self,
        request: &DiffContentSearchRequest,
        cancellation: &dyn Cancellation,
        executor: &mut dyn CommandExecutor,
    ) -> Result<DiffContentSearchResult, DeepGraphError> {
        self.search_with_options(
            request,
            DiffContentSearchOptions::default(),
            cancellation,
            executor,
        )
    }

    /// Searches with explicit case and revision scope policy.
    /// # Errors
    /// Returns validation, process, parse, resource or cancellation errors.
    pub fn search_with_options(
        &self,
        request: &DiffContentSearchRequest,
        options: DiffContentSearchOptions,
        cancellation: &dyn Cancellation,
        executor: &mut dyn CommandExecutor,
    ) -> Result<DiffContentSearchResult, DeepGraphError> {
        Session {
            executable: self.executable.clone(),
            executor: RefCell::new(executor),
        }
        .search(request, options, cancellation)
    }
    /// Reads durable bisect state without modifying the repository.
    /// # Errors
    /// Returns process, malformed-state, resource or cancellation errors.
    pub fn discover(
        &self,
        repository: &WorktreeId,
        cancellation: &dyn Cancellation,
        executor: &mut dyn CommandExecutor,
    ) -> Result<BisectState, DeepGraphError> {
        if cancellation.is_cancelled() {
            return Err(DeepGraphError::new(
                DeepGraphErrorKind::Canceled,
                "bisect discovery was canceled",
            ));
        }
        Session {
            executable: self.executable.clone(),
            executor: RefCell::new(executor),
        }
        .discover(repository, cancellation)
    }
}
struct Session<'a> {
    executable: OsString,
    executor: RefCell<&'a mut dyn CommandExecutor>,
}
impl Session<'_> {
    fn private(
        &self,
        repository: &WorktreeId,
        arguments: Vec<ProcessArgument>,
        stdin: Vec<u8>,
        redact_output: bool,
        _cancellation: &dyn Cancellation,
    ) -> (ActivityId, ActivityStatus, Vec<u8>, bool) {
        let mut command = GitProcessCommand::git(&self.executable)
            .current_dir(repository.worktree_root().as_path())
            .args(arguments)
            .stdin_bytes(stdin)
            .output_limit(SEARCH_OUTPUT_LIMIT)
            .semantic_timeout(
                SemanticTimeout::new(Duration::from_secs(30), "deep graph query")
                    .expect("constant timeout is valid"),
            );
        if redact_output {
            command = command.redact_output();
        }
        let mut result = self.executor.borrow_mut().execute(command);
        let id = result.activity.id;
        let truncated = result.stdout.truncated || result.stderr.truncated;
        (
            id,
            result.activity.status,
            result.stdout.take_bytes(),
            truncated,
        )
    }

    fn resolve(
        &self,
        repository: &WorktreeId,
        revision: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<ObjectId>, DeepGraphError> {
        let (id, status, stdout, truncated) = self.private(
            repository,
            vec![
                ProcessArgument::visible("rev-parse"),
                ProcessArgument::visible("--verify"),
                ProcessArgument::visible("--quiet"),
                ProcessArgument::visible(format!("{revision}^{{commit}}")),
            ],
            Vec::new(),
            false,
            cancellation,
        );
        if truncated {
            return Err(resource_limit("Git output exceeded the deep graph bound"));
        }
        match status {
            ActivityStatus::Succeeded { .. } => parse_one_oid(&stdout).map(Some),
            ActivityStatus::Failed { .. } => Ok(None),
            ActivityStatus::Canceled { .. } | ActivityStatus::TimedOut { .. } => {
                Err(DeepGraphError::new(
                    DeepGraphErrorKind::Canceled,
                    "deep graph query was canceled",
                ))
            }
            ActivityStatus::LaunchError { .. } => Err(DeepGraphError::new(
                DeepGraphErrorKind::Launch,
                "installed Git could not start deep graph query",
            )),
            ActivityStatus::Running => Err(DeepGraphError::new(
                DeepGraphErrorKind::Git,
                format!("deep graph activity {} did not finish", id.get()),
            )),
        }
    }
    fn search(
        &self,
        request: &DiffContentSearchRequest,
        options: DiffContentSearchOptions,
        cancellation: &dyn Cancellation,
    ) -> Result<DiffContentSearchResult, DeepGraphError> {
        request.validate()?;
        if cancellation.is_cancelled() {
            return Err(DeepGraphError::new(
                DeepGraphErrorKind::Canceled,
                "diff-content search was canceled",
            ));
        }
        let mut enumeration = vec![
            ProcessArgument::visible("rev-list"),
            ProcessArgument::visible("--topo-order"),
            ProcessArgument::visible(format!("--max-count={}", request.maximum_commits + 1)),
        ];
        match options.scope {
            DiffContentSearchScope::AllRefs => enumeration.push(ProcessArgument::visible("--all")),
            DiffContentSearchScope::CurrentBranch => {
                enumeration.push(ProcessArgument::visible("HEAD"));
            }
            DiffContentSearchScope::StandardRefs => {
                enumeration.extend(
                    // A glob with a bracket gets no implied `/*`, so the last one names
                    // exactly refs/stash. A missing stash matches nothing.
                    ["--branches", "--remotes", "--tags", "--glob=refs/stas[h]"]
                        .map(ProcessArgument::visible),
                );
                // A detached HEAD is not under any reference; an unborn one adds nothing.
                if let Some(head) = self.resolve(&request.repository, "HEAD", cancellation)? {
                    enumeration.push(ProcessArgument::visible(head.as_str()));
                }
            }
        }
        let (enumerate_id, status, revisions, truncated) = self.private(
            &request.repository,
            enumeration,
            Vec::new(),
            false,
            cancellation,
        );
        ensure_success(&status)?;
        if truncated {
            return Err(resource_limit(
                "commit enumeration exceeded the diff-content search output bound",
            ));
        }
        let mut commits = parse_oid_lines(&revisions)?;
        let commit_limited = commits.len() > request.maximum_commits;
        commits.truncate(request.maximum_commits);
        if cancellation.is_cancelled() {
            return Err(DeepGraphError::new(
                DeepGraphErrorKind::Canceled,
                "diff-content search was canceled",
            ));
        }
        let mut stdin = Vec::with_capacity(commits.len().saturating_mul(41));
        for commit in &commits {
            stdin.extend_from_slice(commit.as_str().as_bytes());
            stdin.push(b'\n');
        }
        let escaped = escape_extended_regex(&request.query);
        let mut search_arguments = vec![
            ProcessArgument::visible("log"),
            ProcessArgument::visible("--no-walk=unsorted"),
            ProcessArgument::visible("--stdin"),
            ProcessArgument::redacted(format!("-G{escaped}"), "-G[REDACTED]"),
        ];
        if !options.case_sensitive {
            search_arguments.push(ProcessArgument::visible("--regexp-ignore-case"));
        }
        search_arguments.extend([
            ProcessArgument::visible("--format=%H%x00%s%x00"),
            ProcessArgument::visible("--no-color"),
            ProcessArgument::visible("--"),
        ]);
        let (search_id, status, output, truncated) = self.private(
            &request.repository,
            search_arguments,
            stdin,
            true,
            cancellation,
        );
        ensure_success(&status)?;
        if truncated {
            return Err(resource_limit(
                "diff-content search results exceeded the output bound",
            ));
        }
        let mut matches = parse_diff_search_matches(&output)?;
        let result_limited = matches.len() > request.maximum_results;
        matches.truncate(request.maximum_results);
        let completeness = if result_limited {
            SearchCompleteness::ResultLimitReached
        } else if commit_limited {
            SearchCompleteness::CommitLimitReached
        } else {
            SearchCompleteness::Complete
        };
        Ok(DiffContentSearchResult {
            matches,
            examined_commits: commits.len(),
            completeness,
            activities: vec![enumerate_id, search_id],
        })
    }
    fn discover(
        &self,
        repository: &WorktreeId,
        cancellation: &dyn Cancellation,
    ) -> Result<BisectState, DeepGraphError> {
        let Some(_) = self.resolve(repository, "refs/bisect/bad", cancellation)? else {
            return Ok(BisectState {
                active: false,
                current: None,
                original_head: None,
                timeline: Vec::new(),
                remaining_steps: None,
                next_step_summary: None,
            });
        };
        let original_head = self.bisect_original_head(repository, cancellation)?;
        let current = self.resolve(repository, "HEAD", cancellation)?;
        let (_, status, output, truncated) = self.private(
            repository,
            vec![
                ProcessArgument::visible("bisect"),
                ProcessArgument::visible("log"),
            ],
            Vec::new(),
            false,
            cancellation,
        );
        ensure_success(&status)?;
        if truncated {
            return Err(resource_limit(
                "bisect discovery output exceeded the output bound",
            ));
        }
        Ok(BisectState {
            active: true,
            current,
            original_head: Some(original_head),
            timeline: parse_bisect_log(&output)?,
            remaining_steps: parse_remaining_steps(&output),
            next_step_summary: parse_next_step(&output),
        })
    }
    fn bisect_original_head(
        &self,
        repository: &WorktreeId,
        cancellation: &dyn Cancellation,
    ) -> Result<ObjectId, DeepGraphError> {
        if cancellation.is_cancelled() {
            return Err(DeepGraphError::new(
                DeepGraphErrorKind::Canceled,
                "bisect restart discovery was canceled",
            ));
        }
        let directory = cap_std::fs::Dir::open_ambient_dir(
            repository.git_dir().as_path(),
            cap_std::ambient_authority(),
        )
        .map_err(|_| malformed("active bisect Git directory could not be opened safely"))?;
        let bytes = read_bisect_start(&directory)
            .map_err(|_| malformed("active bisect has an unsafe BISECT_START state"))?;
        let value = std::str::from_utf8(&bytes)
            .map_err(|_| malformed("bisect start identity is not representable"))?
            .trim();
        if value.is_empty() {
            return Err(malformed("bisect start identity is empty"));
        }
        self.resolve(repository, value, cancellation)?
            .ok_or_else(|| malformed("bisect start identity no longer resolves to a commit"))
    }
}
fn ensure_success(status: &ActivityStatus) -> Result<(), DeepGraphError> {
    match status {
        ActivityStatus::Succeeded { .. } => Ok(()),
        ActivityStatus::Canceled { .. } | ActivityStatus::TimedOut { .. } => {
            Err(DeepGraphError::new(
                DeepGraphErrorKind::Canceled,
                "deep graph query was canceled",
            ))
        }
        ActivityStatus::LaunchError { .. } => Err(DeepGraphError::new(
            DeepGraphErrorKind::Launch,
            "installed Git could not start deep graph query",
        )),
        ActivityStatus::Failed { .. } | ActivityStatus::Running => Err(DeepGraphError::new(
            DeepGraphErrorKind::Git,
            "installed Git rejected deep graph query",
        )),
    }
}

fn parse_one_oid(bytes: &[u8]) -> Result<ObjectId, DeepGraphError> {
    crate::history::search::parse_one_oid(bytes).map_err(|error| malformed(error.0))
}

fn parse_oid_lines(bytes: &[u8]) -> Result<Vec<ObjectId>, DeepGraphError> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(parse_one_oid)
        .collect()
}

fn parse_diff_search_matches(bytes: &[u8]) -> Result<Vec<DiffContentSearchMatch>, DeepGraphError> {
    crate::history::search::parse_diff_search_matches(bytes)
        .map(|matches| {
            matches
                .into_iter()
                .map(|entry| DiffContentSearchMatch {
                    commit: entry.commit,
                    summary: String::from_utf8_lossy(&entry.summary)
                        .chars()
                        .take(512)
                        .collect(),
                })
                .collect()
        })
        .map_err(|error| malformed(error.0))
}

fn escape_extended_regex(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(
            character,
            '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn parse_bisect_log(bytes: &[u8]) -> Result<Vec<BisectTimelineEntry>, DeepGraphError> {
    use crate::history::search::{BisectMark as Mark, BisectParseError};
    crate::history::search::parse_bisect_log(bytes, MAX_BISECT_TIMELINE_ENTRIES)
        .map(|entries| {
            entries
                .into_iter()
                .map(|entry| BisectTimelineEntry {
                    revision: entry.revision,
                    mark: match entry.mark {
                        Mark::Good => BisectMark::Good,
                        Mark::Bad => BisectMark::Bad,
                        Mark::Skip => BisectMark::Skip,
                    },
                })
                .collect()
        })
        .map_err(|error| match error {
            BisectParseError::Malformed(error) => malformed(error.0),
            BisectParseError::TooManyEntries => resource_limit(&error.to_string()),
        })
}

fn resource_limit(detail: &str) -> DeepGraphError {
    DeepGraphError::new(DeepGraphErrorKind::ResourceLimit, detail)
}

fn parse_remaining_steps(bytes: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(bytes).ok()?;
    text.lines().find_map(|line| {
        let (_, rest) = line.split_once("roughly ")?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

fn parse_next_step(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    text.lines()
        .rev()
        .find(|line| line.starts_with("# Bisecting:"))
        .map(|line| line.trim_start_matches("# ").chars().take(256).collect())
}

fn malformed(detail: &'static str) -> DeepGraphError {
    DeepGraphError::new(DeepGraphErrorKind::MalformedOutput, detail)
}

fn read_bisect_start(directory: &cap_std::fs::Dir) -> std::io::Result<Vec<u8>> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
    use std::io::Read;
    let path = std::path::Path::new("BISECT_START");
    let invalid =
        || std::io::Error::new(std::io::ErrorKind::InvalidData, "unsafe bisect state file");
    let before = directory.symlink_metadata(path)?;
    if !before.is_file() || before.len() > 1024 {
        return Err(invalid());
    }
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(
            i32::try_from(rustix::fs::OFlags::NONBLOCK.bits()).map_err(|_| invalid())?,
        );
    }
    let mut file = directory.open_with(path, &options)?;
    let opened = file.metadata()?;
    if !opened.is_file() || !crate::filesystem::same_cap_metadata(&before, &opened) {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    file.by_ref().take(1025).read_to_end(&mut bytes)?;
    if bytes.len() > 1024
        || !crate::filesystem::same_cap_metadata(&opened, &file.metadata()?)
        || !crate::filesystem::same_cap_metadata(&opened, &directory.symlink_metadata(path)?)
    {
        return Err(invalid());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_escape_makes_user_text_literal() {
        assert_eq!(escape_extended_regex("a.*[b]"), "a\\.\\*\\[b\\]");
    }

    #[test]
    fn bisect_log_parser_keeps_typed_marks() {
        let log = format!(
            "# bad: [{}] broken\n# good: [{}] okay\n# skip: [{}] unknown\n",
            "b".repeat(40),
            "a".repeat(40),
            "c".repeat(40)
        );
        let parsed = parse_bisect_log(log.as_bytes()).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[1].mark, BisectMark::Good);
    }

    #[test]
    fn bisect_log_parser_rejects_an_unbounded_timeline() {
        let entry = format!("# good: [{}] okay\n", "a".repeat(40));
        let log = entry.repeat(MAX_BISECT_TIMELINE_ENTRIES + 1);
        let error = parse_bisect_log(log.as_bytes()).unwrap_err();
        assert_eq!(error.kind(), DeepGraphErrorKind::ResourceLimit);
    }
}
