//! Bounded commit-detail, changed-file, and patch loading through recorded Git execution.
use super::CommandExecutor;
use crate::activity::{ActivityId, ActivityStatus};
use crate::cancellation::Cancellation;
use crate::diff::model::{
    ContextLines, DiffDocument, DiffLayout, DiffPath, DiffWhitespacePolicy, FileChangeKind,
    GitFileMode,
};
use crate::diff::{
    parse_raw_diff_z, parse_unified_patch_with_raw_metadata, PatchLimits, RawObjectId,
};
use crate::process::{GitProcessCommand, ProcessArgument};
use crate::types::{ObjectId, WorktreeId};
use std::ffi::OsString;
use std::fmt;

pub const MAX_COMMIT_MESSAGE_BYTES: usize = 1024 * 1024;
pub const MAX_COMMIT_IDENTITY_BYTES: usize = 4 * 1024;
pub const MAX_COMMIT_PARENTS: usize = 256;
pub const MAX_COMMIT_FILE_CHANGES: usize = 10_000;
pub const MAX_COMMIT_DIFF_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_HISTORY_READ_ERROR_BYTES: usize = 512;
pub const MAX_COMMIT_DETAIL_BATCH: usize = 256;
pub const MAX_COMMIT_DETAIL_BATCH_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum HistoryReadKind {
    Detail,
    Files,
    Diff,
}

/// Explicit parent policy for the selected commit. History topology supplies this value so the
/// loader never guesses which parent comparison the caller means.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommitDiffBase {
    EmptyTree,
    FirstParent(ObjectId),
}

/// Byte-safe scope for a selected-commit patch read.
///
/// A renamed or copied file retains both paths. Adapters must pass both paths to Git so rename
/// detection can preserve the file identity instead of degrading the result into an add/delete.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CommitDiffScope {
    All,
    File {
        old_path: DiffPath,
        new_path: DiffPath,
    },
}

/// Patch options included in the repository read identity and cache key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitDiffOptions {
    pub scope: CommitDiffScope,
    pub context: ContextLines,
    pub whitespace: DiffWhitespacePolicy,
    pub layout: DiffLayout,
    /// Diffs every file as text (`git diff --text`), even one Git considers binary.
    pub force_text: bool,
}

impl Default for CommitDiffOptions {
    fn default() -> Self {
        Self {
            scope: CommitDiffScope::All,
            context: ContextLines::DEFAULT,
            whitespace: DiffWhitespacePolicy::Preserve,
            layout: DiffLayout::Unified,
            force_text: false,
        }
    }
}

