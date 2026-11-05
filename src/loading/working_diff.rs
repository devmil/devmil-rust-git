//! Bounded working-tree/index diff reads with incremental first-file publication.
use crate::activity::{ActivityId, ActivityStatus};
use crate::diff::model::{
    ContextLines, DiffDocument, DiffFallback, DiffFile, DiffFileId, DiffLayout, DiffLineOrigin,
    DiffPath, DiffWhitespacePolicy, FileChangeKind, FileStagingState, GitFileMode,
    SyntaxAvailability,
};
use crate::diff::{
    parse_combined_raw_diff_z, parse_raw_diff_z, parse_unified_patch_with_raw_metadata, DiffError,
    DiffErrorKind, PatchLimits, RawFileChangeKind,
};
use crate::process::{GitProcessCommand, PrivateStdoutObserver, ProcessArgument};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::PathBuf;
const RAW_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const PATCH_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
struct CommandOutput {
    activity: ActivityId,
    bytes: Vec<u8>,
    difference_exit: bool,
}
struct WorkingDiffRead {
    document: DiffDocument,
    raw_bytes: Vec<u8>,
    first_raw_bytes: Vec<u8>,
    activity: Option<ActivityId>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkingDiffLimits {
    pub raw_output_bytes: usize,
    pub patch_output_bytes: usize,
    pub patch: PatchLimits,
}

impl Default for WorkingDiffLimits {
    fn default() -> Self {
        Self {
            raw_output_bytes: RAW_OUTPUT_LIMIT,
            patch_output_bytes: PATCH_OUTPUT_LIMIT,
            patch: PatchLimits::default(),
        }
    }
}

/// An exact Git endpoint; callers choose both sides explicitly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiffEndpoint {
    EmptyTree,
    Head,
    Index,
    WorkingTree,
    Commit(crate::types::ObjectId),
}

/// Inputs affecting Git diff output, independent of consumer request identities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkingDiffKey {
    pub worktree: crate::types::WorktreeId,
    pub base: DiffEndpoint,
    pub target: DiffEndpoint,
    pub file: Option<DiffFileId>,
    pub layout: DiffLayout,
    pub context: ContextLines,
    pub whitespace: DiffWhitespacePolicy,
    /// Diffs every file as text (`git diff --text`), even one Git considers binary.
    pub force_text: bool,
}
impl WorkingDiffKey {
    pub const fn worktree(&self) -> &crate::types::WorktreeId {
        &self.worktree
    }
    pub const fn base(&self) -> &DiffEndpoint {
        &self.base
    }
    pub const fn target(&self) -> &DiffEndpoint {
        &self.target
    }
    pub const fn file(&self) -> Option<&DiffFileId> {
        self.file.as_ref()
    }
    pub const fn layout(&self) -> DiffLayout {
        self.layout
    }
    pub const fn context(&self) -> ContextLines {
        self.context
    }
    pub const fn whitespace(&self) -> DiffWhitespacePolicy {
        self.whitespace
    }
    pub const fn force_text(&self) -> bool {
        self.force_text
    }
}
pub struct WorkingDiffRequest<'a> {
    pub key: &'a WorkingDiffKey,
    pub cancellation: &'a dyn crate::cancellation::Cancellation,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkingDiffValue {
    FirstFile {
        file: DiffFile,
        raw_bytes: Vec<u8>,
    },
    Complete {
        document: DiffDocument,
        raw_bytes: Vec<u8>,
    },
}
/// Executes recorded commands, optionally exposing private stdout to a byte parser.
/// Observer bytes are unredacted and must never be sent to public activity events.
pub trait StreamingCommandExecutor {
    fn execute(
        &mut self,
        command: GitProcessCommand,
        observer: PrivateStdoutObserver<'_>,
    ) -> crate::process::PrivateActivityResult;
    fn after_auxiliary(&mut self, _: ActivityId) {}
}

