//! Git-backed recent commit messages and repository commit templates.

use super::CommandExecutor;
use crate::activity::{ActivityRecord, ActivityStatus};
use crate::cancellation::Cancellation;
use crate::process::{GitProcessCommand, ProcessArgument};
use crate::types::{ObjectId, WorktreeId};
use std::{ffi::OsStr, io::Read, path::Path};

pub const MAX_RECENT_COMMIT_MESSAGES: usize = 20;
pub const MAX_COMMIT_TEMPLATE_BYTES: usize = 128 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecentCommitMessage {
    pub object: ObjectId,
    pub subject: String,
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitTools {
    pub recent: Vec<RecentCommitMessage>,
    pub template: Option<String>,
    pub activities: Vec<ActivityRecord>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitToolsErrorKind {
    Canceled,
    SpawnFailed,
    Failed,
    InvalidOutput,
    TooLarge,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitToolsError {
    pub kind: CommitToolsErrorKind,
    pub detail: &'static str,
    pub activities: Vec<ActivityRecord>,
}

impl std::fmt::Display for CommitToolsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.detail)
    }
}
impl std::error::Error for CommitToolsError {}

fn error(
    kind: CommitToolsErrorKind,
    detail: &'static str,
    activities: &[ActivityRecord],
) -> CommitToolsError {
    CommitToolsError {
        kind,
        detail,
        activities: activities.to_vec(),
    }
}

/// Loads recent messages and the configured template through a recorded executor.
/// Template files can live outside the repository and are read afresh on each call.
/// # Errors
/// Returns bounded command, cancellation, parse or template-file failures.
pub fn load(
    executable: &OsStr,
    worktree: &WorktreeId,
    cancellation: &dyn Cancellation,
    executor: &mut dyn CommandExecutor,
) -> Result<CommitTools, CommitToolsError> {
    let mut activities = Vec::new();
    let mut run = |arguments: &[&str], limit| {
        let command = GitProcessCommand::git(executable)
            .current_dir(worktree.worktree_root().as_path())
            .args(arguments.iter().map(ProcessArgument::visible))
            .output_limit(limit)
            .redact_output();
        let mut result = executor.execute(command);
        let truncated = result.stdout.truncated || result.stderr.truncated;
        let raw = result.stdout.take_bytes();
        let status = result.activity.status.clone();
        activities.push(result.activity);
        if cancellation.is_cancelled() || matches!(status, ActivityStatus::Canceled { .. }) {
            return Err(error(
                CommitToolsErrorKind::Canceled,
                "commit tools loading was canceled",
                &activities,
            ));
        }
        if truncated {
            return Err(error(
                CommitToolsErrorKind::TooLarge,
                "commit tools output exceeded its bound",
                &activities,
            ));
        }
        match status {
            ActivityStatus::Succeeded { .. } => Ok(Some(raw)),
            ActivityStatus::Failed { exit_code: Some(1) }
                if arguments.first() == Some(&"config") =>
            {
                Ok(None)
            }
            ActivityStatus::LaunchError { .. } => Err(error(
                CommitToolsErrorKind::SpawnFailed,
                "could not start Git commit-tools inspection",
                &activities,
            )),
            _ => Err(error(
                CommitToolsErrorKind::Failed,
                "Git could not load commit tools",
                &activities,
            )),
        }
    };
    if cancellation.is_cancelled() {
        return Err(error(
            CommitToolsErrorKind::Canceled,
            "commit tools loading was canceled",
            &[],
        ));
    }
    let count = format!("--max-count={MAX_RECENT_COMMIT_MESSAGES}");
    let recent = run(
        &[
            "log",
            "-z",
            "--no-show-signature",
            &count,
            "--format=%H%x00%B",
        ],
        2 * 1024 * 1024,
    )?
    .unwrap_or_default();
    let template_path = run(&["config", "--path", "--get", "commit.template"], 16 * 1024)?;
    let recent = parse_recent(&recent).map_err(|mut err| {
        err.activities.clone_from(&activities);
        err
    })?;
    let template = template_path
        .map(|raw| read_template(&raw, worktree, &activities))
        .transpose()?
        .flatten();
    if cancellation.is_cancelled() {
        return Err(error(
            CommitToolsErrorKind::Canceled,
            "commit tools loading was canceled",
            &activities,
        ));
    }
    Ok(CommitTools {
        recent,
        template,
        activities,
    })
}