pub use crate::history::commit::CommitDetail;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitFileChange {
    pub old_mode: GitFileMode,
    pub new_mode: GitFileMode,
    pub old_object: Option<ObjectId>,
    pub new_object: Option<ObjectId>,
    pub kind: FileChangeKind,
    pub old_path: DiffPath,
    pub new_path: DiffPath,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HistoryReadValue {
    Detail(CommitDetail),
    Files(Vec<CommitFileChange>),
    Diff(DiffDocument),
}

impl HistoryReadValue {
    pub const fn kind(&self) -> HistoryReadKind {
        match self {
            Self::Detail(_) => HistoryReadKind::Detail,
            Self::Files(_) => HistoryReadKind::Files,
            Self::Diff(_) => HistoryReadKind::Diff,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryReadFailureKind {
    InvalidRequest,
    MissingObject,
    ProcessFailed,
    MalformedOutput,
    TooLarge,
    Canceled,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryReadFailure {
    pub kind: HistoryReadFailureKind,
    detail: String,
    activity_id: Option<ActivityId>,
}

impl HistoryReadFailure {
    pub fn new(kind: HistoryReadFailureKind, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        if detail.len() > MAX_HISTORY_READ_ERROR_BYTES {
            let mut boundary = MAX_HISTORY_READ_ERROR_BYTES;
            while !detail.is_char_boundary(boundary) {
                boundary -= 1;
            }
            detail.truncate(boundary);
        }
        Self {
            kind,
            detail,
            activity_id: None,
        }
    }

    #[must_use]
    pub const fn with_activity_id(mut self, activity_id: ActivityId) -> Self {
        self.activity_id = Some(activity_id);
        self
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }

    pub const fn activity_id(&self) -> Option<ActivityId> {
        self.activity_id
    }
}

impl fmt::Display for HistoryReadFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for HistoryReadFailure {}

/// An immutable read specification. Generation and publication policy belong to the caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitReadRequest {
    pub worktree: WorktreeId,
    pub commit: ObjectId,
    pub diff_base: CommitDiffBase,
    pub diff_scope: CommitDiffScope,
    pub kind: HistoryReadKind,
    pub context: ContextLines,
    pub whitespace: DiffWhitespacePolicy,
    pub layout: DiffLayout,
    /// Diffs every file as text (`git diff --text`), even one Git considers binary.
    pub force_text: bool,
}
impl CommitReadRequest {
    pub fn new(
        worktree: WorktreeId,
        commit: ObjectId,
        diff_base: CommitDiffBase,
        kind: HistoryReadKind,
    ) -> Self {
        Self {
            worktree,
            commit,
            diff_base,
            kind,
            diff_scope: CommitDiffScope::All,
            context: ContextLines::DEFAULT,
            whitespace: DiffWhitespacePolicy::Preserve,
            layout: DiffLayout::Unified,
            force_text: false,
        }
    }
    pub fn worktree(&self) -> &WorktreeId {
        &self.worktree
    }
    pub fn commit(&self) -> &ObjectId {
        &self.commit
    }
    pub fn diff_base(&self) -> &CommitDiffBase {
        &self.diff_base
    }
    pub fn diff_scope(&self) -> &CommitDiffScope {
        &self.diff_scope
    }
    pub const fn kind(&self) -> HistoryReadKind {
        self.kind
    }
    pub const fn diff_context(&self) -> ContextLines {
        self.context
    }
    pub const fn diff_whitespace(&self) -> DiffWhitespacePolicy {
        self.whitespace
    }
    pub const fn diff_layout(&self) -> DiffLayout {
        self.layout
    }
    pub const fn diff_force_text(&self) -> bool {
        self.force_text
    }
}

/// Selected-commit reads through a caller-supplied recorded command transport.
#[derive(Clone, Debug)]
pub struct CommitLoader {
    executable: OsString,
    patch_limits: PatchLimits,
}
impl CommitLoader {
    pub fn new(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
            patch_limits: PatchLimits::default(),
        }
    }
    #[must_use]
    pub fn with_patch_limits(mut self, limits: PatchLimits) -> Self {
        self.patch_limits = limits;
        self
    }

    /// Loads ordered commit details in one `cat-file --batch` invocation.
    /// Empty requests do not launch Git. Duplicate IDs retain their input positions.
    ///
    /// # Errors
    /// Rejects oversized batches, missing objects, cancellation, malformed frames, and output limits.
    pub fn load_details(
        &self,
        worktree: &WorktreeId,
        commits: &[ObjectId],
        executor: &mut dyn CommandExecutor,
        cancellation: Option<&dyn Cancellation>,
    ) -> Result<Vec<CommitDetail>, HistoryReadFailure> {
        let Some(first) = commits.first() else {
            return Ok(Vec::new());
        };
        if commits.len() > MAX_COMMIT_DETAIL_BATCH {
            return Err(HistoryReadFailure::new(
                HistoryReadFailureKind::TooLarge,
                "commit detail batch exceeds its item limit",
            ));
        }
        let request = CommitReadRequest::new(
            worktree.clone(),
            first.clone(),
            CommitDiffBase::EmptyTree,
            HistoryReadKind::Detail,
        );
        let mut input = Vec::with_capacity(commits.len() * 65);
        for commit in commits {
            input.extend_from_slice(commit.as_str().as_bytes());
            input.push(b'\n');
        }
        let mut session = ReadSession {
            executable: self.executable.clone(),
            patch_limits: self.patch_limits,
            executor,
            cancellation,
        };
        let (activity, bytes) = session.run(
            &request,
            ["cat-file".into(), "--batch".into()],
            MAX_COMMIT_DETAIL_BATCH_BYTES,
            Some(input),
        )?;
        parse_detail_batch(commits, &bytes).map_err(|failure| failure.with_activity_id(activity))
    }
    /// Loads one typed component, retaining the original bounded command strategy.
    ///
    /// # Errors
    /// Returns bounded cancellation, command, parse, missing-object, and resource-limit failures.
    pub fn load(
        &self,
        request: &CommitReadRequest,
        executor: &mut dyn CommandExecutor,
        cancellation: Option<&dyn Cancellation>,
    ) -> Result<HistoryReadValue, HistoryReadFailure> {
        let mut session = ReadSession {
            executable: self.executable.clone(),
            patch_limits: self.patch_limits,
            executor,
            cancellation,
        };
        match request.kind {
            HistoryReadKind::Detail => session.read_commit_detail(request),
            HistoryReadKind::Files => session.read_commit_files(request),
            HistoryReadKind::Diff => session.read_commit_diff(request),
        }
    }
}

fn parse_detail_batch(
    commits: &[ObjectId],
    mut bytes: &[u8],
) -> Result<Vec<CommitDetail>, HistoryReadFailure> {
    let mut result = Vec::with_capacity(commits.len());
    for commit in commits {
        let header_end = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(|| malformed("batch header"))?;
        let header = &bytes[..header_end];
        bytes = &bytes[header_end + 1..];
        let fields = header.split(|byte| *byte == b' ').collect::<Vec<_>>();
        if fields.len() == 2 && fields[1] == b"missing" {
            return Err(HistoryReadFailure::new(
                HistoryReadFailureKind::MissingObject,
                "selected commit is no longer available",
            ));
        }
        if fields.len() != 3 || fields[0] != commit.as_str().as_bytes() || fields[1] != b"commit" {
            return Err(malformed("batch object identity"));
        }
        let count = std::str::from_utf8(fields[2])
            .ok()
            .and_then(|size| size.parse::<usize>().ok())
            .ok_or_else(|| malformed("batch size"))?;
        if count > DETAIL_OUTPUT_LIMIT {
            return Err(HistoryReadFailure::new(
                HistoryReadFailureKind::TooLarge,
                "commit body exceeds its bounded result limit",
            ));
        }
        if bytes.get(count) != Some(&b'\n') {
            return Err(malformed("batch body boundary"));
        }
        result.push(parse_commit_detail(commit.clone(), &bytes[..count])?);
        bytes = &bytes[count + 1..];
    }
    if !bytes.is_empty() {
        return Err(malformed("batch trailing output"));
    }
    Ok(result)
}

const DETAIL_OUTPUT_LIMIT: usize = MAX_COMMIT_MESSAGE_BYTES + 64 * 1024;

const RAW_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const PATCH_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;

struct ReadSession<'a> {
    executable: OsString,
    patch_limits: PatchLimits,
    executor: &'a mut dyn CommandExecutor,
    cancellation: Option<&'a dyn Cancellation>,
}

impl ReadSession<'_> {
    fn run(
        &mut self,
        request: &CommitReadRequest,
        arguments: impl IntoIterator<Item = OsString>,
        output_limit: usize,
        stdin: Option<Vec<u8>>,
    ) -> Result<(ActivityId, Vec<u8>), HistoryReadFailure> {
        if self.cancellation.is_some_and(Cancellation::is_cancelled) {
            return Err(HistoryReadFailure::new(
                HistoryReadFailureKind::Canceled,
                "commit read canceled",
            ));
        }
        let mut command = GitProcessCommand::git(self.executable.clone())
            .current_dir(request.worktree().worktree_root().as_path())
            .args(arguments.into_iter().map(ProcessArgument::visible))
            .output_limit(output_limit)
            // Commit messages, author identities, paths, and patch content remain parser-private.
            .redact_output()
            .env("GIT_OPTIONAL_LOCKS", "0")
            .expect("constant environment name is valid")
            .env("LC_ALL", "C")
            .expect("constant environment name is valid");
        if let Some(stdin) = stdin {
            command = command.stdin_bytes(stdin);
        }
        let mut result = self.executor.execute(command);
        let id = result.activity.id;
        let truncated = result.stdout.truncated;
        let stdout = result.stdout.take_bytes();
        let stderr = result.stderr.take_bytes();
        match result.activity.status {
            ActivityStatus::Canceled { .. } => Err(HistoryReadFailure::new(
                HistoryReadFailureKind::Canceled,
                "commit read canceled",
            )
            .with_activity_id(id)),
            ActivityStatus::Succeeded { .. } if truncated => Err(HistoryReadFailure::new(
                HistoryReadFailureKind::TooLarge,
                "Git commit output exceeded its bounded result limit",
            )
            .with_activity_id(id)),
            ActivityStatus::Succeeded { .. } => Ok((id, stdout)),
            ActivityStatus::Failed { .. } if looks_like_missing_object(&stderr) => {
                Err(HistoryReadFailure::new(
                    HistoryReadFailureKind::MissingObject,
                    "selected commit is no longer available",
                )
                .with_activity_id(id))
            }
            ActivityStatus::LaunchError { .. }
            | ActivityStatus::Failed { .. }
            | ActivityStatus::TimedOut { .. }
            | ActivityStatus::Running => Err(HistoryReadFailure::new(
                HistoryReadFailureKind::ProcessFailed,
                "Git could not read the selected commit",
            )
            .with_activity_id(id)),
        }
    }
}

fn looks_like_missing_object(stderr: &[u8]) -> bool {
    let lower = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    lower.contains("bad object")
        || lower.contains("bad file")
        || lower.contains("unknown revision")
        || lower.contains("not a valid object")
        || lower.contains("could not parse object")
}

fn ensure_kind(
    request: &CommitReadRequest,
    expected: HistoryReadKind,
) -> Result<(), HistoryReadFailure> {
    if request.kind() == expected {
        Ok(())
    } else {
        Err(HistoryReadFailure::new(
            HistoryReadFailureKind::InvalidRequest,
            "commit read kind does not match the selected operation",
        ))
    }
}

fn commit_argument(request: &CommitReadRequest) -> OsString {
    OsString::from(request.commit().as_str())
}

fn raw_diff_arguments(request: &CommitReadRequest) -> Result<Vec<OsString>, HistoryReadFailure> {
    let arguments = match request.diff_base() {
        CommitDiffBase::EmptyTree => vec![
            "diff-tree".into(),
            "--root".into(),
            "--no-commit-id".into(),
            "-r".into(),
            "-M".into(),
            "--raw".into(),
            "-z".into(),
            "--no-abbrev".into(),
            commit_argument(request),
        ],
        CommitDiffBase::FirstParent(parent) => vec![
            "diff".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "-M".into(),
            "--raw".into(),
            "-z".into(),
            "--no-abbrev".into(),
            parent.as_str().into(),
            commit_argument(request),
        ],
    };
    apply_diff_scope(arguments, request.diff_scope())
}

fn patch_arguments(request: &CommitReadRequest) -> Result<Vec<OsString>, HistoryReadFailure> {
    // History displays binary summaries, not encoded apply-ready blob payloads.
    // Blob sizes are loaded separately without consuming the patch output budget.
    let mut arguments = match request.diff_base() {
        CommitDiffBase::EmptyTree => vec![
            "diff-tree".into(),
            "--root".into(),
            "--no-commit-id".into(),
            "-r".into(),
            "-M".into(),
            "--patch".into(),
            "--full-index".into(),
            "--no-color".into(),
            commit_argument(request),
        ],
        CommitDiffBase::FirstParent(parent) => vec![
            "diff".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "-M".into(),
            "--patch".into(),
            "--full-index".into(),
            "--no-color".into(),
            parent.as_str().into(),
            commit_argument(request),
        ],
    };
    arguments.insert(
        1,
        format!("--unified={}", request.diff_context().value()).into(),
    );
    let whitespace = match request.diff_whitespace() {
        DiffWhitespacePolicy::Preserve => None,
        DiffWhitespacePolicy::IgnoreAll => Some("--ignore-all-space"),
        DiffWhitespacePolicy::IgnoreAmount => Some("--ignore-space-change"),
        DiffWhitespacePolicy::IgnoreEndOfLine => Some("--ignore-space-at-eol"),
    };
    if let Some(argument) = whitespace {
        arguments.insert(1, argument.into());
    }
    if request.diff_force_text() {
        arguments.insert(1, "--text".into());
    }
    apply_diff_scope(arguments, request.diff_scope())
}

fn apply_diff_scope(
    mut arguments: Vec<OsString>,
    scope: &CommitDiffScope,
) -> Result<Vec<OsString>, HistoryReadFailure> {
    let CommitDiffScope::File { old_path, new_path } = scope else {
        arguments.push("--".into());
        return Ok(arguments);
    };
    arguments.insert(0, "--literal-pathspecs".into());
    arguments.push("--".into());
    arguments.push(diff_path_to_os_string(old_path)?);
    if old_path != new_path {
        arguments.push(diff_path_to_os_string(new_path)?);
    }
    Ok(arguments)
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn diff_path_to_os_string(path: &DiffPath) -> Result<OsString, HistoryReadFailure> {
    use std::os::unix::ffi::OsStringExt as _;
    Ok(OsString::from_vec(path.as_bytes().to_vec()))
}

#[cfg(not(unix))]
fn diff_path_to_os_string(path: &DiffPath) -> Result<OsString, HistoryReadFailure> {
    String::from_utf8(path.as_bytes().to_vec())
        .map(OsString::from)
        .map_err(|_| {
            HistoryReadFailure::new(
                HistoryReadFailureKind::InvalidRequest,
                "selected commit path cannot be represented on this platform",
            )
        })
}

impl ReadSession<'_> {
    fn read_commit_detail(
        &mut self,
        request: &CommitReadRequest,
    ) -> Result<HistoryReadValue, HistoryReadFailure> {
        let outcome = ensure_kind(request, HistoryReadKind::Detail).and_then(|()| {
            self.run(
                request,
                [
                    OsString::from("cat-file"),
                    OsString::from("commit"),
                    commit_argument(request),
                ],
                DETAIL_OUTPUT_LIMIT,
                None,
            )
            .and_then(|(activity, bytes)| {
                parse_commit_detail(request.commit().clone(), &bytes)
                    .map_err(|failure| failure.with_activity_id(activity))
            })
            .map(HistoryReadValue::Detail)
        });
        outcome
    }
}

impl ReadSession<'_> {
    fn read_commit_files(
        &mut self,
        request: &CommitReadRequest,
    ) -> Result<HistoryReadValue, HistoryReadFailure> {
        ensure_kind(request, HistoryReadKind::Files).and_then(|()| {
            let arguments = raw_diff_arguments(request)?;
            self.run(request, arguments, RAW_OUTPUT_LIMIT, None)
                .and_then(|(activity, bytes)| {
                    parse_file_changes(&bytes).map_err(|failure| failure.with_activity_id(activity))
                })
                .map(HistoryReadValue::Files)
        })
    }
}