pub struct WorkingDiffLoader<'a> {
    executable: &'a OsStr,
    executor: std::cell::RefCell<&'a mut dyn StreamingCommandExecutor>,
    limits: WorkingDiffLimits,
}
impl<'a> WorkingDiffLoader<'a> {
    pub fn new(
        executable: &'a OsStr,
        executor: &'a mut dyn StreamingCommandExecutor,
        limits: WorkingDiffLimits,
    ) -> Self {
        let ceiling = WorkingDiffLimits::default();
        Self {
            executable,
            executor: std::cell::RefCell::new(executor),
            limits: WorkingDiffLimits {
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
            },
        }
    }
    fn run(
        &self,
        request: &WorkingDiffRequest<'_>,
        arguments: Vec<ProcessArgument>,
        output_limit: usize,
        difference_exit: bool,
        observer: PrivateStdoutObserver<'_>,
        prior_activity: Option<ActivityId>,
    ) -> Result<CommandOutput, DiffError> {
        if request.cancellation.is_cancelled() {
            return Err(prior_activity.map_or_else(canceled, |id| with_activity(canceled(), id)));
        }
        let command = GitProcessCommand::git(self.executable.to_os_string())
            .current_dir(request.key.worktree().worktree_root().as_path())
            .args(arguments)
            .output_limit(output_limit)
            // File names and diff content stay private. The activity still records the exact
            // option shape and a redacted placeholder for each selected path.
            .redact_output()
            .env("GIT_OPTIONAL_LOCKS", "0")
            .expect("constant environment name is valid")
            .env("LC_ALL", "C")
            .expect("constant environment name is valid");
        let mut result = self.executor.borrow_mut().execute(command, observer);
        let id = result.activity.id;
        let stdout_truncated = result.stdout.truncated;
        let stdout = result.stdout.take_bytes();
        let status = &result.activity.status;
        if request.cancellation.is_cancelled() {
            return Err(with_activity(canceled(), id));
        }
        match status {
            ActivityStatus::Canceled { .. } => Err(with_activity(canceled(), id)),
            ActivityStatus::Succeeded { .. } if stdout_truncated => Err(with_activity(
                DiffError::new(
                    DiffErrorKind::TooLarge,
                    "Git diff output exceeded its bounded result limit",
                ),
                id,
            )),
            ActivityStatus::Succeeded { .. } => Ok(CommandOutput {
                activity: id,
                bytes: stdout,
                difference_exit: false,
            }),
            ActivityStatus::Failed { exit_code: Some(1) } if difference_exit => {
                if stdout_truncated {
                    Err(with_activity(
                        DiffError::new(
                            DiffErrorKind::TooLarge,
                            "Git diff output exceeded its bounded result limit",
                        ),
                        id,
                    ))
                } else {
                    Ok(CommandOutput {
                        activity: id,
                        bytes: stdout,
                        difference_exit: true,
                    })
                }
            }
            ActivityStatus::LaunchError { .. }
            | ActivityStatus::Failed { .. }
            | ActivityStatus::TimedOut { .. }
            | ActivityStatus::Running => Err(with_activity(
                DiffError::new(DiffErrorKind::GitFailed, "Git could not read the diff"),
                id,
            )),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn read(
        &self,
        request: &WorkingDiffRequest<'_>,
        observer: PrivateStdoutObserver<'_>,
    ) -> Result<WorkingDiffRead, DiffError> {
        let key = request.key;
        if endpoints_equal(key.base(), key.target()) {
            return Ok(WorkingDiffRead {
                document: empty_document(key.layout()),
                raw_bytes: Vec::new(),
                first_raw_bytes: Vec::new(),
                activity: None,
            });
        }
        let untracked = untracked_candidate(key)?;
        let mut last_activity = None;
        if let Some(untracked) = untracked {
            let activity = self.preflight_untracked(request, untracked)?;
            last_activity = Some(activity);
            self.executor.borrow_mut().after_auxiliary(activity);
        }
        let empty_tree = if needs_empty_tree(key) {
            let (tree, activity) = self.resolve_empty_tree(request, last_activity)?;
            last_activity = Some(activity);
            self.executor.borrow_mut().after_auxiliary(activity);
            Some(tree)
        } else {
            None
        };
        if request.cancellation.is_cancelled() {
            return Err(last_activity.map_or_else(canceled, |id| with_activity(canceled(), id)));
        }
        if let Some(untracked) = untracked {
            if let Some(size) = super::diff_preview::worktree_size(
                key.worktree().worktree_root().as_path(),
                untracked.path,
            )
            .filter(|size| *size > super::diff_preview::MAX_INLINE_FILE_BYTES)
            {
                let mut document = empty_untracked_document(key, untracked);
                document.files[0].fallback = Some(DiffFallback::TooLarge {
                    byte_count: size,
                    line_count: 0,
                    preview_line_limit: 0,
                });
                return Ok(WorkingDiffRead {
                    document,
                    raw_bytes: Vec::new(),
                    first_raw_bytes: Vec::new(),
                    activity: last_activity,
                });
            }
        }
        let plan = if untracked.is_none() {
            let arguments =
                diff_arguments_with_preview(key, empty_tree.as_deref(), None, false, None)?;
            let output = self.run(
                request,
                arguments,
                self.limits.raw_output_bytes,
                false,
                None,
                last_activity,
            )?;
            last_activity = Some(output.activity);
            // Combined conflicts have multiple parents and retain their dedicated projection.
            if output.bytes.starts_with(b"::") {
                None
            } else {
                Some(super::diff_preview::PreviewPlan::inspect(
                    &output.bytes,
                    key.worktree().worktree_root().as_path(),
                    self.executable,
                    if matches!(key.target(), DiffEndpoint::WorkingTree) {
                        Some(true)
                    } else if matches!(key.base(), DiffEndpoint::WorkingTree) {
                        Some(false)
                    } else {
                        None
                    },
                    Some(request.cancellation),
                    &mut |command| {
                        let result = self.executor.borrow_mut().execute(command, None);
                        self.executor
                            .borrow_mut()
                            .after_auxiliary(result.activity.id);
                        result
                    },
                )?)
            }
        } else {
            None
        };
        let large_plan = plan.as_ref().filter(|plan| plan.has_large_files());
        if let Some(plan) = large_plan.filter(|plan| plan.inline_paths().is_empty()) {
            let mut document = plan.merge(empty_document(key.layout()));
            apply_staging_state(&mut document, key, false);
            return Ok(WorkingDiffRead {
                document,
                raw_bytes: Vec::new(),
                first_raw_bytes: Vec::new(),
                activity: last_activity,
            });
        }
        let arguments = diff_arguments_with_preview(
            key,
            empty_tree.as_deref(),
            untracked,
            true,
            large_plan.map(|plan| plan.inline_paths()),
        )?;
        let difference_exit = untracked.is_some();
        let output_limit = combined_output_limit(self.limits);
        let output = self.run(
            request,
            arguments,
            output_limit,
            difference_exit,
            observer,
            last_activity,
        )?;
        let activity = output.activity;
        let combined = output.bytes;
        if combined.len() > output_limit {
            return Err(with_activity(
                DiffError::new(
                    DiffErrorKind::TooLarge,
                    "Git diff output exceeded its bounded combined limit",
                ),
                activity,
            ));
        }
        if output.difference_exit && combined.is_empty() {
            return Err(with_activity(
                DiffError::new(
                    DiffErrorKind::GitFailed,
                    "Git could not read the selected untracked file",
                ),
                activity,
            ));
        }
        let streams = split_combined_diff(&combined).map_err(|failure| {
            if output.difference_exit {
                with_activity(
                    DiffError::new(
                        DiffErrorKind::GitFailed,
                        "Git returned no usable untracked-file diff",
                    ),
                    activity,
                )
            } else {
                with_activity(failure, activity)
            }
        })?;
        let (raw, patch) = match streams {
            CombinedDiff::Conflict(raw) => {
                if raw.len() > self.limits.raw_output_bytes {
                    return Err(with_activity(
                        DiffError::new(
                            DiffErrorKind::TooLarge,
                            "Git conflict metadata exceeded its bounded raw stream limit",
                        ),
                        activity,
                    ));
                }
                let mut document =
                    conflict_document(&raw).map_err(|failure| with_activity(failure, activity))?;
                document.layout = key.layout();
                apply_staging_state(&mut document, key, false);
                validate_file_scope(&document, key.file())
                    .map_err(|failure| with_activity(failure, activity))?;
                let first_raw = raw.clone();
                return Ok(WorkingDiffRead {
                    document,
                    raw_bytes: combined,
                    first_raw_bytes: first_raw,
                    activity: Some(activity),
                });
            }
            CombinedDiff::Empty => {
                if output.difference_exit {
                    return Err(with_activity(
                        DiffError::new(
                            DiffErrorKind::GitFailed,
                            "Git returned no usable untracked-file diff",
                        ),
                        activity,
                    ));
                }
                if let Some(untracked) = untracked {
                    let document = empty_untracked_document(key, untracked);
                    return Ok(WorkingDiffRead {
                        document,
                        raw_bytes: combined,
                        first_raw_bytes: Vec::new(),
                        activity: Some(activity),
                    });
                }
                return Ok(WorkingDiffRead {
                    document: empty_document(key.layout()),
                    raw_bytes: combined,
                    first_raw_bytes: Vec::new(),
                    activity: Some(activity),
                });
            }
            CombinedDiff::RawOnly(raw) => {
                if output.difference_exit
                    || matches!(key.whitespace(), DiffWhitespacePolicy::Preserve)
                {
                    return Err(with_activity(
                        DiffError::new(
                            DiffErrorKind::MalformedMetadata,
                            "Git returned diff metadata without a patch",
                        ),
                        activity,
                    ));
                }
                let raw = if raw.ends_with(b"\0\0") {
                    &raw[..raw.len() - 1]
                } else {
                    raw.as_slice()
                };
                parse_raw_diff_z(raw).map_err(|failure| with_activity(failure, activity))?;
                return Ok(WorkingDiffRead {
                    document: empty_document(key.layout()),
                    raw_bytes: combined,
                    first_raw_bytes: Vec::new(),
                    activity: Some(activity),
                });
            }
            CombinedDiff::Standard { raw, patch } => (raw, patch),
        };
        if raw.len() > self.limits.raw_output_bytes || patch.len() > self.limits.patch_output_bytes
        {
            return Err(with_activity(
                DiffError::new(
                    DiffErrorKind::TooLarge,
                    "Git diff output exceeded its bounded stream limit",
                ),
                activity,
            ));
        }
        let parse = if large_plan.is_some() && key.whitespace() != DiffWhitespacePolicy::Preserve {
            crate::diff::parse_unified_patch_with_filtered_raw_metadata
        } else {
            parse_unified_patch_with_raw_metadata
        };
        let mut document = parse(&raw, &patch, self.limits.patch).map_err(|failure| {
            if output.difference_exit {
                with_activity(
                    DiffError::new(
                        DiffErrorKind::GitFailed,
                        "Git returned no usable untracked-file diff",
                    ),
                    activity,
                )
            } else {
                with_activity(failure, activity)
            }
        })?;
        if output.difference_exit && document.files.is_empty() {
            return Err(with_activity(
                DiffError::new(
                    DiffErrorKind::GitFailed,
                    "Git returned no usable untracked-file diff",
                ),
                activity,
            ));
        }
        document.layout = key.layout();
        if let Some(plan) = large_plan {
            document = plan.merge(document);
        }
        apply_staging_state(&mut document, key, untracked.is_some());
        validate_file_scope(&document, key.file())
            .map_err(|failure| with_activity(failure, activity))?;
        self.populate_binary_size(request, &mut document);
        let first_raw = document.files.first().map_or_else(Vec::new, |file| {
            first_file_patch(&patch, file.kind == FileChangeKind::TypeChanged)
        });
        Ok(WorkingDiffRead {
            document,
            raw_bytes: combined,
            first_raw_bytes: first_raw,
            activity: Some(activity),
        })
    }

    fn preflight_untracked(
        &self,
        request: &WorkingDiffRequest<'_>,
        untracked: UntrackedCandidate<'_>,
    ) -> Result<ActivityId, DiffError> {
        validate_no_index_path(untracked.path)?;
        let arguments = vec![
            visible("--literal-pathspecs"),
            visible("ls-files"),
            visible("--others"),
            visible("--exclude-standard"),
            visible("-z"),
            visible("--"),
            redacted_path(untracked.path)?,
        ];
        let output = self.run(
            request,
            arguments,
            untracked.path.as_bytes().len().saturating_add(128),
            false,
            None,
            None,
        )?;
        // Status can also return a staged rename whose source is this path.
        // Query only untracked paths so that registration cannot consume the
        // output budget or invalidate the exact untracked-file identity.
        let expected = untracked.path.as_bytes();
        let fields = output
            .bytes
            .strip_suffix(b"\0")
            .map(|bytes| bytes.split(|byte| *byte == 0).collect::<Vec<_>>())
            .unwrap_or_default();
        if fields.as_slice() == [expected] {
            Ok(output.activity)
        } else {
            Err(with_activity(
                DiffError::new(
                    DiffErrorKind::GitFailed,
                    "selected path is not an accessible untracked file",
                ),
                output.activity,
            ))
        }
    }

    fn resolve_empty_tree(
        &self,
        request: &WorkingDiffRequest<'_>,
        prior_activity: Option<ActivityId>,
    ) -> Result<(OsString, ActivityId), DiffError> {
        let arguments = vec![
            visible("hash-object"),
            visible("-t"),
            visible("tree"),
            visible("--stdin"),
        ];
        let output = self.run(request, arguments, 256, false, None, prior_activity)?;
        let text = std::str::from_utf8(&output.bytes)
            .ok()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                with_activity(
                    DiffError::new(
                        DiffErrorKind::MalformedMetadata,
                        "Git returned an invalid empty-tree object identity",
                    ),
                    output.activity,
                )
            })?;
        crate::types::ObjectId::parse(text).map_err(|_| {
            with_activity(
                DiffError::new(
                    DiffErrorKind::MalformedMetadata,
                    "Git returned an invalid empty-tree object identity",
                ),
                output.activity,
            )
        })?;
        Ok((OsString::from(text), output.activity))
    }