fn read_template(
    raw: &[u8],
    worktree: &WorktreeId,
    activities: &[ActivityRecord],
) -> Result<Option<String>, CommitToolsError> {
    let invalid = |detail| error(CommitToolsErrorKind::InvalidOutput, detail, activities);
    let path = std::str::from_utf8(raw)
        .map_err(|_| invalid("commit template path is not UTF-8"))?
        .trim_end_matches(['\r', '\n']);
    if path.is_empty() || path.contains('\0') {
        return Ok(None);
    }
    let path = Path::new(path);
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        worktree.worktree_root().as_path().join(path)
    };
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|_| invalid("configured commit template cannot be read"))?;
    if !metadata.is_file() || metadata.len() > MAX_COMMIT_TEMPLATE_BYTES as u64 {
        return Err(invalid(
            "configured commit template is not a bounded regular file",
        ));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(
            (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK)
                .bits()
                .cast_signed(),
        );
    }
    let file = options
        .open(path)
        .map_err(|_| invalid("configured commit template cannot be opened"))?;
    if !file
        .metadata()
        .map_err(|_| invalid("configured commit template cannot be inspected"))?
        .is_file()
    {
        return Err(invalid("configured commit template is not a regular file"));
    }
    let mut text = String::new();
    file.take(MAX_COMMIT_TEMPLATE_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|_| invalid("configured commit template is not valid UTF-8"))?;
    if text.len() > MAX_COMMIT_TEMPLATE_BYTES || text.contains('\0') {
        return Err(invalid(
            "configured commit template exceeds its content bounds",
        ));
    }
    Ok(Some(text))
}

/// Parses bounded NUL-delimited log records into message fields.
/// # Errors
/// Rejects incomplete records, invalid object identities and non-UTF-8 messages.
pub fn parse_recent(raw: &[u8]) -> Result<Vec<RecentCommitMessage>, CommitToolsError> {
    let invalid = |detail| error(CommitToolsErrorKind::InvalidOutput, detail, &[]);
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let body = raw
        .strip_suffix(&[0])
        .ok_or_else(|| invalid("Git recent-message output has no trailing record delimiter"))?;
    let fields = body.split(|byte| *byte == 0).collect::<Vec<_>>();
    if fields.len() % 2 != 0 || fields.len() / 2 > MAX_RECENT_COMMIT_MESSAGES {
        return Err(invalid(
            "Git returned an incomplete or oversized recent-message list",
        ));
    }
    fields
        .chunks_exact(2)
        .map(|record| {
            let object = std::str::from_utf8(record[0])
                .ok()
                .and_then(|value| ObjectId::parse(value.to_owned()).ok())
                .ok_or_else(|| invalid("Git returned an invalid recent commit identity"))?;
            let message = std::str::from_utf8(record[1])
                .map_err(|_| invalid("recent commit message is not UTF-8"))?;
            let normalized = message.trim_end_matches('\n').replace("\r\n", "\n");
            let (subject, description) = normalized.split_once('\n').unwrap_or((&normalized, ""));
            Ok(RecentCommitMessage {
                object,
                subject: subject.to_owned(),
                description: description.trim_start_matches('\n').to_owned(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recent_records_preserve_message_and_reject_partial_records() {
        let raw = format!("{}\0subject\n\nbody\n\0", "a".repeat(40));
        let rows = parse_recent(raw.as_bytes()).unwrap();
        assert_eq!(rows[0].subject, "subject");
        assert_eq!(rows[0].description, "body");
        assert!(parse_recent(b"partial\0").is_err());
        assert!(parse_recent(&raw.as_bytes()[..raw.len() - 1]).is_err());
    }
}