impl ReadSession<'_> {
    fn read_commit_diff(
        &mut self,
        request: &CommitReadRequest,
    ) -> Result<HistoryReadValue, HistoryReadFailure> {
        ensure_kind(request, HistoryReadKind::Diff)?;
        let (_, raw) = self.run(
            request,
            raw_diff_arguments(request)?,
            RAW_OUTPUT_LIMIT,
            None,
        )?;
        let plan = super::diff_preview::PreviewPlan::inspect(
            &raw,
            request.worktree().worktree_root().as_path(),
            &self.executable,
            None,
            self.cancellation,
            &mut |command| self.executor.execute(command),
        )
        .map_err(|failure| map_diff_failure(&failure))?;
        let mut document = if plan.has_large_files() && plan.inline_paths().is_empty() {
            DiffDocument {
                layout: request.diff_layout(),
                files: Vec::new(),
            }
        } else {
            let mut arguments = patch_arguments(request)?;
            if plan.has_large_files() {
                plan.restrict(&mut arguments)
                    .map_err(|failure| map_diff_failure(&failure))?;
            }
            let (activity, patch) = self.run(request, arguments, PATCH_OUTPUT_LIMIT, None)?;
            let inline_raw = plan.inline_raw(&raw);
            let parse = if request.diff_whitespace() == DiffWhitespacePolicy::Preserve {
                parse_unified_patch_with_raw_metadata
            } else {
                crate::diff::parse_unified_patch_with_filtered_raw_metadata
            };
            parse(&inline_raw, &patch, self.patch_limits)
                .map_err(|failure| map_diff_failure(&failure).with_activity_id(activity))?
        };
        document = plan.merge(document);
        document.layout = request.diff_layout();
        validate_diff_scope(&document, request.diff_scope())?;
        Ok(HistoryReadValue::Diff(document))
    }
}