    fn populate_binary_size(&self, request: &WorkingDiffRequest<'_>, document: &mut DiffDocument) {
        let [file] = document.files.as_mut_slice() else {
            return;
        };
        if !matches!(
            file.fallback,
            Some(DiffFallback::Binary { byte_count: None })
        ) {
            return;
        }
        let key = request.key;
        let endpoint_and_path = file
            .id
            .new_path
            .as_ref()
            .map(|path| (key.target(), path))
            .or_else(|| file.id.old_path.as_ref().map(|path| (key.base(), path)));
        let Some((endpoint, path)) = endpoint_and_path else {
            return;
        };
        let resolved_byte_count = if matches!(endpoint, DiffEndpoint::WorkingTree) {
            worktree_file_size(key.worktree().worktree_root().as_path(), path)
        } else if matches!(endpoint, DiffEndpoint::EmptyTree) {
            None
        } else {
            let revision = match endpoint {
                DiffEndpoint::Head => OsString::from("HEAD"),
                DiffEndpoint::Index => OsString::new(),
                DiffEndpoint::Commit(object) => OsString::from(object.as_str()),
                DiffEndpoint::EmptyTree | DiffEndpoint::WorkingTree => unreachable!(),
            };
            let Ok(path) = diff_path_os(path) else {
                return;
            };
            let mut object = revision;
            object.push(OsStr::new(":"));
            object.push(path);
            self.run(
                request,
                vec![
                    visible("cat-file"),
                    visible("-s"),
                    ProcessArgument::redacted(object, "[OBJECT:PATH]"),
                ],
                64,
                false,
                None,
                None,
            )
            .ok()
            .and_then(|output| parse_byte_count(&output.bytes))
        };
        if let (Some(size), Some(DiffFallback::Binary { byte_count })) =
            (resolved_byte_count, &mut file.fallback)
        {
            *byte_count = Some(size);
        }
    }
}

