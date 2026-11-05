//! Bounded filtered Git-blob and worktree-file reads, independent of decoding or UI.
use super::{working_diff::DiffEndpoint, CommandExecutor};
use crate::{
    activity::ActivityStatus,
    cancellation::Cancellation,
    diff::model::DiffPath,
    process::{GitProcessCommand, ProcessArgument},
    types::WorktreeId,
};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::Read as _,
    path::PathBuf,
};
pub const MAX_CONTENT_BYTES: usize = 64 * 1024 * 1024;
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContentOrigin {
    GitObject,
    RepositoryFile(DiffPath),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentBytes {
    pub bytes: Vec<u8>,
    pub origin: ContentOrigin,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentError {
    Canceled,
    InvalidPath,
    Missing,
    TooLarge,
    GitFailed,
}
impl std::fmt::Display for ContentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Canceled => "content read canceled",
            Self::InvalidPath => "invalid repository content path",
            Self::Missing => "repository content is unavailable",
            Self::TooLarge => "repository content exceeded its byte bound",
            Self::GitFailed => "Git content read failed",
        })
    }
}
impl std::error::Error for ContentError {}
pub struct ContentRequest<'a> {
    pub worktree: &'a WorktreeId,
    pub endpoint: &'a DiffEndpoint,
    pub path: &'a DiffPath,
    pub byte_limit: usize,
}
#[derive(Clone, Debug)]
pub struct ContentLoader {
    executable: OsString,
}
impl ContentLoader {
    pub fn new(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
        }
    }
    /// Reads one exact content side, applying Git filters to object data.
    /// # Errors
    /// Returns cancellation, path, availability, byte-bound or Git failures.
    pub fn read(
        &self,
        request: &ContentRequest<'_>,
        cancellation: &dyn Cancellation,
        executor: &mut dyn CommandExecutor,
    ) -> Result<Option<ContentBytes>, ContentError> {
        if cancellation.is_cancelled() {
            return Err(ContentError::Canceled);
        }
        if request.byte_limit > MAX_CONTENT_BYTES {
            return Err(ContentError::TooLarge);
        }
        if matches!(request.endpoint, DiffEndpoint::EmptyTree) {
            return Ok(None);
        }
        if matches!(request.endpoint, DiffEndpoint::WorkingTree) {
            return read_worktree(request, cancellation).map(Some);
        }
        let mut object = match request.endpoint {
            DiffEndpoint::Head => OsString::from("HEAD"),
            DiffEndpoint::Index => OsString::new(),
            DiffEndpoint::Commit(object) => OsString::from(object.as_str()),
            DiffEndpoint::EmptyTree | DiffEndpoint::WorkingTree => unreachable!(),
        };
        object.push(OsStr::new(":"));
        object.push(path_os(request.path)?);
        let command = GitProcessCommand::git(self.executable.clone())
            .arg(ProcessArgument::visible("--no-pager"))
            .arg(ProcessArgument::visible("cat-file"))
            .arg(ProcessArgument::visible("--filters"))
            .arg(ProcessArgument::redacted(object, "[OBJECT:PATH]"))
            .current_dir(request.worktree.worktree_root().as_path())
            .redact_output()
            .output_limit(request.byte_limit.saturating_add(1));
        let mut result = executor.execute(command);
        if cancellation.is_cancelled()
            || matches!(result.activity.status, ActivityStatus::Canceled { .. })
        {
            return Err(ContentError::Canceled);
        }
        if !matches!(result.activity.status, ActivityStatus::Succeeded { .. }) {
            return Err(ContentError::GitFailed);
        }
        if result.stdout.truncated {
            return Err(ContentError::TooLarge);
        }
        let bytes = result.stdout.take_bytes();
        if bytes.len() > request.byte_limit {
            return Err(ContentError::TooLarge);
        }
        Ok(Some(ContentBytes {
            bytes,
            origin: ContentOrigin::GitObject,
        }))
    }
}
impl ContentLoader {
    /// Reads the exact stored bytes of blob `object`: no filters, textconv,
    /// smudge or external commands. Fails for a missing object or a non-blob.
    /// # Errors
    /// Returns cancellation, availability, byte-bound or Git failures.
    pub fn read_raw_blob(
        &self,
        worktree: &WorktreeId,
        object: &crate::types::ObjectId,
        byte_limit: usize,
        cancellation: &dyn Cancellation,
        executor: &mut dyn CommandExecutor,
    ) -> Result<Vec<u8>, ContentError> {
        if cancellation.is_cancelled() {
            return Err(ContentError::Canceled);
        }
        if byte_limit > MAX_CONTENT_BYTES {
            return Err(ContentError::TooLarge);
        }
        let command = GitProcessCommand::git(self.executable.clone())
            .arg(ProcessArgument::visible("--no-pager"))
            .arg(ProcessArgument::visible("cat-file"))
            .arg(ProcessArgument::visible("blob"))
            .arg(ProcessArgument::redacted(object.as_str(), "[OBJECT]"))
            .current_dir(worktree.worktree_root().as_path())
            .redact_output()
            .output_limit(byte_limit.saturating_add(1));
        let mut result = executor.execute(command);
        if cancellation.is_cancelled()
            || matches!(result.activity.status, ActivityStatus::Canceled { .. })
        {
            return Err(ContentError::Canceled);
        }
        if !matches!(result.activity.status, ActivityStatus::Succeeded { .. }) {
            return Err(ContentError::Missing);
        }
        if result.stdout.truncated {
            return Err(ContentError::TooLarge);
        }
        let bytes = result.stdout.take_bytes();
        if bytes.len() > byte_limit {
            return Err(ContentError::TooLarge);
        }
        Ok(bytes)
    }