fn validate_diff_scope(
    document: &DiffDocument,
    scope: &CommitDiffScope,
) -> Result<(), HistoryReadFailure> {
    let CommitDiffScope::File { old_path, new_path } = scope else {
        return Ok(());
    };
    let [file] = document.files.as_slice() else {
        return Err(malformed("file-scoped diff result"));
    };
    let old_matches = file
        .id
        .old_path
        .as_ref()
        .is_none_or(|path| path == old_path);
    let new_matches = file
        .id
        .new_path
        .as_ref()
        .is_none_or(|path| path == new_path);
    let names_selected_file =
        file.id.old_path.as_ref() == Some(old_path) || file.id.new_path.as_ref() == Some(new_path);
    if old_matches && new_matches && names_selected_file {
        Ok(())
    } else {
        Err(malformed("file-scoped diff identity"))
    }
}

/// Parses a bounded commit body using this loader's default limits.
///
/// # Errors
/// Rejects malformed fields and exceeded bounds.
pub fn parse_commit_detail(
    commit: ObjectId,
    input: &[u8],
) -> Result<CommitDetail, HistoryReadFailure> {
    use crate::history::commit::{CommitLimits, CommitParseErrorKind};
    let detail = crate::history::commit::parse_commit_detail(
        commit,
        input,
        CommitLimits {
            message_bytes: MAX_COMMIT_MESSAGE_BYTES,
            identity_bytes: MAX_COMMIT_IDENTITY_BYTES,
            parents: MAX_COMMIT_PARENTS,
        },
    )
    .map_err(|error| {
        HistoryReadFailure::new(
            match error.kind {
                CommitParseErrorKind::Malformed => HistoryReadFailureKind::MalformedOutput,
                CommitParseErrorKind::TooLarge => HistoryReadFailureKind::TooLarge,
            },
            error.detail,
        )
    })?;
    Ok(CommitDetail {
        commit: detail.commit,
        tree: detail.tree,
        parents: detail.parents,
        author: detail.author,
        author_time: detail.author_time,
        author_timezone: detail.author_timezone,
        committer: detail.committer,
        committer_time: detail.committer_time,
        committer_timezone: detail.committer_timezone,
        message: detail.message,
    })
}