fn parse_byte_count(bytes: &[u8]) -> Option<u64> {
    std::str::from_utf8(bytes).ok()?.trim().parse().ok()
}

fn worktree_file_size(root: &std::path::Path, path: &DiffPath) -> Option<u64> {
    let root = fs::canonicalize(root).ok()?;
    let target = fs::canonicalize(root.join(PathBuf::from(diff_path_os(path).ok()?))).ok()?;
    if !target.starts_with(&root) {
        return None;
    }
    let metadata = fs::metadata(target).ok()?;
    metadata.is_file().then_some(metadata.len())
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)] // Non-Unix rejects paths that cannot cross the process API.
fn diff_path_os(path: &DiffPath) -> Result<OsString, ()> {
    use std::os::unix::ffi::OsStringExt as _;
    Ok(OsString::from_vec(path.as_bytes().to_vec()))
}

#[cfg(not(unix))]
fn diff_path_os(path: &DiffPath) -> Result<OsString, ()> {
    String::from_utf8(path.as_bytes().to_vec())
        .map(OsString::from)
        .map_err(|_| ())
}

impl WorkingDiffLoader<'_> {
    /// Streams the first complete file before returning the full bounded diff.
    pub fn read_diff(
        &self,
        request: &WorkingDiffRequest<'_>,
        publish: &mut dyn FnMut(Result<WorkingDiffValue, DiffError>),
    ) {
        let mut incremental = IncrementalFirstFile::new(request.key.clone(), self.limits.patch);
        let mut streamed = false;
        let result = {
            let mut observer = |bytes: &[u8]| {
                if !request.cancellation.is_cancelled() {
                    let Some((file, raw_bytes)) = incremental.push(bytes) else {
                        return;
                    };
                    streamed = true;
                    publish(publication(
                        request,
                        Ok(WorkingDiffValue::FirstFile { file, raw_bytes }),
                    ));
                }
            };
            self.read(request, Some(&mut observer))
        };
        let WorkingDiffRead {
            document,
            raw_bytes,
            first_raw_bytes,
            activity,
        } = match result {
            Ok(value) => value,
            Err(error) => {
                publish(publication(request, Err(error)));
                return;
            }
        };
        if request.cancellation.is_cancelled() {
            let error = activity.map_or_else(canceled, |id| with_activity(canceled(), id));
            publish(publication(request, Err(error)));
            return;
        }
        if !streamed {
            if let Some(file) = document.files.first() {
                publish(publication(
                    request,
                    Ok(WorkingDiffValue::FirstFile {
                        file: file.clone(),
                        raw_bytes: first_raw_bytes,
                    }),
                ));
                if request.cancellation.is_cancelled() {
                    let error = activity.map_or_else(canceled, |id| with_activity(canceled(), id));
                    publish(publication(request, Err(error)));
                    return;
                }
            }
        }
        if request.cancellation.is_cancelled() {
            let error = activity.map_or_else(canceled, |id| with_activity(canceled(), id));
            publish(publication(request, Err(error)));
            return;
        }
        publish(publication(
            request,
            Ok(WorkingDiffValue::Complete {
                document,
                raw_bytes,
            }),
        ));
    }
}

