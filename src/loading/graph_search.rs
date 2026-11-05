//! Frozen-topology file path search with one bounded Git command per request.
use super::CommandExecutor;
use crate::activity::{ActivityId, ActivityRecord, ActivityStatus};
use crate::cancellation::Cancellation;
use crate::history::{GraphParseError, RevListParser, MAX_REV_LIST_RECORD_BYTES};
use crate::process::{GitProcessCommand, ProcessArgument};
use crate::types::{ObjectId, RepositoryRelativePath, WorktreeId};
use std::ffi::OsString;
pub const MAX_PATH_SEARCH_ROOTS: usize = 16_384;
pub const MAX_GRAPH_QUERY_ERROR_BYTES: usize = 4096;
/// A path query keeps free-text display search separate from byte-safe selected paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GraphPathQuery {
    DisplaySubstring(String),
    ExactPath(RepositoryRelativePath),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphPathSearchFailureKind {
    Canceled,
    InvalidPath,
    InvalidTopology,
    ProcessFailed,
    MalformedOutput,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphPathSearchFailure {
    kind: GraphPathSearchFailureKind,
    message: String,
    activity_id: Option<ActivityId>,
}

impl GraphPathSearchFailure {
    pub fn new(kind: GraphPathSearchFailureKind, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > MAX_GRAPH_QUERY_ERROR_BYTES {
            let mut end = MAX_GRAPH_QUERY_ERROR_BYTES;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
        }
        Self {
            kind,
            message,
            activity_id: None,
        }
    }

    #[must_use]
    pub fn with_activity_id(mut self, activity_id: ActivityId) -> Self {
        self.activity_id = Some(activity_id);
        self
    }

    pub const fn kind(&self) -> GraphPathSearchFailureKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub const fn activity_id(&self) -> Option<ActivityId> {
        self.activity_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathSearchRequest {
    pub worktree: WorktreeId,
    pub topology_roots: Vec<ObjectId>,
    pub query: GraphPathQuery,
    pub maximum_matches: usize,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathSearchResult {
    pub worktree: WorktreeId,
    pub activity_id: ActivityId,
    pub matches: Vec<ObjectId>,
    pub truncated: bool,
}
pub const MAX_PATH_QUERY_COMMITS: usize = 100_000;
const MAX_OBJECT_ID_LINE_BYTES: usize = 65;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathSearchPlanError {
    InvalidLimit,
    InvalidTopology,
    EmptyDisplaySubstring,
}

/// Builds a shell-free exact-path history query.
///
/// `--literal-pathspecs` prevents wildcard, leading-colon, and magic path bytes from becoming Git
/// pathspec syntax. The explicit `--` keeps leading dashes out of option parsing. Git emits one full
/// object ID per line for [`PathMatchParser`]. One additional record is requested so the adapter can
/// report truncation without returning more than `maximum_matches` identities.
///
/// # Errors
///
/// Rejects zero or unbounded match limits before constructing a process argument vector.
pub fn literal_path_history_arguments(
    path: &RepositoryRelativePath,
    topology_roots: &[ObjectId],
    maximum_matches: usize,
) -> Result<Vec<OsString>, PathSearchPlanError> {
    let mut arguments =
        path_history_prefix("--literal-pathspecs", topology_roots, maximum_matches)?;
    arguments.push(OsString::from("--"));
    arguments.push(path.as_path().as_os_str().to_os_string());
    Ok(arguments)
}

/// Builds a frozen-topology query for the display substring entered by Ctrl+F.
///
/// # Errors
///
/// Rejects an empty substring, invalid match limit, or empty or oversized frozen root set.
pub fn display_substring_history_arguments(
    text: &str,
    topology_roots: &[ObjectId],
    maximum_matches: usize,
) -> Result<Vec<OsString>, PathSearchPlanError> {
    if text.is_empty() {
        return Err(PathSearchPlanError::EmptyDisplaySubstring);
    }
    let mut arguments = path_history_prefix("--glob-pathspecs", topology_roots, maximum_matches)?;
    arguments.push(OsString::from("--"));
    arguments.push(OsString::from(format!(
        ":(glob,top)**/*{}*",
        escape_git_glob(text)
    )));
    Ok(arguments)
}

fn path_history_prefix(
    pathspec_mode: &str,
    topology_roots: &[ObjectId],
    maximum_matches: usize,
) -> Result<Vec<OsString>, PathSearchPlanError> {
    if maximum_matches == 0 || maximum_matches > MAX_PATH_QUERY_COMMITS {
        return Err(PathSearchPlanError::InvalidLimit);
    }
    if topology_roots.is_empty() || topology_roots.len() > MAX_PATH_SEARCH_ROOTS {
        return Err(PathSearchPlanError::InvalidTopology);
    }
    let arguments = vec![
        OsString::from(pathspec_mode),
        OsString::from("rev-list"),
        OsString::from("--stdin"),
        OsString::from("--topo-order"),
        OsString::from(format!("--max-count={}", maximum_matches + 1)),
    ];
    Ok(arguments)
}

fn topology_roots_stdin(topology_roots: &[ObjectId]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(
        topology_roots
            .len()
            .saturating_mul(MAX_OBJECT_ID_LINE_BYTES),
    );
    for root in topology_roots {
        bytes.extend_from_slice(root.as_str().as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

fn escape_git_glob(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(character, '*' | '?' | '[' | ']' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

#[derive(Debug, Default)]
pub struct PathMatchParser {
    inner: RevListParser,
}

impl PathMatchParser {
    /// Appends one arbitrary stream chunk.
    ///
    /// # Errors
    ///
    /// Returns a typed graph parse error for malformed or oversized records.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<ObjectId>, GraphParseError> {
        path_match_ids(self.inner.push(chunk)?)
    }

    /// Finishes the stream, including a final record without a newline.
    ///
    /// # Errors
    ///
    /// Returns a typed graph parse error for a malformed retained record.
    pub fn finish(self) -> Result<Vec<ObjectId>, GraphParseError> {
        path_match_ids(self.inner.finish()?)
    }

    pub fn retained_partial_bytes(&self) -> usize {
        self.inner.retained_partial_bytes()
    }

    pub const fn maximum_record_bytes() -> usize {
        MAX_REV_LIST_RECORD_BYTES
    }
}

fn path_match_ids(
    records: Vec<crate::history::RevListRecord>,
) -> Result<Vec<ObjectId>, GraphParseError> {
    if let Some(record) = records.iter().find(|record| !record.parents.is_empty()) {
        return Err(GraphParseError::UnexpectedFieldCount {
            expected_multiple: 1,
            actual: record.parents.len().saturating_add(1),
        });
    }
    Ok(records.into_iter().map(|record| record.id).collect())
}

#[derive(Clone, Debug)]
pub struct PathSearchLoader {
    executable: OsString,
}
impl PathSearchLoader {
    pub fn new(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
        }
    }
    /// Loads matches from the caller's frozen roots.
    /// # Errors
    /// Returns typed validation, cancellation, process or parsing failures.
    pub fn load(
        &self,
        request: &PathSearchRequest,
        cancellation: &dyn Cancellation,
        executor: &mut dyn CommandExecutor,
    ) -> Result<PathSearchResult, GraphPathSearchFailure> {
        if cancellation.is_cancelled() {
            return Err(GraphPathSearchFailure::new(
                GraphPathSearchFailureKind::Canceled,
                "path search canceled",
            ));
        }
        let (activity, stdout, stdout_truncated) = self.run(request, executor)?;
        let activity_id = activity.id;
        match activity.status {
            ActivityStatus::Canceled { .. } => Err(GraphPathSearchFailure::new(
                GraphPathSearchFailureKind::Canceled,
                "path search canceled",
            )
            .with_activity_id(activity_id)),
            ActivityStatus::Succeeded { .. } if stdout_truncated => {
                Err(GraphPathSearchFailure::new(
                    GraphPathSearchFailureKind::MalformedOutput,
                    "Git path search output exceeded its bounded result limit",
                )
                .with_activity_id(activity_id))
            }
            ActivityStatus::Succeeded { .. } => {
                let mut parser = PathMatchParser::default();
                let mut matches = parser.push(&stdout).map_err(|_| {
                    GraphPathSearchFailure::new(
                        GraphPathSearchFailureKind::MalformedOutput,
                        "Git returned malformed path history",
                    )
                    .with_activity_id(activity_id)
                })?;
                matches.extend(parser.finish().map_err(|_| {
                    GraphPathSearchFailure::new(
                        GraphPathSearchFailureKind::MalformedOutput,
                        "Git returned malformed path history",
                    )
                    .with_activity_id(activity_id)
                })?);
                let truncated = matches.len() > request.maximum_matches;
                matches.truncate(request.maximum_matches);
                if cancellation.is_cancelled() {
                    return Err(GraphPathSearchFailure::new(
                        GraphPathSearchFailureKind::Canceled,
                        "path search canceled",
                    )
                    .with_activity_id(activity_id));
                }
                Ok(PathSearchResult {
                    worktree: request.worktree.clone(),
                    activity_id,
                    matches,
                    truncated,
                })
            }
            ActivityStatus::LaunchError { .. }
            | ActivityStatus::Failed { .. }
            | ActivityStatus::TimedOut { .. }
            | ActivityStatus::Running => Err(GraphPathSearchFailure::new(
                GraphPathSearchFailureKind::ProcessFailed,
                "Git path search failed",
            )
            .with_activity_id(activity_id)),
        }
    }
    fn run(
        &self,
        request: &PathSearchRequest,
        executor: &mut dyn CommandExecutor,
    ) -> Result<(ActivityRecord, Vec<u8>, bool), GraphPathSearchFailure> {
        let arguments = match &request.query {
            GraphPathQuery::DisplaySubstring(text) => display_substring_history_arguments(
                text,
                &request.topology_roots,
                request.maximum_matches,
            ),
            GraphPathQuery::ExactPath(path) => literal_path_history_arguments(
                path,
                &request.topology_roots,
                request.maximum_matches,
            ),
        }
        .map_err(|error| match error {
            PathSearchPlanError::InvalidTopology => GraphPathSearchFailure::new(
                GraphPathSearchFailureKind::InvalidTopology,
                "path search requires a bounded frozen topology",
            ),
            PathSearchPlanError::EmptyDisplaySubstring => GraphPathSearchFailure::new(
                GraphPathSearchFailureKind::InvalidPath,
                "path search substring must not be empty",
            ),
            PathSearchPlanError::InvalidLimit => GraphPathSearchFailure::new(
                GraphPathSearchFailureKind::Internal,
                "path search match limit is outside the supported range",
            ),
        })?;
        let output_limit = request
            .maximum_matches
            .saturating_add(1)
            .saturating_mul(MAX_OBJECT_ID_LINE_BYTES);
        let command = GitProcessCommand::git(self.executable.clone())
            .current_dir(request.worktree.worktree_root().as_path())
            .args(arguments.into_iter().map(ProcessArgument::visible))
            .stdin_bytes(topology_roots_stdin(&request.topology_roots))
            .output_limit(output_limit)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .expect("constant environment name is valid")
            .env("LC_ALL", "C")
            .expect("constant environment name is valid");
        let mut result = executor.execute(command);
        let stdout_truncated = result.stdout.truncated;
        let stdout = result.stdout.take_bytes();
        Ok((result.activity, stdout, stdout_truncated))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const A: &str = "0000000000000000000000000000000000000001";
    const B: &str = "0000000000000000000000000000000000000002";

    fn roots() -> Vec<ObjectId> {
        vec![ObjectId::parse(A).unwrap(), ObjectId::parse(B).unwrap()]
    }

    #[test]
    fn literal_plan_keeps_option_shaped_and_wildcard_paths_as_one_argument() {
        let path = RepositoryRelativePath::new("-src/[graph]*.rs").unwrap();
        let arguments = literal_path_history_arguments(&path, &roots(), 512).unwrap();
        assert_eq!(arguments[0], "--literal-pathspecs");
        assert_eq!(arguments[2], "--stdin");
        assert_eq!(arguments[4], "--max-count=513");
        assert_eq!(arguments[5], "--");
        assert_eq!(arguments[6], path.as_path());
        assert_eq!(arguments.len(), 7);
        assert!(!arguments.iter().any(|argument| argument == "--all"));
        assert_eq!(
            topology_roots_stdin(&roots()),
            format!("{A}\n{B}\n").as_bytes()
        );
    }

    #[cfg(unix)]
    #[test]
    fn literal_plan_preserves_non_utf8_native_path_bytes() {
        use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

        let path = RepositoryRelativePath::new(PathBuf::from(OsString::from_vec(
            b"src/non-utf8-\xff.rs".to_vec(),
        )))
        .unwrap();
        let arguments = literal_path_history_arguments(&path, &roots(), 1).unwrap();
        assert_eq!(arguments[6].as_os_str().as_bytes(), b"src/non-utf8-\xff.rs");
    }

    #[test]
    fn parser_is_chunk_invariant_and_bounded() {
        let bytes = format!("{A}\n{B}\n").into_bytes();
        for chunk_size in 1..=bytes.len() {
            let mut parser = PathMatchParser::default();
            let mut ids = Vec::new();
            for chunk in bytes.chunks(chunk_size) {
                ids.extend(parser.push(chunk).unwrap());
                assert!(parser.retained_partial_bytes() <= PathMatchParser::maximum_record_bytes());
            }
            ids.extend(parser.finish().unwrap());
            assert_eq!(
                ids,
                vec![ObjectId::parse(A).unwrap(), ObjectId::parse(B).unwrap()]
            );
        }
    }

    #[test]
    fn parser_rejects_unexpected_parent_fields() {
        let mut parser = PathMatchParser::default();
        assert_eq!(
            parser.push(format!("{A} {B}\n").as_bytes()),
            Err(GraphParseError::UnexpectedFieldCount {
                expected_multiple: 1,
                actual: 2,
            })
        );
    }

    #[test]
    fn invalid_limits_fail_before_process_planning() {
        let path = RepositoryRelativePath::new("src/lib.rs").unwrap();
        assert_eq!(
            literal_path_history_arguments(&path, &roots(), 0),
            Err(PathSearchPlanError::InvalidLimit)
        );
        assert_eq!(
            literal_path_history_arguments(&path, &roots(), MAX_PATH_QUERY_COMMITS + 1),
            Err(PathSearchPlanError::InvalidLimit)
        );
        assert_eq!(
            literal_path_history_arguments(&path, &[], 1),
            Err(PathSearchPlanError::InvalidTopology)
        );
    }

    #[test]
    fn display_substring_escapes_pathspec_magic_and_uses_frozen_roots() {
        let arguments = display_substring_history_arguments("[graph]*?.rs", &roots(), 10).unwrap();
        assert_eq!(arguments[0], "--glob-pathspecs");
        assert_eq!(arguments[2], "--stdin");
        assert_eq!(arguments[6], ":(glob,top)**/*\\[graph\\]\\*\\?.rs*");
        assert!(!arguments.iter().any(|argument| argument == "--all"));
    }
}
