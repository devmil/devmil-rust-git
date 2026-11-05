//! Index changes staged on a private copy and installed under Git's `index.lock` protocol.
//!
//! An [`IndexTransaction`] copies the expected index bytes into a temporary file inside the
//! worktree's Git directory, lets Git commands operate on that copy through `GIT_INDEX_FILE`, and
//! finally installs the result only if the real index still holds the expected bytes. Git's own
//! lock file is taken with exclusive creation, so a concurrent Git process either holds the lock
//! (installation fails with [`IndexTransactionError::Locked`]) or sees the complete new index.
//!
//! Commands run on the private copy with `core.splitIndex=false`, so the installed index never
//! depends on a shared-index file written for the copy. The private path handed to Git has a
//! Windows verbatim (`\\?\`) prefix removed, which Git for Windows does not understand.
//!
//! The caller owns preconditions beyond the index bytes (worktree fingerprints, `HEAD`), the
//! mutation lease and refresh after installation.

use crate::commands::invocation::{CommandArgument, GitInvocation};
use std::fmt;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Largest index the transaction reads, in bytes.
pub const MAX_INDEX_TRANSACTION_BYTES: u64 = 128 * 1024 * 1024;

static NEXT_TRANSACTION: AtomicU64 = AtomicU64::new(1);

/// Why a transaction could not be prepared or installed. Values carry no paths.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum IndexTransactionError {
    /// The index or a temporary file is a link, not a file, or larger than the bound.
    UnsafeFile,
    /// Another process holds `index.lock`.
    Locked,
    /// The real index no longer holds the expected bytes.
    Changed,
    /// The private copy is missing after the Git commands ran.
    Missing,
    Io,
}

impl fmt::Display for IndexTransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnsafeFile => "the index is not a regular file within the size bound",
            Self::Locked => "another Git process holds the index lock",
            Self::Changed => "the index changed before the transaction was installed",
            Self::Missing => "the private index is missing",
            Self::Io => "the index transaction failed to read or write a file",
        })
    }
}

impl std::error::Error for IndexTransactionError {}

fn io_error(error: &std::io::Error) -> IndexTransactionError {
    match error.kind() {
        std::io::ErrorKind::AlreadyExists => IndexTransactionError::Locked,
        _ => IndexTransactionError::Io,
    }
}