fn parse_file_changes(input: &[u8]) -> Result<Vec<CommitFileChange>, HistoryReadFailure> {
    let entries = parse_raw_diff_z(input).map_err(|failure| map_diff_failure(&failure))?;
    if entries.len() > MAX_COMMIT_FILE_CHANGES {
        return Err(HistoryReadFailure::new(
            HistoryReadFailureKind::TooLarge,
            "commit changed-file list exceeds its limit",
        ));
    }
    Ok(entries
        .into_iter()
        .map(|entry| CommitFileChange {
            old_mode: entry.old_mode,
            new_mode: entry.new_mode,
            old_object: raw_object(entry.old_object),
            new_object: raw_object(entry.new_object),
            kind: entry.kind,
            old_path: entry.old_path,
            new_path: entry.new_path,
        })
        .collect())
}

fn raw_object(object: RawObjectId) -> Option<ObjectId> {
    match object {
        RawObjectId::Object(object) => Some(object),
        RawObjectId::Null(_) => None,
    }
}

/// Parses raw-plus-patch output while preserving whitespace filtering semantics.
///
/// # Errors
/// Rejects malformed metadata or unexpected combined merge output.
pub fn parse_filtered_history_diff(
    combined: &[u8],
    limits: PatchLimits,
) -> Result<(DiffDocument, Vec<u8>), HistoryReadFailure> {
    use crate::diff::{split_combined_diff, CombinedDiff};
    match split_combined_diff(combined).map_err(|failure| map_diff_failure(&failure))? {
        CombinedDiff::Empty => Ok((
            DiffDocument {
                layout: DiffLayout::Unified,
                files: Vec::new(),
            },
            Vec::new(),
        )),
        CombinedDiff::RawOnly(raw) => {
            let trimmed = if raw.ends_with(b"\0\0") {
                &raw[..raw.len() - 1]
            } else {
                raw.as_slice()
            };
            parse_raw_diff_z(trimmed).map_err(|failure| map_diff_failure(&failure))?;
            Ok((
                DiffDocument {
                    layout: DiffLayout::Unified,
                    files: Vec::new(),
                },
                raw,
            ))
        }
        CombinedDiff::Standard { raw, patch } => {
            crate::diff::parse_unified_patch_with_filtered_raw_metadata(&raw, &patch, limits)
                .map_err(|failure| map_diff_failure(&failure))
                .map(|document| (document, raw))
        }
        CombinedDiff::Conflict(_) => Err(HistoryReadFailure::new(
            HistoryReadFailureKind::MalformedOutput,
            "selected commit diff unexpectedly used combined merge format",
        )),
    }
}

