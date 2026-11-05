//! Validating and applying prepared interactive-rebase todo files.
use crate::rebase::{parse_todo, render_todo, validate_plan, MAX_TODO_LINES, MAX_TODO_LINE_BYTES};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
const MAX_PLAN_BYTES: u64 = (MAX_TODO_LINES as u64) * (MAX_TODO_LINE_BYTES as u64);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SequenceEditorErrorKind {
    NotRequested,
    InvalidArguments,
    InvalidPlanPath,
    InvalidTodoPath,
    CommitMessageStop,
    PlanTooLarge,
    InvalidPlan,
    Io,
}

#[derive(Debug)]
pub struct SequenceEditorError {
    kind: SequenceEditorErrorKind,
    detail: String,
}

impl SequenceEditorError {
    pub fn new(kind: SequenceEditorErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    pub const fn kind(&self) -> SequenceEditorErrorKind {
        self.kind
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for SequenceEditorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for SequenceEditorError {}

/// Routes Git's editor target without waiting: todo files receive the prepared plan, while a
/// commit-message target returns a typed stop so the main application can resume the sequencer.
///
/// # Errors
///
/// Returns [`SequenceEditorErrorKind::CommitMessageStop`] for non-todo editor targets, or the
/// validation and I/O errors from [`apply_prepared_plan`].
pub fn apply_editor_target(plan: &Path, target: &Path) -> Result<(), SequenceEditorError> {
    if target
        .file_name()
        .is_none_or(|name| name != "git-rebase-todo")
    {
        return Err(SequenceEditorError::new(
            SequenceEditorErrorKind::CommitMessageStop,
            "interactive rebase paused for commit-message editing",
        ));
    }
    apply_prepared_plan(plan, target)
}

/// Validates and writes one prepared plan into Git's todo file.
///
/// # Errors
///
/// Returns a typed refusal for relative paths, symlinks, non-regular files, oversized input,
/// malformed todo bytes, illegal instruction ordering, or an I/O failure.
pub fn apply_prepared_plan(plan: &Path, todo: &Path) -> Result<(), SequenceEditorError> {
    validate_absolute_regular(
        plan,
        SequenceEditorErrorKind::InvalidPlanPath,
        "prepared plan",
    )?;
    validate_absolute_regular(
        todo,
        SequenceEditorErrorKind::InvalidTodoPath,
        "Git todo file",
    )?;
    let metadata = fs::symlink_metadata(plan).map_err(|error| io_error(&error))?;
    if metadata.len() > MAX_PLAN_BYTES {
        return Err(SequenceEditorError::new(
            SequenceEditorErrorKind::PlanTooLarge,
            "the prepared rebase plan exceeds its byte bound",
        ));
    }
    let bytes = fs::read(plan).map_err(|error| io_error(&error))?;
    let parsed = parse_todo(&bytes).map_err(|error| {
        SequenceEditorError::new(
            SequenceEditorErrorKind::InvalidPlan,
            format!("the prepared rebase plan is malformed: {error}"),
        )
    })?;
    validate_plan(&parsed).map_err(|error| {
        SequenceEditorError::new(
            SequenceEditorErrorKind::InvalidPlan,
            format!("the prepared rebase plan is not executable: {error}"),
        )
    })?;
    let rendered = render_todo(&parsed);
    let mut destination = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(todo)
        .map_err(|error| io_error(&error))?;
    destination
        .write_all(&rendered)
        .map_err(|error| io_error(&error))?;
    destination.sync_all().map_err(|error| io_error(&error))
}

fn validate_absolute_regular(
    path: &Path,
    kind: SequenceEditorErrorKind,
    label: &str,
) -> Result<(), SequenceEditorError> {
    if !path.is_absolute() {
        return Err(SequenceEditorError::new(
            kind,
            format!("the {label} path is not absolute"),
        ));
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| io_error(&error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SequenceEditorError::new(
            kind,
            format!("the {label} is a symlink or non-regular file"),
        ));
    }
    Ok(())
}

fn io_error(error: &std::io::Error) -> SequenceEditorError {
    SequenceEditorError::new(
        SequenceEditorErrorKind::Io,
        format!("sequence-editor file operation failed: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);

    fn directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "devmil-git-sequence-editor-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn valid_plan_replaces_todo_byte_for_byte() {
        let directory = directory();
        let plan = directory.join("plan");
        let todo = directory.join("git-rebase-todo");
        let bytes = b"pick 1111111 first\n# keep this comment\nexec printf ok\n";
        fs::write(&plan, bytes).unwrap();
        fs::write(&todo, b"pick 2222222 old\n").unwrap();
        apply_prepared_plan(&plan, &todo).unwrap();
        assert_eq!(fs::read(todo).unwrap(), bytes);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_plan_never_truncates_gits_todo() {
        let directory = directory();
        let plan = directory.join("plan");
        let todo = directory.join("git-rebase-todo");
        fs::write(&plan, b"squash 1111111 cannot-lead\n").unwrap();
        fs::write(&todo, b"pick 2222222 unchanged\n").unwrap();
        let error = apply_prepared_plan(&plan, &todo).unwrap_err();
        assert_eq!(error.kind(), SequenceEditorErrorKind::InvalidPlan);
        assert_eq!(fs::read(&todo).unwrap(), b"pick 2222222 unchanged\n");
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_plan_is_refused() {
        use std::os::unix::fs::symlink;

        let directory = directory();
        let real = directory.join("real");
        let plan = directory.join("plan");
        let todo = directory.join("git-rebase-todo");
        fs::write(&real, b"pick 1111111 first\n").unwrap();
        symlink(&real, &plan).unwrap();
        fs::write(&todo, b"pick 2222222 unchanged\n").unwrap();
        let error = apply_prepared_plan(&plan, &todo).unwrap_err();
        assert_eq!(error.kind(), SequenceEditorErrorKind::InvalidPlanPath);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn commit_message_target_requests_a_non_blocking_pause() {
        let directory = directory();
        let plan = directory.join("plan");
        let message = directory.join("COMMIT_EDITMSG");
        fs::write(&plan, b"reword 1111111 first\n").unwrap();
        fs::write(&message, b"first\n").unwrap();
        let error = apply_editor_target(&plan, &message).unwrap_err();
        assert_eq!(error.kind(), SequenceEditorErrorKind::CommitMessageStop);
        assert_eq!(fs::read(&message).unwrap(), b"first\n");
        fs::remove_dir_all(directory).unwrap();
    }
}