struct IncrementalFirstFile {
    key: WorkingDiffKey,
    limits: PatchLimits,
    bytes: Vec<u8>,
    finished: bool,
}

impl IncrementalFirstFile {
    fn new(key: WorkingDiffKey, limits: PatchLimits) -> Self {
        Self {
            key,
            limits,
            bytes: Vec::new(),
            finished: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Option<(DiffFile, Vec<u8>)> {
        if self.finished {
            return None;
        }
        self.bytes.extend_from_slice(bytes);
        let (file, raw_bytes) = parse_incremental_first_file(&self.bytes, &self.key, self.limits)?;
        self.finished = true;
        Some((file, raw_bytes))
    }
}

fn parse_incremental_first_file(
    input: &[u8],
    key: &WorkingDiffKey,
    limits: PatchLimits,
) -> Option<(DiffFile, Vec<u8>)> {
    let separator = b"\0\0diff --git ";
    let split = input
        .windows(separator.len())
        .position(|window| window == separator)?;
    let raw_end = split.checked_add(1)?;
    let raw = first_raw_record(&input[..raw_end])?;
    let raw_entry = parse_raw_diff_z(raw).ok()?.into_iter().next()?;
    let patch = &input[split + 2..];
    let marker = b"\ndiff --git ";
    let mut boundaries = patch
        .windows(marker.len())
        .enumerate()
        .filter_map(|(index, window)| (window == marker).then_some(index));
    let boundary = if matches!(raw_entry.raw_kind, RawFileChangeKind::TypeChanged) {
        boundaries.nth(1)?
    } else {
        boundaries.next()?
    };
    let first_patch = &patch[..=boundary];
    let mut document = parse_unified_patch_with_raw_metadata(raw, first_patch, limits).ok()?;
    document.layout = key.layout();
    apply_staging_state(&mut document, key, false);
    validate_file_scope(&document, key.file()).ok()?;
    let [file] = document.files.as_slice() else {
        return None;
    };
    Some((file.clone(), first_patch.to_vec()))
}

fn first_raw_record(input: &[u8]) -> Option<&[u8]> {
    let header_end = input.iter().position(|byte| *byte == 0)?;
    let header = &input[..header_end];
    if !header.starts_with(b":") || header.starts_with(b"::") {
        return None;
    }
    let status = header.rsplit(|byte| *byte == b' ').next()?;
    let path_fields = usize::from(matches!(status.first(), Some(b'R' | b'C'))) + 1;
    let mut cursor = header_end + 1;
    for _ in 0..path_fields {
        let end = input.get(cursor..)?.iter().position(|byte| *byte == 0)?;
        cursor = cursor.checked_add(end)?.checked_add(1)?;
    }
    Some(&input[..cursor])
}

fn diff_arguments_with_preview(
    key: &WorkingDiffKey,
    empty_tree: Option<&std::ffi::OsStr>,
    untracked: Option<UntrackedCandidate<'_>>,
    patch: bool,
    inline_paths: Option<Vec<&DiffPath>>,
) -> Result<Vec<ProcessArgument>, DiffError> {
    let mut arguments = vec![visible("--literal-pathspecs"), visible("diff")];
    arguments.extend([
        visible("--no-ext-diff"),
        visible("--no-textconv"),
        visible("-M"),
        visible("-C"),
    ]);
    arguments.extend(whitespace_argument(key.whitespace()));
    if key.force_text() {
        arguments.push(visible("--text"));
    }
    arguments.extend([visible("--raw"), visible("-z"), visible("--no-abbrev")]);
    if patch {
        arguments.extend([
            visible("--patch"),
            visible("--binary"),
            visible("--full-index"),
            visible("--no-color"),
            visible(format!("--unified={}", key.context().value())),
        ]);
    }

    if let Some(untracked) = untracked {
        arguments.push(visible("--no-index"));
        arguments.push(visible("--"));
        match untracked.direction {
            UntrackedDirection::Addition => {
                arguments.push(visible(null_device()));
                arguments.push(redacted_path(untracked.path)?);
            }
            UntrackedDirection::Deletion => {
                arguments.push(redacted_path(untracked.path)?);
                arguments.push(visible(null_device()));
            }
        }
        return Ok(arguments);
    }

    let base = treeish(key.base(), empty_tree)?;
    let target = treeish(key.target(), empty_tree)?;
    match (key.base(), key.target()) {
        (DiffEndpoint::Index, DiffEndpoint::WorkingTree) => {}
        (DiffEndpoint::WorkingTree, DiffEndpoint::Index) => arguments.push(visible("-R")),
        (DiffEndpoint::Index, _) => {
            arguments.push(visible("--cached"));
            arguments.push(visible(
                target.expect("non-working-tree target has a treeish"),
            ));
            arguments.push(visible("-R"));
        }
        (_, DiffEndpoint::Index) => {
            arguments.push(visible("--cached"));
            arguments.push(visible(base.expect("non-working-tree base has a treeish")));
        }
        (DiffEndpoint::WorkingTree, _) => {
            arguments.push(visible(target.expect("non-index target has a treeish")));
            arguments.push(visible("-R"));
        }
        (_, DiffEndpoint::WorkingTree) => {
            arguments.push(visible(base.expect("non-index base has a treeish")));
        }
        _ => {
            arguments.push(visible(base.expect("tree endpoint has a treeish")));
            arguments.push(visible(target.expect("tree endpoint has a treeish")));
        }
    }
    arguments.push(visible("--"));
    for path in inline_paths.unwrap_or(selected_paths(key.file())?) {
        arguments.push(redacted_path(path)?);
    }
    Ok(arguments)
}

fn treeish(
    endpoint: &DiffEndpoint,
    empty_tree: Option<&std::ffi::OsStr>,
) -> Result<Option<OsString>, DiffError> {
    match endpoint {
        DiffEndpoint::EmptyTree => empty_tree.map(OsString::from).map(Some).ok_or_else(|| {
            DiffError::new(
                DiffErrorKind::InvalidSelection,
                "empty-tree diff endpoint was not resolved",
            )
        }),
        DiffEndpoint::Head => Ok(Some(OsString::from("HEAD"))),
        DiffEndpoint::Commit(id) => Ok(Some(OsString::from(id.as_str()))),
        DiffEndpoint::Index | DiffEndpoint::WorkingTree => Ok(None),
    }
}

fn selected_paths(file: Option<&DiffFileId>) -> Result<Vec<&DiffPath>, DiffError> {
    let Some(file) = file else {
        return Ok(Vec::new());
    };
    let mut paths = Vec::with_capacity(2);
    if let Some(path) = file.old_path.as_ref() {
        paths.push(path);
    }
    if let Some(path) = file
        .new_path
        .as_ref()
        .filter(|path| paths.first() != Some(path))
    {
        paths.push(path);
    }
    if paths.is_empty() {
        return Err(DiffError::new(
            DiffErrorKind::InvalidSelection,
            "selected diff file has no repository path",
        ));
    }
    Ok(paths)
}

#[derive(Clone, Copy)]
struct UntrackedCandidate<'a> {
    path: &'a DiffPath,
    direction: UntrackedDirection,
}

#[derive(Clone, Copy)]
enum UntrackedDirection {
    Addition,
    Deletion,
}

fn untracked_candidate(key: &WorkingDiffKey) -> Result<Option<UntrackedCandidate<'_>>, DiffError> {
    let Some(file) = key.file() else {
        return Ok(None);
    };
    match (key.base(), key.target()) {
        (DiffEndpoint::Index, DiffEndpoint::WorkingTree) if file.old_path.is_none() => file
            .new_path
            .as_ref()
            .map(|path| {
                Some(UntrackedCandidate {
                    path,
                    direction: UntrackedDirection::Addition,
                })
            })
            .ok_or_else(|| {
                DiffError::new(
                    DiffErrorKind::InvalidSelection,
                    "selected untracked file has no working-tree path",
                )
            }),
        (DiffEndpoint::WorkingTree, DiffEndpoint::Index) if file.new_path.is_none() => file
            .old_path
            .as_ref()
            .map(|path| {
                Some(UntrackedCandidate {
                    path,
                    direction: UntrackedDirection::Deletion,
                })
            })
            .ok_or_else(|| {
                DiffError::new(
                    DiffErrorKind::InvalidSelection,
                    "selected reverse-untracked file has no working-tree path",
                )
            }),
        _ => Ok(None),
    }
}

