//! Installed-Git repository discovery and native repository identities.

use crate::activity::{ActivityRecord, ActivityStatus};
use crate::process::{GitProcessCommand, ProcessArgument};
use crate::types::{HeadState, ObjectId, RefId, RepositoryId, RepositoryPath, WorktreeId};
use crate::version::GitVersion;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::{fmt, fs, io};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryKind {
    Worktree,
    Bare,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryRequest {
    pub path: RepositoryPath,
    pub discover_parents: bool,
}

impl DiscoveryRequest {
    pub const fn new(path: RepositoryPath) -> Self {
        Self {
            path,
            discover_parents: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredRepository {
    pub worktree: WorktreeId,
    pub kind: RepositoryKind,
    pub head: HeadState,
    /// Version resolved once during discovery and reused by initial repository reads.
    pub git_version: GitVersion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiscoveryErrorKind {
    NotFound,
    AccessDenied,
    NotRepository,
    ParentDiscoveryDisabled,
    GitMissing,
    GitFailed,
    InvalidGitOutput,
    Canceled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryError {
    pub kind: DiscoveryErrorKind,
    message: String,
}

impl DiscoveryError {
    pub fn new(kind: DiscoveryErrorKind, message: impl Into<String>) -> Self {
        let mut message = message.into();
        while message.len() > 512 {
            message.pop();
        }
        Self { kind, message }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DiscoveryError {}

const OUTPUT_LIMIT: usize = 64 * 1024;

/// Discovers repository identity through installed Git, including linked worktrees.
#[derive(Clone, Debug)]
pub struct RepositoryDiscovery {
    executable: OsString,
}

impl Default for RepositoryDiscovery {
    fn default() -> Self {
        Self::new("git")
    }
}

impl RepositoryDiscovery {
    pub fn new(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    fn git(
        &self,
        directory: &Path,
        arguments: &[&OsStr],
        execute: &mut dyn FnMut(GitProcessCommand) -> ActivityRecord,
    ) -> ActivityRecord {
        let command = GitProcessCommand::git(self.executable.clone())
            .current_dir(directory)
            .args(
                arguments
                    .iter()
                    .map(|argument| ProcessArgument::visible(*argument)),
            )
            .output_limit(OUTPUT_LIMIT)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .expect("constant environment name is valid");
        execute(command)
    }

    fn required_output(
        &self,
        directory: &Path,
        arguments: &[&OsStr],
        execute: &mut dyn FnMut(GitProcessCommand) -> ActivityRecord,
    ) -> Result<Vec<u8>, DiscoveryError> {
        let record = self.git(directory, arguments, execute);
        match record.status {
            ActivityStatus::Succeeded { .. } if !record.stdout.truncated => {
                Ok(trim_record_terminator(record.stdout.bytes))
            }
            ActivityStatus::LaunchError { .. } => Err(DiscoveryError::new(
                DiscoveryErrorKind::GitMissing,
                "the installed Git executable could not be started",
            )),
            ActivityStatus::Succeeded { .. } => Err(DiscoveryError::new(
                DiscoveryErrorKind::InvalidGitOutput,
                "Git repository discovery output exceeded its bound",
            )),
            ActivityStatus::Canceled { .. } => Err(DiscoveryError::new(
                DiscoveryErrorKind::Canceled,
                "repository opening was canceled",
            )),
            _ => Err(DiscoveryError::new(
                DiscoveryErrorKind::NotRepository,
                "the selected path is not inside a Git repository",
            )),
        }
    }

    fn optional_output(
        &self,
        directory: &Path,
        arguments: &[&OsStr],
        execute: &mut dyn FnMut(GitProcessCommand) -> ActivityRecord,
    ) -> Result<Option<Vec<u8>>, DiscoveryError> {
        let record = self.git(directory, arguments, execute);
        match record.status {
            ActivityStatus::Succeeded { .. } if !record.stdout.truncated => {
                Ok(Some(trim_record_terminator(record.stdout.bytes)))
            }
            ActivityStatus::Succeeded { .. } => Err(DiscoveryError::new(
                DiscoveryErrorKind::InvalidGitOutput,
                "Git repository discovery output exceeded its bound",
            )),
            ActivityStatus::Failed { .. } => Ok(None),
            ActivityStatus::LaunchError { .. } => Err(DiscoveryError::new(
                DiscoveryErrorKind::GitMissing,
                "the installed Git executable could not be started",
            )),
            ActivityStatus::Canceled { .. } => Err(DiscoveryError::new(
                DiscoveryErrorKind::Canceled,
                "repository opening was canceled",
            )),
            _ => Err(DiscoveryError::new(
                DiscoveryErrorKind::GitFailed,
                "Git repository discovery did not complete",
            )),
        }
    }

    /// Resolves a file or directory to Git identity and HEAD.
    /// Every command is supplied to `execute`, allowing the consumer to own activity IDs
    /// and cancellation. No command bypasses that execution boundary.
    ///
    /// # Errors
    /// Returns a typed path, Git, cancellation, or parse failure.
    pub fn discover(
        &self,
        request: &DiscoveryRequest,
        execute: &mut dyn FnMut(GitProcessCommand) -> ActivityRecord,
    ) -> Result<DiscoveredRepository, DiscoveryError> {
        let requested = canonicalize_input(request.path.as_path())?;
        let start = if requested.is_file() {
            requested.parent().map(Path::to_path_buf).ok_or_else(|| {
                DiscoveryError::new(
                    DiscoveryErrorKind::NotRepository,
                    "the selected file has no containing directory",
                )
            })?
        } else {
            requested.clone()
        };

        let version = self.required_output(&start, &[os("--version")], execute)?;
        let git_version = GitVersion::parse(&version).ok_or_else(|| {
            DiscoveryError::new(
                DiscoveryErrorKind::InvalidGitOutput,
                "Git returned an invalid version",
            )
        })?;

        let identity = self.required_output(
            &start,
            &[
                os("rev-parse"),
                os("--path-format=absolute"),
                os("--absolute-git-dir"),
                os("--git-common-dir"),
                os("--is-bare-repository"),
            ],
            execute,
        )?;
        let (git_dir, common_dir, bare) = parse_repository_identity(&identity)?;
        let kind = match bare {
            b"true" => RepositoryKind::Bare,
            b"false" => RepositoryKind::Worktree,
            _ => {
                return Err(DiscoveryError::new(
                    DiscoveryErrorKind::InvalidGitOutput,
                    "Git returned an invalid bare-repository flag",
                ));
            }
        };
        let root = if kind == RepositoryKind::Bare {
            common_dir.clone()
        } else {
            self.git_path(
                &start,
                &[
                    os("rev-parse"),
                    os("--path-format=absolute"),
                    os("--show-toplevel"),
                ],
                execute,
            )?
        };
        if !request.discover_parents && requested != root {
            return Err(DiscoveryError::new(
                DiscoveryErrorKind::ParentDiscoveryDisabled,
                "the selected path is not the repository root",
            ));
        }

        let symbolic =
            self.optional_output(&start, &[os("symbolic-ref"), os("-q"), os("HEAD")], execute)?;
        let target = self.optional_output(
            &start,
            &[os("rev-parse"), os("--verify"), os("HEAD")],
            execute,
        )?;
        let head = parse_head(symbolic, target)?;
        let repository = RepositoryId::new(repository_path(common_dir)?);
        let worktree = WorktreeId::new(
            repository,
            repository_path(root)?,
            repository_path(git_dir)?,
        );
        Ok(DiscoveredRepository {
            worktree,
            kind,
            head,
            git_version,
        })
    }

    fn git_path(
        &self,
        directory: &Path,
        arguments: &[&OsStr],
        execute: &mut dyn FnMut(GitProcessCommand) -> ActivityRecord,
    ) -> Result<PathBuf, DiscoveryError> {
        let bytes = self.required_output(directory, arguments, execute)?;
        if bytes.is_empty() {
            return Err(DiscoveryError::new(
                DiscoveryErrorKind::InvalidGitOutput,
                "Git returned an empty repository path",
            ));
        }
        let path = bytes_to_path(bytes)?;
        fs::canonicalize(&path).map_err(|error| map_path_error(&error))
    }
}

fn parse_repository_identity(bytes: &[u8]) -> Result<(PathBuf, PathBuf, &[u8]), DiscoveryError> {
    let lines = bytes
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let [git_dir, common_dir, bare] = lines.as_slice() else {
        return Err(DiscoveryError::new(
            DiscoveryErrorKind::InvalidGitOutput,
            "Git returned incomplete repository identity output",
        ));
    };
    let git_dir = bytes_to_path((*git_dir).to_vec())?;
    let common_dir = bytes_to_path((*common_dir).to_vec())?;
    let git_dir = fs::canonicalize(git_dir).map_err(|error| map_path_error(&error))?;
    let common_dir = fs::canonicalize(common_dir).map_err(|error| map_path_error(&error))?;
    Ok((git_dir, common_dir, bare))
}

fn parse_head(
    symbolic: Option<Vec<u8>>,
    target: Option<Vec<u8>>,
) -> Result<HeadState, DiscoveryError> {
    let target = target
        .map(|bytes| String::from_utf8(bytes).map_err(|_| invalid_head()))
        .transpose()?
        .map(ObjectId::parse)
        .transpose()
        .map_err(|_| invalid_head())?;
    match (symbolic, target) {
        (Some(branch), Some(target)) => Ok(HeadState::Attached {
            branch: RefId::from_bytes(branch).map_err(|_| invalid_head())?,
            target,
        }),
        (Some(branch), None) => Ok(HeadState::Unborn {
            branch: RefId::from_bytes(branch).map_err(|_| invalid_head())?,
        }),
        (None, Some(target)) => Ok(HeadState::Detached { target }),
        (None, None) => Err(invalid_head()),
    }
}

fn invalid_head() -> DiscoveryError {
    DiscoveryError::new(
        DiscoveryErrorKind::InvalidGitOutput,
        "Git returned an invalid HEAD state",
    )
}

fn canonicalize_input(path: &Path) -> Result<PathBuf, DiscoveryError> {
    fs::canonicalize(path).map_err(|error| map_path_error(&error))
}

fn map_path_error(error: &io::Error) -> DiscoveryError {
    let kind = match error.kind() {
        io::ErrorKind::NotFound => DiscoveryErrorKind::NotFound,
        io::ErrorKind::PermissionDenied => DiscoveryErrorKind::AccessDenied,
        _ => DiscoveryErrorKind::GitFailed,
    };
    DiscoveryError::new(
        kind,
        format!("repository path could not be resolved: {error}"),
    )
}

fn repository_path(path: PathBuf) -> Result<RepositoryPath, DiscoveryError> {
    RepositoryPath::new(path).map_err(|_| invalid_head())
}

fn trim_record_terminator(mut bytes: Vec<u8>) -> Vec<u8> {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    bytes
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)] // The cross-platform helper is fallible on Windows.
fn bytes_to_path(bytes: Vec<u8>) -> Result<PathBuf, DiscoveryError> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: Vec<u8>) -> Result<PathBuf, DiscoveryError> {
    String::from_utf8(bytes).map(PathBuf::from).map_err(|_| {
        DiscoveryError::new(
            DiscoveryErrorKind::InvalidGitOutput,
            "Git returned a non-UTF-8 repository path",
        )
    })
}

fn os(value: &'static str) -> &'static OsStr {
    OsStr::new(value)
}
