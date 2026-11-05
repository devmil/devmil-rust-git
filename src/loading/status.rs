//! One bounded status query, reusing the version resolved during repository discovery.
use super::CommandExecutor;
use crate::activity::ActivityStatus;
use crate::process::{GitProcessCommand, ProcessArgument};
use crate::status::{discover_operations, parse_porcelain_v2, RepositoryStatus};
use crate::types::WorktreeId;
use crate::version::GitVersion;
use std::ffi::OsStr;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UntrackedFilesMode {
    None,
    Normal,
    All,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum StatusFailure {
    MissingGit,
    UnsupportedGit,
    Canceled,
    InvalidOutput,
    GitFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatusOptions {
    pub bare: bool,
    pub include_ignored: bool,
    pub untracked_files: UntrackedFilesMode,
}

impl Default for StatusOptions {
    fn default() -> Self {
        Self {
            bare: false,
            include_ignored: false,
            untracked_files: UntrackedFilesMode::All,
        }
    }
}

/// Reads status with optional discovery-version reuse. Bare repositories return `None`.
/// # Errors
/// Returns a typed launch, version, cancellation, framing, or Git failure.
pub fn load(
    executable: &OsStr,
    worktree: &WorktreeId,
    known_version: Option<GitVersion>,
    options: StatusOptions,
    executor: &mut dyn CommandExecutor,
) -> Result<Option<RepositoryStatus>, StatusFailure> {
    load_with_index(executable, worktree, known_version, options, None, executor)
}

pub(crate) fn load_with_index(
    executable: &OsStr,
    worktree: &WorktreeId,
    known_version: Option<GitVersion>,
    options: StatusOptions,
    private_index: Option<&std::path::Path>,
    executor: &mut dyn CommandExecutor,
) -> Result<Option<RepositoryStatus>, StatusFailure> {
    let command = |arguments: Vec<ProcessArgument>| {
        let mut command = GitProcessCommand::git(executable);
        if let Some(index) = private_index {
            command = command
                .args(["-c", "core.splitIndex=false"].map(ProcessArgument::visible))
                .env_redacted("GIT_INDEX_FILE", index.as_os_str())
                .expect("constant environment name");
        }
        command
            .current_dir(worktree.worktree_root().as_path())
            .args(arguments)
            .output_limit(8 * 1024 * 1024)
    };
    let version = if let Some(version) = known_version {
        version
    } else {
        let mut result = executor.execute(command(vec![ProcessArgument::visible("--version")]));
        successful(&result.activity.status, result.stdout.truncated)?;
        GitVersion::parse(&result.stdout.take_bytes()).ok_or(StatusFailure::InvalidOutput)?
    };
    if version < GitVersion::new(2, 11, 0) {
        return Err(StatusFailure::UnsupportedGit);
    }
    if options.bare {
        return Ok(None);
    }
    let mut args = [
        "status",
        "--porcelain=v2",
        "--branch",
        "-z",
        match options.untracked_files {
            UntrackedFilesMode::None => "--untracked-files=no",
            UntrackedFilesMode::Normal => "--untracked-files=normal",
            UntrackedFilesMode::All => "--untracked-files=all",
        },
    ]
    .map(ProcessArgument::visible)
    .to_vec();
    if options.include_ignored {
        args.push(ProcessArgument::visible(
            if version >= GitVersion::new(2, 13, 0) {
                "--ignored=matching"
            } else {
                "--ignored"
            },
        ));
    }
    let cmd = command(args)
        .env(
            "GIT_OPTIONAL_LOCKS",
            if private_index.is_some() { "1" } else { "0" },
        )
        .map_err(|_| StatusFailure::GitFailed)?;
    let result = executor.execute(cmd);
    successful(&result.activity.status, result.stdout.truncated)?;
    let (branch, entries) =
        parse_porcelain_v2(&result.stdout.bytes).map_err(|_| StatusFailure::InvalidOutput)?;
    Ok(Some(RepositoryStatus::from_parts(
        branch,
        entries,
        discover_operations(worktree.git_dir().as_path()),
    )))
}

fn successful(status: &ActivityStatus, truncated: bool) -> Result<(), StatusFailure> {
    match status {
        ActivityStatus::Succeeded { .. } if !truncated => Ok(()),
        ActivityStatus::Succeeded { .. } => Err(StatusFailure::InvalidOutput),
        ActivityStatus::Canceled { .. } => Err(StatusFailure::Canceled),
        ActivityStatus::LaunchError { .. } => Err(StatusFailure::MissingGit),
        _ => Err(StatusFailure::GitFailed),
    }
}