fn apply_staging_state(document: &mut DiffDocument, key: &WorkingDiffKey, untracked: bool) {
    let state = if matches!(
        (key.base(), key.target()),
        (DiffEndpoint::Index, DiffEndpoint::WorkingTree)
            | (DiffEndpoint::WorkingTree, DiffEndpoint::Index)
    ) {
        FileStagingState::Unstaged
    } else if matches!(key.base(), DiffEndpoint::Index)
        || matches!(key.target(), DiffEndpoint::Index)
    {
        FileStagingState::Staged
    } else {
        FileStagingState::NotApplicable
    };
    let changed_line_state = match state {
        FileStagingState::Unstaged => Some(false),
        FileStagingState::Staged => Some(true),
        FileStagingState::PartiallyStaged | FileStagingState::NotApplicable => None,
    };
    for file in &mut document.files {
        file.staging = state;
        if untracked {
            file.kind = FileChangeKind::Untracked;
        }
        for hunk in &mut file.hunks {
            for line in &mut hunk.lines {
                line.staged = (line.origin != DiffLineOrigin::Context)
                    .then_some(changed_line_state)
                    .flatten();
            }
        }
    }
}

fn empty_untracked_document(
    key: &WorkingDiffKey,
    untracked: UntrackedCandidate<'_>,
) -> DiffDocument {
    let id = match untracked.direction {
        UntrackedDirection::Addition => DiffFileId {
            old_path: None,
            new_path: Some(untracked.path.clone()),
        },
        UntrackedDirection::Deletion => DiffFileId {
            old_path: Some(untracked.path.clone()),
            new_path: None,
        },
    };
    DiffDocument {
        layout: key.layout(),
        files: vec![DiffFile {
            id,
            kind: FileChangeKind::Untracked,
            old_mode: matches!(untracked.direction, UntrackedDirection::Deletion)
                .then_some(GitFileMode::REGULAR),
            new_mode: matches!(untracked.direction, UntrackedDirection::Addition)
                .then_some(GitFileMode::REGULAR),
            syntax: SyntaxAvailability::Unavailable,
            staging: FileStagingState::Unstaged,
            hunks: Vec::new(),
            fallback: None,
            patch_header: Vec::new(),
        }],
    }
}