fn map_diff_failure(failure: &crate::diff::DiffError) -> HistoryReadFailure {
    use crate::diff::DiffErrorKind;
    let kind = match failure.kind {
        DiffErrorKind::TooLarge => HistoryReadFailureKind::TooLarge,
        DiffErrorKind::Canceled => HistoryReadFailureKind::Canceled,
        DiffErrorKind::GitFailed => HistoryReadFailureKind::ProcessFailed,
        DiffErrorKind::MalformedMetadata | DiffErrorKind::MalformedPatch => {
            HistoryReadFailureKind::MalformedOutput
        }
        DiffErrorKind::UnsupportedBinary | DiffErrorKind::InvalidSelection => {
            HistoryReadFailureKind::Internal
        }
    };
    HistoryReadFailure::new(kind, failure.detail())
}

fn malformed(field: &str) -> HistoryReadFailure {
    HistoryReadFailure::new(
        HistoryReadFailureKind::MalformedOutput,
        format!("Git returned a malformed commit {field}"),
    )
}

#[cfg(test)]
mod batch_tests {
    use super::*;
    use crate::process::ActivityGitRunner;
    use crate::types::{RepositoryId, RepositoryPath};

    #[test]
    fn batch_framing_retains_duplicates_and_rejects_truncation_and_wrong_identity() {
        let oid = ObjectId::parse("1".repeat(40)).unwrap();
        let body = format!(
            "tree {}\nauthor A <a@b> 1 +0000\ncommitter A <a@b> 1 +0000\n\nsubject\n",
            "2".repeat(40)
        );
        let frame = format!("{} commit {}\n{}\n", oid.as_str(), body.len(), body);
        let values =
            parse_detail_batch(&[oid.clone(), oid.clone()], frame.repeat(2).as_bytes()).unwrap();
        assert_eq!(values[0], values[1]);
        assert!(parse_detail_batch(
            std::slice::from_ref(&oid),
            &frame.as_bytes()[..frame.len() - 1]
        )
        .is_err());
        let wrong = ObjectId::parse("3".repeat(40)).unwrap();
        assert!(parse_detail_batch(&[wrong], frame.as_bytes()).is_err());
        assert_eq!(
            parse_detail_batch(
                std::slice::from_ref(&oid),
                format!("{} missing\n", oid.as_str()).as_bytes()
            )
            .unwrap_err()
            .kind,
            HistoryReadFailureKind::MissingObject
        );
    }