    /// Reads the exact bytes of a regular worktree file. Symlinks are never
    /// followed and no Git filter runs.
    /// # Errors
    /// Returns cancellation, path, availability or byte-bound failures.
    pub fn read_worktree_file(
        worktree: &WorktreeId,
        path: &DiffPath,
        byte_limit: usize,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<u8>, ContentError> {
        if byte_limit > MAX_CONTENT_BYTES {
            return Err(ContentError::TooLarge);
        }
        read_worktree(
            &ContentRequest {
                worktree,
                endpoint: &DiffEndpoint::WorkingTree,
                path,
                byte_limit,
            },
            cancellation,
        )
        .map(|content| content.bytes)
    }
}

fn read_worktree(
    request: &ContentRequest<'_>,
    cancellation: &dyn Cancellation,
) -> Result<ContentBytes, ContentError> {
    let mut file = open_worktree_file(request)?;
    let metadata = file.metadata().map_err(|_| ContentError::Missing)?;
    if !metadata.is_file() {
        return Err(ContentError::InvalidPath);
    }
    if metadata.len() > request.byte_limit as u64 {
        return Err(ContentError::TooLarge);
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(request.byte_limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ContentError::Missing)?;
    if bytes.len() > request.byte_limit {
        return Err(ContentError::TooLarge);
    }
    if cancellation.is_cancelled() {
        return Err(ContentError::Canceled);
    }
    Ok(ContentBytes {
        bytes,
        origin: ContentOrigin::RepositoryFile(request.path.clone()),
    })
}

fn open_worktree_file(request: &ContentRequest<'_>) -> Result<File, ContentError> {
    let relative = PathBuf::from(path_os(request.path)?);
    let root = request.worktree.worktree_root().as_path();
    #[cfg(windows)]
    {
        crate::filesystem::worktree_probe::open_regular_file(root, &relative).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ContentError::Missing
            } else {
                ContentError::InvalidPath
            }
        })
    }
    #[cfg(not(windows))]
    {
        use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
        let directory = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())
            .map_err(|_| ContentError::Missing)?;
        let before = directory
            .symlink_metadata(&relative)
            .map_err(|_| ContentError::Missing)?;
        if !before.is_file() || before.file_type().is_symlink() {
            return Err(ContentError::InvalidPath);
        }
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.custom_flags(
                i32::try_from(rustix::fs::OFlags::NONBLOCK.bits())
                    .map_err(|_| ContentError::InvalidPath)?,
            );
        }
        let file = directory
            .open_with(&relative, &options)
            .map_err(|_| ContentError::InvalidPath)?;
        let opened = file.metadata().map_err(|_| ContentError::Missing)?;
        if !opened.is_file() || !crate::filesystem::same_cap_metadata(&before, &opened) {
            return Err(ContentError::InvalidPath);
        }
        Ok(file.into_std())
    }
}
#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn path_os(path: &DiffPath) -> Result<OsString, ContentError> {
    use std::os::unix::ffi::OsStringExt as _;
    Ok(OsString::from_vec(path.as_bytes().to_vec()))
}
#[cfg(not(unix))]
fn path_os(path: &DiffPath) -> Result<OsString, ContentError> {
    String::from_utf8(path.as_bytes().to_vec())
        .map(OsString::from)
        .map_err(|_| ContentError::InvalidPath)
}