/// Reads an index file with the transaction's bound. A missing file is `Ok(None)`.
///
/// # Errors
/// Rejects links, non-files and oversized files; reports other I/O failures.
pub fn read_index_bytes(path: &Path) -> Result<Option<Vec<u8>>, IndexTransactionError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(IndexTransactionError::Io),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_INDEX_TRANSACTION_BYTES
    {
        return Err(IndexTransactionError::UnsafeFile);
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| {
            file.take(MAX_INDEX_TRANSACTION_BYTES + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|_| IndexTransactionError::Io)?;
    if bytes.len() as u64 > MAX_INDEX_TRANSACTION_BYTES {
        return Err(IndexTransactionError::UnsafeFile);
    }
    Ok(Some(bytes))
}

/// A private index copy that is installed or discarded. Dropping it removes the copy.
#[derive(Debug)]
pub struct IndexTransaction {
    git_dir: PathBuf,
    temporary: PathBuf,
    expected: Option<Vec<u8>>,
}

impl IndexTransaction {
    /// Starts a transaction expecting the index to hold `expected` (`None`: no index exists).
    /// Pass the bytes captured when the change was previewed, so a change in between fails at
    /// installation. The copy is written with exclusive creation inside `git_dir`.
    ///
    /// # Errors
    /// Reports I/O failures creating the private copy.
    pub fn begin(git_dir: &Path, expected: Option<Vec<u8>>) -> Result<Self, IndexTransactionError> {
        let temporary = loop {
            let candidate = git_dir.join(format!(
                "devmil-git-index-{}-{}",
                std::process::id(),
                NEXT_TRANSACTION.fetch_add(1, Ordering::Relaxed)
            ));
            if std::fs::symlink_metadata(&candidate).is_err() {
                break candidate;
            }
        };
        if let Some(bytes) = &expected {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|error| io_error(&error))?;
            let written = file.write_all(bytes).and_then(|()| file.sync_all());
            if written.is_err() {
                drop(file);
                let _ = std::fs::remove_file(&temporary);
                return Err(IndexTransactionError::Io);
            }
        }
        Ok(Self {
            git_dir: git_dir.to_path_buf(),
            temporary,
            expected,
        })
    }

    /// Starts a transaction expecting the index's current bytes.
    ///
    /// # Errors
    /// As [`read_index_bytes`] and [`Self::begin`].
    pub fn begin_current(git_dir: &Path) -> Result<Self, IndexTransactionError> {
        Self::begin(git_dir, read_index_bytes(&git_dir.join("index"))?)
    }

    /// Whether no index existed, so the caller must first run
    /// [`crate::commands::index::read_tree_empty`] through [`Self::wrap`].
    #[must_use]
    pub const fn starts_empty(&self) -> bool {
        self.expected.is_none()
    }

    /// The private index path as Git should receive it.
    #[must_use]
    pub fn index_path(&self) -> PathBuf {
        git_compatible_index_path(&self.temporary)
    }

    /// Runs `invocation` against the private copy: prepends `-c core.splitIndex=false` and sets
    /// a redacted `GIT_INDEX_FILE`.
    #[must_use]
    pub fn wrap(&self, invocation: GitInvocation) -> GitInvocation {
        invocation
            .with_leading_arguments(vec![
                CommandArgument::visible("-c"),
                CommandArgument::visible("core.splitIndex=false"),
            ])
            .env_redacted("GIT_INDEX_FILE", self.index_path().into_os_string())
    }

    /// The private copy's current bytes.
    ///
    /// # Errors
    /// As [`read_index_bytes`]; a missing copy is [`IndexTransactionError::Missing`].
    pub fn private_bytes(&self) -> Result<Vec<u8>, IndexTransactionError> {
        read_index_bytes(&self.temporary)?.ok_or(IndexTransactionError::Missing)
    }

    /// Installs the private copy as the worktree's index: creates `index.lock` exclusively,
    /// verifies the real index still holds the expected bytes, writes and syncs the lock, and
    /// renames it over `index`. On failure the lock is removed and the index is unchanged.
    ///
    /// # Errors
    /// [`IndexTransactionError::Locked`] when the lock exists, [`IndexTransactionError::Changed`]
    /// when the index changed, and file errors otherwise.
    pub fn install(self) -> Result<(), IndexTransactionError> {
        let bytes = self.private_bytes()?;
        let index = self.git_dir.join("index");
        let lock = self.git_dir.join("index.lock");
        let mut lock_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .map_err(|error| io_error(&error))?;
        let result = (|| {
            if read_index_bytes(&index)? != self.expected {
                return Err(IndexTransactionError::Changed);
            }
            lock_file
                .write_all(&bytes)
                .and_then(|()| lock_file.sync_all())
                .map_err(|_| IndexTransactionError::Io)?;
            Ok(())
        })();
        drop(lock_file);
        let result = result
            .and_then(|()| std::fs::rename(&lock, &index).map_err(|_| IndexTransactionError::Io));
        if result.is_err() {
            let _ = std::fs::remove_file(&lock);
        }
        result
    }
}

impl Drop for IndexTransaction {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temporary);
    }
}

/// Removes a Windows verbatim prefix (`\\?\C:\…` becomes `C:\…`, `\\?\UNC\server\share`
/// becomes `\\server\share`) so Git for Windows can open the path. Other platforms return the
/// path unchanged.
#[must_use]
pub fn git_compatible_index_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::ffi::OsString;
        use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
        let units = path.as_os_str().encode_wide().collect::<Vec<_>>();
        PathBuf::from(OsString::from_wide(&strip_verbatim_prefix(&units)))
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

/// UTF-16 form of [`git_compatible_index_path`], usable on every platform.
#[must_use]
pub fn strip_verbatim_prefix(units: &[u16]) -> Vec<u16> {
    const VERBATIM: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    const UNC: [u16; 4] = [b'U' as u16, b'N' as u16, b'C' as u16, b'\\' as u16];
    let Some(rest) = units.strip_prefix(&VERBATIM[..]) else {
        return units.to_vec();
    };
    if let Some(unc) = rest.strip_prefix(&UNC[..]) {
        let mut conventional = vec![u16::from(b'\\'), u16::from(b'\\')];
        conventional.extend_from_slice(unc);
        return conventional;
    }
    rest.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().collect()
    }

    #[test]
    fn verbatim_prefixes_are_removed() {
        assert_eq!(
            strip_verbatim_prefix(&wide(r"\\?\C:\repo\.git\x")),
            wide(r"C:\repo\.git\x")
        );
        assert_eq!(
            strip_verbatim_prefix(&wide(r"\\?\UNC\server\share\x")),
            wide(r"\\server\share\x")
        );
        assert_eq!(strip_verbatim_prefix(&wide(r"C:\x")), wide(r"C:\x"));
    }
}