    #[test]
    fn batch_uses_one_process_and_empty_or_canceled_reads_use_none() {
        let root = std::env::temp_dir().join(format!(
            "devmil-commit-batch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "subject",
        ]);
        let oid = ObjectId::parse(
            String::from_utf8(git(&["rev-parse", "HEAD"]))
                .unwrap()
                .trim(),
        )
        .unwrap();
        let worktree = WorktreeId::new(
            RepositoryId::new(RepositoryPath::new(root.join(".git")).unwrap()),
            RepositoryPath::new(&root).unwrap(),
            RepositoryPath::new(root.join(".git")).unwrap(),
        );
        let mut calls = 0;
        let mut executor = |command| {
            calls += 1;
            ActivityGitRunner::run_private(command, ActivityId::new(1).unwrap(), None, &mut |_| {})
        };
        let loader = CommitLoader::new("git");
        assert!(loader
            .load_details(&worktree, &[], &mut executor, None)
            .unwrap()
            .is_empty());
        let canceled = std::sync::atomic::AtomicBool::new(true);
        assert_eq!(
            loader
                .load_details(
                    &worktree,
                    std::slice::from_ref(&oid),
                    &mut executor,
                    Some(&canceled)
                )
                .unwrap_err()
                .kind,
            HistoryReadFailureKind::Canceled
        );
        let details = loader
            .load_details(&worktree, &[oid.clone(), oid], &mut executor, None)
            .unwrap();
        assert_eq!(details.len(), 2);
        assert_eq!(details[0], details[1]);
        assert_eq!(calls, 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}