#[cfg_attr(not(windows), allow(clippy::unnecessary_wraps))]
fn validate_no_index_path(path: &DiffPath) -> Result<(), DiffError> {
    #[cfg(windows)]
    {
        validate_windows_no_index_path(path.as_bytes())?;
    }
    #[cfg(not(windows))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn validate_windows_no_index_path(bytes: &[u8]) -> Result<(), DiffError> {
    if bytes.starts_with(b"\\")
        || bytes.starts_with(b"/")
        || bytes.get(1) == Some(&b':')
        || bytes
            .split(|byte| *byte == b'/' || *byte == b'\\')
            .any(|component| component.is_empty() || component == b"." || component == b"..")
    {
        Err(DiffError::new(
            DiffErrorKind::InvalidSelection,
            "untracked diff path is not a relative native Windows path",
        ))
    } else {
        Ok(())
    }
}

fn validate_file_scope(
    document: &DiffDocument,
    selected: Option<&DiffFileId>,
) -> Result<(), DiffError> {
    let Some(selected) = selected else {
        return Ok(());
    };
    match document.files.as_slice() {
        [] => Ok(()),
        [file] if &file.id == selected => Ok(()),
        _ => Err(DiffError::new(
            DiffErrorKind::MalformedMetadata,
            "file-scoped Git diff did not return the selected file identity",
        )),
    }
}

fn endpoints_equal(base: &DiffEndpoint, target: &DiffEndpoint) -> bool {
    base == target
}

fn needs_empty_tree(key: &WorkingDiffKey) -> bool {
    matches!(key.base(), DiffEndpoint::EmptyTree) || matches!(key.target(), DiffEndpoint::EmptyTree)
}

