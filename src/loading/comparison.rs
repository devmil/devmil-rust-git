//! Bounded ordered two-commit comparisons independent of UI request generations.
use super::CommandExecutor;
use crate::diff::model::{ContextLines, DiffDocument, DiffLayout, DiffWhitespacePolicy};
use crate::diff::{parse_unified_patch_with_raw_metadata, PatchLimits};
use crate::process::{GitProcessCommand, ProcessArgument};
use crate::{
    activity::{ActivityId, ActivityStatus},
    cancellation::Cancellation,
    types::{ObjectId, WorktreeId},
};
use std::{ffi::OsString, fmt};
pub const MAX_COMPARISON_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_COMPARISON_ERROR_BYTES: usize = 512;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComparisonFailureKind {
    InvalidSelection,
    MissingObject,
    ProcessFailed,
    MalformedOutput,
    TooLarge,
    Canceled,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComparisonFailure {
    pub kind: ComparisonFailureKind,
    detail: String,
    activity_id: Option<ActivityId>,
}

impl ComparisonFailure {
    pub fn new(kind: ComparisonFailureKind, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        if detail.len() > MAX_COMPARISON_ERROR_BYTES {
            let mut boundary = MAX_COMPARISON_ERROR_BYTES;
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

impl fmt::Display for ComparisonFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for ComparisonFailure {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComparisonValue {
    pub base: ObjectId,
    pub target: ObjectId,
    pub diff: DiffDocument,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComparisonQuery {
    pub base: ObjectId,
    pub target: ObjectId,
    pub context: ContextLines,
    pub whitespace: DiffWhitespacePolicy,
    pub layout: DiffLayout,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComparisonResult {
    pub outcome: Result<ComparisonValue, ComparisonFailure>,
    pub activity_ids: Vec<ActivityId>,
}
const RAW_OUTPUT_LIMIT: usize = MAX_COMPARISON_OUTPUT_BYTES;
const PATCH_OUTPUT_LIMIT: usize = MAX_COMPARISON_OUTPUT_BYTES;
const COMPARISON_PATCH_LINE_LIMIT: usize = 512 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComparisonLimits {
    pub raw_output_bytes: usize,
    pub patch_output_bytes: usize,
    pub patch: PatchLimits,
}

impl Default for ComparisonLimits {
    fn default() -> Self {
        Self {
            raw_output_bytes: RAW_OUTPUT_LIMIT,
            patch_output_bytes: PATCH_OUTPUT_LIMIT,
            patch: PatchLimits {
                max_bytes: PATCH_OUTPUT_LIMIT,
                max_lines: COMPARISON_PATCH_LINE_LIMIT,
                ..PatchLimits::default()
            },
        }
    }
}

pub struct ComparisonLoader<'a> {
    executable: OsString,
    worktree: &'a WorktreeId,
    cancellation: &'a dyn Cancellation,
    executor: &'a mut dyn CommandExecutor,
    limits: ComparisonLimits,
}
impl<'a> ComparisonLoader<'a> {
    pub fn new(
        executable: impl Into<OsString>,
        worktree: &'a WorktreeId,
        cancellation: &'a dyn Cancellation,
        executor: &'a mut dyn CommandExecutor,
    ) -> Self {
        Self {
            executable: executable.into(),
            worktree,
            cancellation,
            executor,
            limits: ComparisonLimits::default(),
        }
    }
    /// Lowers comparison limits for tests or constrained hosts. Values above the production
    /// ceiling are clamped so callers cannot turn a bounded adapter into an unbounded one.
    #[must_use]
    pub fn with_limits(mut self, limits: ComparisonLimits) -> Self {
        let ceiling = ComparisonLimits::default();
        self.limits = ComparisonLimits {
            raw_output_bytes: limits.raw_output_bytes.min(ceiling.raw_output_bytes),
            patch_output_bytes: limits.patch_output_bytes.min(ceiling.patch_output_bytes),
            patch: PatchLimits {
                max_bytes: limits.patch.max_bytes.min(ceiling.patch.max_bytes),
                max_lines: limits.patch.max_lines.min(ceiling.patch.max_lines),
                preview_line_limit: limits
                    .patch
                    .preview_line_limit
                    .min(ceiling.patch.preview_line_limit),
            },
        };
        self
    }

    fn run(
        &mut self,
        _request: &ComparisonQuery,
        arguments: impl IntoIterator<Item = OsString>,
        output_limit: usize,
    ) -> (Option<ActivityId>, Result<Vec<u8>, ComparisonFailure>) {
        if self.cancellation.is_cancelled() {
            return (
                None,
                Err(ComparisonFailure::new(
                    ComparisonFailureKind::Canceled,
                    "commit comparison canceled",
                )),
            );
        }
        let command = GitProcessCommand::git(self.executable.clone())
            .current_dir(self.worktree.worktree_root().as_path())
            .args(arguments.into_iter().map(ProcessArgument::visible))
            .output_limit(output_limit)
            // Paths and patch content stay parser-private. Activity records expose only the command.
            .redact_output()
            .env("GIT_OPTIONAL_LOCKS", "0")
            .expect("constant environment name is valid")
            .env("LC_ALL", "C")
            .expect("constant environment name is valid");
        let mut result = self.executor.execute(command);
        let id = result.activity.id;
        let truncated = result.stdout.truncated;
        let stdout = result.stdout.take_bytes();
        let stderr = result.stderr.take_bytes();
        let outcome = match result.activity.status {
            ActivityStatus::Canceled { .. } => Err(ComparisonFailure::new(
                ComparisonFailureKind::Canceled,
                "commit comparison canceled",
            )
            .with_activity_id(id)),
            ActivityStatus::Succeeded { .. } if truncated => Err(ComparisonFailure::new(
                ComparisonFailureKind::TooLarge,
                "Git comparison output exceeded its bounded result limit",
            )
            .with_activity_id(id)),
            ActivityStatus::Succeeded { .. } => Ok(stdout),
            ActivityStatus::Failed { .. } if looks_like_missing_object(&stderr) => {
                Err(ComparisonFailure::new(
                    ComparisonFailureKind::MissingObject,
                    "one of the compared commits is no longer available",
                )
                .with_activity_id(id))
            }
            ActivityStatus::LaunchError { .. }
            | ActivityStatus::Failed { .. }
            | ActivityStatus::TimedOut { .. }
            | ActivityStatus::Running => Err(ComparisonFailure::new(
                ComparisonFailureKind::ProcessFailed,
                "Git could not compare the selected commits",
            )
            .with_activity_id(id)),
        };
        (Some(id), outcome)
    }

    fn raw_arguments(request: &ComparisonQuery) -> Vec<OsString> {
        vec![
            "diff".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "-M".into(),
            "--raw".into(),
            "-z".into(),
            "--no-abbrev".into(),
            comparison_range(request),
            "--".into(),
        ]
    }

    fn patch_arguments(request: &ComparisonQuery) -> Vec<OsString> {
        let mut arguments = vec![
            "diff".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "-M".into(),
            "--patch".into(),
            "--binary".into(),
            "--full-index".into(),
            "--no-color".into(),
            comparison_range(request),
            "--".into(),
        ];
        arguments.insert(1, format!("--unified={}", request.context.value()).into());
        let whitespace = match request.whitespace {
            DiffWhitespacePolicy::Preserve => None,
            DiffWhitespacePolicy::IgnoreAll => Some("--ignore-all-space"),
            DiffWhitespacePolicy::IgnoreAmount => Some("--ignore-space-change"),
            DiffWhitespacePolicy::IgnoreEndOfLine => Some("--ignore-space-at-eol"),
        };
        if let Some(argument) = whitespace {
            arguments.insert(1, argument.into());
        }
        arguments
    }
}
impl ComparisonLoader<'_> {
    pub fn load(&mut self, request: &ComparisonQuery) -> ComparisonResult {
        let mut activity_ids = Vec::new();
        let outcome = (|| {
            let (raw_activity, raw) = self.run(
                request,
                Self::raw_arguments(request),
                self.limits.raw_output_bytes,
            );
            activity_ids.extend(raw_activity);
            let raw = raw?;
            let plan = super::diff_preview::PreviewPlan::inspect(
                &raw,
                self.worktree.worktree_root().as_path(),
                &self.executable,
                None,
                Some(self.cancellation),
                &mut |command| {
                    let result = self.executor.execute(command);
                    activity_ids.push(result.activity.id);
                    result
                },
            )
            .map_err(|failure| map_diff_failure(&failure, raw_activity))?;
            let mut diff = if plan.has_large_files() && plan.inline_paths().is_empty() {
                DiffDocument {
                    layout: request.layout,
                    files: Vec::new(),
                }
            } else {
                let mut arguments = Self::patch_arguments(request);
                if plan.has_large_files() {
                    plan.restrict(&mut arguments)
                        .map_err(|failure| map_diff_failure(&failure, raw_activity))?;
                }
                let (activity, patch) =
                    self.run(request, arguments, self.limits.patch_output_bytes);
                activity_ids.extend(activity);
                let patch = patch?;
                let inline_raw = plan.inline_raw(&raw);
                let parse = if request.whitespace == DiffWhitespacePolicy::Preserve {
                    parse_unified_patch_with_raw_metadata
                } else {
                    crate::diff::parse_unified_patch_with_filtered_raw_metadata
                };
                parse(&inline_raw, &patch, self.limits.patch)
                    .map_err(|failure| map_diff_failure(&failure, activity))?
            };
            diff = plan.merge(diff);
            diff.layout = request.layout;
            Ok(ComparisonValue {
                base: request.base.clone(),
                target: request.target.clone(),
                diff,
            })
        })();
        ComparisonResult {
            outcome,
            activity_ids,
        }
    }
}

fn comparison_range(request: &ComparisonQuery) -> OsString {
    OsString::from(format!("{}..{}", request.base, request.target))
}

fn looks_like_missing_object(stderr: &[u8]) -> bool {
    let lower = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    lower.contains("bad object")
        || lower.contains("bad file")
        || lower.contains("unknown revision")
        || lower.contains("not a valid object")
        || lower.contains("could not parse object")
        || lower.contains("invalid object")
        || lower.contains("invalid revision range")
}

fn map_diff_failure(
    failure: &crate::diff::DiffError,
    activity: Option<ActivityId>,
) -> ComparisonFailure {
    use crate::diff::DiffErrorKind as DiffPortErrorKind;
    let kind = match failure.kind {
        DiffPortErrorKind::TooLarge => ComparisonFailureKind::TooLarge,
        DiffPortErrorKind::Canceled => ComparisonFailureKind::Canceled,
        DiffPortErrorKind::GitFailed => ComparisonFailureKind::ProcessFailed,
        DiffPortErrorKind::MalformedMetadata | DiffPortErrorKind::MalformedPatch => {
            ComparisonFailureKind::MalformedOutput
        }
        DiffPortErrorKind::UnsupportedBinary | DiffPortErrorKind::InvalidSelection => {
            ComparisonFailureKind::Internal
        }
    };
    let failure = ComparisonFailure::new(kind, failure.detail());
    match activity {
        Some(activity) => failure.with_activity_id(activity),
        None => failure,
    }
}
