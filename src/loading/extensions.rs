//! Bounded worktree, submodule and Git LFS inventory reads.
use super::CommandExecutor;
use crate::activity::{ActivityRecord, ActivityStatus};
use crate::cancellation::Cancellation;
use crate::process::{GitProcessCommand, ProcessArgument, SemanticTimeout};
use crate::types::{
    ObjectId, RefId, RepositoryId, RepositoryPath, RepositoryRelativePath, WorktreeId,
};
use std::{
    ffi::OsString,
    fmt, fs,
    path::{Path, PathBuf},
    time::Duration,
};
const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const POINTER_LIMIT: u64 = 4 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_mins(2);
pub const MAX_SUBMODULES: usize = 4_096;
pub const MAX_EXTENSION_DETAIL_BYTES: usize = 512;
pub const MAX_BRANCH_NAME_BYTES: usize = 255;
pub const MAX_WORKTREES: usize = 1_024;
pub const MAX_LFS_ITEMS: usize = 4_096;
pub const MAX_LFS_PATTERN_BYTES: usize = 1_024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmoduleCheckoutState {
    AtRecordedCommit,
    DifferentCommit,
    Uninitialized,
    Conflicted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmoduleStatus {
    pub path: RepositoryRelativePath,
    pub commit: ObjectId,
    pub checkout: SubmoduleCheckoutState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmoduleStatusResult {
    pub submodules: Vec<SubmoduleStatus>,
    pub activities: Vec<ActivityRecord>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryExtensionErrorKind {
    InvalidRequest,
    UnsupportedRemote,
    GitFailed,
    Canceled,
    InvalidOutput,
    TooLarge,
    Unavailable,
    UnsafeTarget,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedWorktree {
    pub identity: WorktreeId,
    pub head: Option<ObjectId>,
    pub branch: Option<RefId>,
    pub locked: bool,
    pub prunable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeInventory {
    pub worktrees: Vec<ManagedWorktree>,
    pub activities: Vec<ActivityRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LfsPathState {
    Pointer,
    Materialized,
    Missing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LfsStatusEntry {
    pub path: RepositoryRelativePath,
    pub state: LfsPathState,
    pub pointer: Option<LfsPointerPreview>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LfsStatusResult {
    pub available: bool,
    pub unavailable_reason: String,
    pub entries: Vec<LfsStatusEntry>,
    pub activities: Vec<ActivityRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LfsPointerPreview {
    pub oid_sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryExtensionError {
    kind: RepositoryExtensionErrorKind,
    detail: String,
}

impl RepositoryExtensionError {
    #[must_use]
    pub fn new(kind: RepositoryExtensionErrorKind, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        if detail.is_empty() {
            detail.push_str("repository extension failed");
        }
        if detail.len() > MAX_EXTENSION_DETAIL_BYTES {
            let mut boundary = MAX_EXTENSION_DETAIL_BYTES;
            while !detail.is_char_boundary(boundary) {
                boundary -= 1;
            }
            detail.truncate(boundary);
        }
        Self { kind, detail }
    }

    #[must_use]
    pub const fn kind(&self) -> RepositoryExtensionErrorKind {
        self.kind
    }

    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for RepositoryExtensionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for RepositoryExtensionError {}

pub struct ExtensionLoader<'a> {
    executable: OsString,
    executor: &'a mut dyn CommandExecutor,
}
impl<'a> ExtensionLoader<'a> {
    pub fn new(executable: impl Into<OsString>, executor: &'a mut dyn CommandExecutor) -> Self {
        Self {
            executable: executable.into(),
            executor,
        }
    }
    fn command(&self, worktree: &WorktreeId, arguments: Vec<ProcessArgument>) -> GitProcessCommand {
        GitProcessCommand::git(self.executable.clone())
            .current_dir(worktree.worktree_root().as_path())
            .args(arguments)
            .output_limit(OUTPUT_LIMIT)
            .output_idle_timeout(
                SemanticTimeout::new(
                    COMMAND_TIMEOUT,
                    "Git produced no output for 2 minutes during repository management",
                )
                .unwrap(),
            )
            .env("LC_ALL", "C")
            .unwrap()
    }

    fn run_private(
        &mut self,
        command: GitProcessCommand,
        cancellation: &dyn Cancellation,
        activities: &mut Vec<ActivityRecord>,
    ) -> Result<Vec<u8>, RepositoryExtensionError> {
        if cancellation.is_cancelled() {
            return Err(RepositoryExtensionError::new(
                RepositoryExtensionErrorKind::Canceled,
                "repository read was canceled",
            ));
        }
        let mut result = self.executor.execute(command);
        let truncated = result.stdout.truncated || result.stderr.truncated;
        let bytes = result.stdout.take_bytes();
        activities.push(result.activity.clone());
        map_status(&result.activity, bytes, truncated, activities)
    }

    fn lfs_available(
        &mut self,
        worktree: &WorktreeId,
        cancellation: &dyn Cancellation,
        activities: &mut Vec<ActivityRecord>,
    ) -> Result<bool, RepositoryExtensionError> {
        let command = self
            .command(worktree, visible(&["lfs", "version"]))
            .redact_output();
        match self.run_private(command, cancellation, activities) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == RepositoryExtensionErrorKind::GitFailed => Ok(false),
            Err(error) => Err(error),
        }
    }
    /// Loads bounded worktree inventory, reusing the current worktree's discovered Git directory.
    /// # Errors
    /// Returns cancellation, Git, path or output-bound failures.
    pub fn worktrees(
        &mut self,
        worktree: &WorktreeId,
        cancellation: &dyn Cancellation,
    ) -> Result<WorktreeInventory, RepositoryExtensionError> {
        let mut activities = Vec::new();
        let raw = self.run_private(
            self.command(
                worktree,
                visible(&["worktree", "list", "--porcelain", "-z"]),
            )
            .redact_output(),
            cancellation,
            &mut activities,
        )?;
        let parsed = parse_worktree_list(&raw)?;
        let mut worktrees = Vec::with_capacity(parsed.len());
        for parsed in parsed {
            let git_dir = if parsed.path == worktree.worktree_root().as_path() {
                worktree.git_dir().clone()
            } else {
                let absolute_git_dir = self.run_private(
                    GitProcessCommand::git(self.executable.clone())
                        .current_dir(&parsed.path)
                        .args(visible(&["rev-parse", "--absolute-git-dir"]))
                        .output_limit(16 * 1024)
                        .redact_output(),
                    cancellation,
                    &mut activities,
                )?;
                RepositoryPath::new(parse_absolute_path(&absolute_git_dir)?)
                    .map_err(|_| invalid("Git returned an unsafe worktree Git directory"))?
            };
            let root = RepositoryPath::new(&parsed.path)
                .map_err(|_| invalid("Git returned an unsafe worktree path"))?;
            worktrees.push(ManagedWorktree {
                identity: WorktreeId::new(worktree.repository().clone(), root, git_dir),
                head: parsed.head,
                branch: parsed.branch,
                locked: parsed.locked,
                prunable: parsed.prunable,
            });
        }
        Ok(WorktreeInventory {
            worktrees,
            activities,
        })
    }

    /// Resolves an initialized submodule's worktree and common Git directories.
    /// # Errors
    /// Rejects absent or unsafe directories, cancellation and failed Git queries.
    pub fn enter_submodule(
        &mut self,
        worktree: &WorktreeId,
        path: &RepositoryRelativePath,
        cancellation: &dyn Cancellation,
    ) -> Result<WorktreeId, RepositoryExtensionError> {
        let root = worktree.worktree_root().as_path().join(path.as_path());
        let metadata =
            fs::symlink_metadata(&root).map_err(|_| invalid("submodule is not initialized"))?;
        if !metadata.file_type().is_dir() {
            return Err(invalid("submodule root is not a regular directory"));
        }
        let mut activities = Vec::new();
        let git_dir = self.run_private(
            GitProcessCommand::git(self.executable.clone())
                .current_dir(&root)
                .args(visible(&["rev-parse", "--absolute-git-dir"]))
                .output_limit(16 * 1024)
                .redact_output(),
            cancellation,
            &mut activities,
        )?;
        let common = self.run_private(
            GitProcessCommand::git(self.executable.clone())
                .current_dir(&root)
                .args(visible(&[
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-common-dir",
                ]))
                .output_limit(16 * 1024)
                .redact_output(),
            cancellation,
            &mut activities,
        )?;
        let root = RepositoryPath::new(root).map_err(|_| invalid("submodule root is unsafe"))?;
        let git_dir = RepositoryPath::new(parse_absolute_path(&git_dir)?)
            .map_err(|_| invalid("submodule Git directory is unsafe"))?;
        let common = RepositoryPath::new(parse_absolute_path(&common)?)
            .map_err(|_| invalid("submodule common Git directory is unsafe"))?;
        Ok(WorktreeId::new(RepositoryId::new(common), root, git_dir))
    }

    /// Loads LFS availability and bounded tracked-file materialization state.
    /// # Errors
    /// Returns cancellation, process, pointer and output-bound failures.
    pub fn lfs_status(
        &mut self,
        worktree: &WorktreeId,
        cancellation: &dyn Cancellation,
    ) -> Result<LfsStatusResult, RepositoryExtensionError> {
        let mut activities = Vec::new();
        if !self.lfs_available(worktree, cancellation, &mut activities)? {
            return Ok(LfsStatusResult {
                available: false,
                unavailable_reason: "git-lfs is not installed".to_owned(),
                entries: Vec::new(),
                activities,
            });
        }
        let raw = self.run_private(
            self.command(worktree, visible(&["lfs", "ls-files", "--json"]))
                .redact_output(),
            cancellation,
            &mut activities,
        )?;
        let paths = parse_lfs_json(&raw)?;
        let mut entries = Vec::with_capacity(paths.len());
        for path in paths {
            let absolute = worktree.worktree_root().as_path().join(path.as_path());
            let pointer = if absolute.exists() {
                read_pointer(&absolute)?
            } else {
                None
            };
            let state = if !absolute.exists() {
                LfsPathState::Missing
            } else if pointer.is_some() {
                LfsPathState::Pointer
            } else {
                LfsPathState::Materialized
            };
            entries.push(LfsStatusEntry {
                path,
                state,
                pointer,
            });
        }
        Ok(LfsStatusResult {
            available: true,
            unavailable_reason: String::new(),
            entries,
            activities,
        })
    }

    /// Reads a bounded pointer preview from a regular working-tree file.
    /// # Errors
    /// Rejects inspection failures and malformed pointer text.
    pub fn lfs_pointer_preview(
        &mut self,
        worktree: &WorktreeId,
        path: &RepositoryRelativePath,
    ) -> Result<Option<LfsPointerPreview>, RepositoryExtensionError> {
        read_pointer(&worktree.worktree_root().as_path().join(path.as_path()))
    }

    /// Loads recursive submodule checkout state with native path bytes.
    /// # Errors
    /// Returns cancellation, Git and bounded-output parsing failures.
    /// # Panics
    /// Only if a constant environment name becomes invalid.
    pub fn submodules(
        &mut self,
        worktree: &WorktreeId,
        cancellation: &dyn Cancellation,
    ) -> Result<SubmoduleStatusResult, RepositoryExtensionError> {
        let command = GitProcessCommand::git(self.executable.clone())
            .current_dir(worktree.worktree_root().as_path())
            .args([
                ProcessArgument::visible("-c"),
                ProcessArgument::visible("core.quotePath=true"),
                ProcessArgument::visible("submodule"),
                ProcessArgument::visible("status"),
                ProcessArgument::visible("--recursive"),
            ])
            .output_limit(OUTPUT_LIMIT)
            .env("LC_ALL", "C")
            .expect("constant environment name is valid")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .expect("constant environment name is valid");
        if cancellation.is_cancelled() {
            return Err(RepositoryExtensionError::new(
                RepositoryExtensionErrorKind::Canceled,
                "submodule status was canceled",
            ));
        }
        let record = self.executor.execute(command).activity;
        if record.stdout.truncated || record.stderr.truncated {
            return Err(RepositoryExtensionError::new(
                RepositoryExtensionErrorKind::TooLarge,
                "submodule status output exceeds its configured bound",
            ));
        }
        match &record.status {
            ActivityStatus::Succeeded { .. } => {
                let submodules = parse_submodule_status(&record.stdout.bytes)?;
                Ok(SubmoduleStatusResult {
                    submodules,
                    activities: vec![record],
                })
            }
            ActivityStatus::Canceled { .. } => Err(RepositoryExtensionError::new(
                RepositoryExtensionErrorKind::Canceled,
                "submodule status was canceled",
            )),
            ActivityStatus::LaunchError { message } => Err(RepositoryExtensionError::new(
                RepositoryExtensionErrorKind::GitFailed,
                format!("Git could not be launched: {message}"),
            )),
            _ => Err(RepositoryExtensionError::new(
                RepositoryExtensionErrorKind::GitFailed,
                bounded_stderr(&record.stderr.bytes),
            )),
        }
    }
}
/// Parses bounded worktree inventory with typed read errors.
/// # Errors
/// Rejects malformed or oversized inventory output.
pub fn parse_worktree_list(
    raw: &[u8],
) -> Result<Vec<crate::refs::WorktreeListEntry>, RepositoryExtensionError> {
    crate::refs::parse_worktree_list(raw, MAX_WORKTREES).map_err(|error| {
        if error == crate::refs::WorktreeListError::TooManyWorktrees {
            too_large(&error.to_string())
        } else {
            invalid(&error.to_string())
        }
    })
}

/// Parses bounded tracked-file JSON with native safe relative paths.
/// # Errors
/// Rejects malformed JSON, unsafe paths and too many entries.
pub fn parse_lfs_json(raw: &[u8]) -> Result<Vec<RepositoryRelativePath>, RepositoryExtensionError> {
    crate::lfs::parse_files_json(raw, MAX_LFS_ITEMS).map_err(|error| {
        if error == crate::lfs::LfsFilesError::TooManyItems {
            too_large(&error.to_string())
        } else {
            invalid(&error.to_string())
        }
    })
}

fn read_pointer(path: &Path) -> Result<Option<LfsPointerPreview>, RepositoryExtensionError> {
    use std::io::Read;
    let metadata = match fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(invalid("LFS path cannot be inspected")),
    };
    if !metadata.file_type().is_file() || metadata.len() > POINTER_LIMIT {
        return Ok(None);
    }
    let file = open_pointer(path).map_err(|_| invalid("LFS pointer cannot be opened"))?;
    if !file
        .metadata()
        .map_err(|_| invalid("LFS pointer cannot be inspected"))?
        .is_file()
    {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(POINTER_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid("LFS pointer cannot be read"))?;
    if bytes.len() as u64 > POINTER_LIMIT {
        return Ok(None);
    }
    let text = String::from_utf8(bytes).map_err(|_| invalid("LFS pointer is not UTF-8"))?;
    parse_pointer(&text)
}

#[cfg(unix)]
fn open_pointer(path: &Path) -> std::io::Result<fs::File> {
    use rustix::fs::{Mode, OFlags};
    rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(fs::File::from)
    .map_err(std::io::Error::from)
}
#[cfg(not(unix))]
fn open_pointer(path: &Path) -> std::io::Result<fs::File> {
    fs::File::open(path)
}

/// Recognizes a strict Git LFS pointer, or an ordinary text file.
/// # Errors
/// Rejects malformed pointer identities and sizes.
pub fn parse_pointer(text: &str) -> Result<Option<LfsPointerPreview>, RepositoryExtensionError> {
    crate::lfs::LfsPointer::parse(text)
        .map(|pointer| {
            pointer.map(|pointer| LfsPointerPreview {
                oid_sha256: pointer.oid_sha256,
                size: pointer.size,
            })
        })
        .map_err(|error| invalid(&error.to_string()))
}

fn map_status(
    record: &ActivityRecord,
    bytes: Vec<u8>,
    truncated: bool,
    activities: &[ActivityRecord],
) -> Result<Vec<u8>, RepositoryExtensionError> {
    if truncated {
        return Err(too_large("repository management output exceeds its bound"));
    }
    match record.status {
        ActivityStatus::Succeeded { .. } => Ok(bytes),
        ActivityStatus::Canceled { .. } => Err(RepositoryExtensionError::new(
            RepositoryExtensionErrorKind::Canceled,
            "repository management was canceled",
        )),
        _ => Err(RepositoryExtensionError::new(
            RepositoryExtensionErrorKind::GitFailed,
            format!(
                "installed Git command failed after {} recorded step(s)",
                activities.len()
            ),
        )),
    }
}

fn visible(values: &[&str]) -> Vec<ProcessArgument> {
    values.iter().map(ProcessArgument::visible).collect()
}
fn parse_absolute_path(value: &[u8]) -> Result<PathBuf, RepositoryExtensionError> {
    let value = value.strip_suffix(b"\n").unwrap_or(value);
    let path = native_path(value)?;
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(invalid("Git returned a relative path"))
    }
}
#[cfg(unix)]
fn native_path(value: &[u8]) -> Result<PathBuf, RepositoryExtensionError> {
    use std::os::unix::ffi::OsStringExt;
    if value.contains(&0) {
        Err(invalid("path contains NUL"))
    } else {
        Ok(PathBuf::from(OsString::from_vec(value.to_vec())))
    }
}
#[cfg(not(unix))]
fn native_path(value: &[u8]) -> Result<PathBuf, RepositoryExtensionError> {
    std::str::from_utf8(value)
        .map(PathBuf::from)
        .map_err(|_| invalid("path is not UTF-8"))
}
fn invalid(detail: &str) -> RepositoryExtensionError {
    RepositoryExtensionError::new(RepositoryExtensionErrorKind::InvalidOutput, detail)
}
fn too_large(detail: &str) -> RepositoryExtensionError {
    RepositoryExtensionError::new(RepositoryExtensionErrorKind::TooLarge, detail)
}

fn parse_submodule_status(bytes: &[u8]) -> Result<Vec<SubmoduleStatus>, RepositoryExtensionError> {
    use crate::submodule::{SubmoduleCheckoutState as Checkout, SubmoduleParseErrorKind};
    crate::submodule::parse_submodule_status(bytes, MAX_SUBMODULES)
        .map(|statuses| {
            statuses
                .into_iter()
                .map(|status| SubmoduleStatus {
                    path: status.path,
                    commit: status.commit,
                    checkout: match status.checkout {
                        Checkout::AtRecordedCommit => SubmoduleCheckoutState::AtRecordedCommit,
                        Checkout::DifferentCommit => SubmoduleCheckoutState::DifferentCommit,
                        Checkout::Uninitialized => SubmoduleCheckoutState::Uninitialized,
                        Checkout::Conflicted => SubmoduleCheckoutState::Conflicted,
                    },
                })
                .collect()
        })
        .map_err(|error| {
            RepositoryExtensionError::new(
                match error.kind() {
                    SubmoduleParseErrorKind::InvalidOutput => {
                        RepositoryExtensionErrorKind::InvalidOutput
                    }
                    SubmoduleParseErrorKind::TooLarge => RepositoryExtensionErrorKind::TooLarge,
                },
                error.to_string(),
            )
        })
}

fn bounded_stderr(bytes: &[u8]) -> String {
    let value = String::from_utf8_lossy(bytes);
    let value = value.trim();
    if value.is_empty() {
        "Git submodule status failed".to_owned()
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pointer_preview_is_bounded_and_never_reads_symlinks() {
        let root = std::env::temp_dir().join(format!("devmil-lfs-preview-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let pointer = root.join("pointer");
        fs::write(
            &pointer,
            format!(
                "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 42\n",
                "1".repeat(64)
            ),
        )
        .unwrap();
        assert_eq!(read_pointer(&pointer).unwrap().unwrap().size, 42);
        let large = root.join("large");
        fs::write(&large, vec![0; 4097]).unwrap();
        assert!(read_pointer(&large).unwrap().is_none());
        #[cfg(unix)]
        {
            let link = root.join("link");
            std::os::unix::fs::symlink(&pointer, &link).unwrap();
            assert!(read_pointer(&link).unwrap().is_none());
        }
        fs::remove_dir_all(root).unwrap();
    }
}