fn combined_output_limit(limits: WorkingDiffLimits) -> usize {
    limits
        .raw_output_bytes
        .saturating_add(limits.patch_output_bytes)
        .saturating_add(1)
}

fn whitespace_argument(policy: DiffWhitespacePolicy) -> Option<ProcessArgument> {
    match policy {
        DiffWhitespacePolicy::Preserve => None,
        DiffWhitespacePolicy::IgnoreAll => Some(visible("--ignore-all-space")),
        DiffWhitespacePolicy::IgnoreAmount => Some(visible("--ignore-space-change")),
        DiffWhitespacePolicy::IgnoreEndOfLine => Some(visible("--ignore-space-at-eol")),
    }
}

fn publication(
    _: &WorkingDiffRequest<'_>,
    outcome: Result<WorkingDiffValue, DiffError>,
) -> Result<WorkingDiffValue, DiffError> {
    outcome
}

fn canceled() -> DiffError {
    DiffError::new(DiffErrorKind::Canceled, "diff read canceled")
}

fn with_activity(error: DiffError, activity: ActivityId) -> DiffError {
    let detail = format!("{} (activity {})", error.detail(), activity.get());
    DiffError::new(error.kind, detail)
}

fn empty_document(layout: DiffLayout) -> DiffDocument {
    DiffDocument {
        layout,
        files: Vec::new(),
    }
}

fn visible(value: impl Into<OsString>) -> ProcessArgument {
    ProcessArgument::visible(value)
}

fn redacted_path(path: &DiffPath) -> Result<ProcessArgument, DiffError> {
    diff_path_to_os_string(path).map(|path| ProcessArgument::redacted(path, "[PATH]"))
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn diff_path_to_os_string(path: &DiffPath) -> Result<OsString, DiffError> {
    use std::os::unix::ffi::OsStringExt as _;
    Ok(OsString::from_vec(path.as_bytes().to_vec()))
}

#[cfg(not(unix))]
fn diff_path_to_os_string(path: &DiffPath) -> Result<OsString, DiffError> {
    String::from_utf8(path.as_bytes().to_vec())
        .map(OsString::from)
        .map_err(|_| {
            DiffError::new(
                DiffErrorKind::InvalidSelection,
                "selected diff path cannot be represented on this platform",
            )
        })
}

#[cfg(windows)]
fn null_device() -> &'static str {
    "NUL"
}

#[cfg(not(windows))]
fn null_device() -> &'static str {
    "/dev/null"
}

fn first_file_patch(patch: &[u8], type_change: bool) -> Vec<u8> {
    let marker = b"\ndiff --git ";
    let mut boundaries = patch
        .windows(marker.len())
        .enumerate()
        .filter_map(|(index, window)| (window == marker).then_some(index));
    let boundary = if type_change {
        boundaries.nth(1)
    } else {
        boundaries.next()
    };
    boundary.map_or_else(|| patch.to_vec(), |next| patch[..=next].to_vec())
}

use crate::diff::{split_combined_diff, CombinedDiff};

fn conflict_document(raw: &[u8]) -> Result<DiffDocument, DiffError> {
    let entries = parse_combined_raw_diff_z(raw)?;
    if entries.is_empty() {
        return Err(DiffError::new(
            DiffErrorKind::MalformedMetadata,
            "combined conflict metadata is empty",
        ));
    }
    let files = entries
        .into_iter()
        .map(|entry| {
            let path = entry.path;
            DiffFile {
                id: DiffFileId {
                    old_path: Some(path.clone()),
                    new_path: Some(path),
                },
                kind: FileChangeKind::Conflict,
                old_mode: entry
                    .parent_modes
                    .into_iter()
                    .find(|mode| mode.value() != 0),
                new_mode: (entry.result_mode.value() != 0).then_some(entry.result_mode),
                syntax: SyntaxAvailability::Unavailable,
                staging: FileStagingState::Unstaged,
                hunks: Vec::new(),
                fallback: None,
                patch_header: Vec::new(),
            }
        })
        .collect();
    Ok(DiffDocument {
        layout: DiffLayout::Unified,
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combined_limit_accounts_for_metadata_patch_and_separator() {
        let limits = WorkingDiffLimits {
            raw_output_bytes: 7,
            patch_output_bytes: 11,
            ..WorkingDiffLimits::default()
        };
        assert_eq!(combined_output_limit(limits), 19);
    }

    #[test]
    fn windows_no_index_aliases_fail_closed_without_rejecting_unix_backslash_names() {
        for alias in [
            b"../x".as_slice(),
            b"..\\x",
            b"C:\\x",
            b"\\\\server\\share",
            b"/root",
        ] {
            assert_eq!(
                validate_windows_no_index_path(alias).unwrap_err().kind,
                DiffErrorKind::InvalidSelection
            );
        }
        #[cfg(unix)]
        {
            let path = DiffPath::from_bytes(b"back\\slash".to_vec()).unwrap();
            assert!(validate_no_index_path(&path).is_ok());
        }
    }
}
