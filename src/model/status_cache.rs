//! A session-private index retains Git's stat cache without holding the user's index lock while
//! Git scans. A scan that had to refresh stale stat data is installed once, under Git's own lock
//! protocol, so later sessions do not repeat it.
use crate::loading::status::{self, StatusFailure, StatusOptions};
use crate::loading::CommandExecutor;
use crate::repository::DiscoveredRepository;
use crate::status::RepositoryStatus;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_INDEX_BYTES: u64 = 32 * 1024 * 1024;
static NEXT_CACHE: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
pub(super) struct StatusCache {
    directory: Option<PathBuf>,
    source_hash: Option<[u8; 32]>,
    installed: bool,
}

impl StatusCache {
    pub(super) fn load(
        &mut self,
        executable: &OsStr,
        repository: &DiscoveredRepository,
        options: StatusOptions,
        install: bool,
        executor: &mut dyn CommandExecutor,
    ) -> Result<Option<RepositoryStatus>, StatusFailure> {
        let source = repository.worktree.git_dir().as_path().join("index");
        let original = (!options.bare && std::env::var_os("GIT_INDEX_FILE").is_none())
            .then(|| read_index(&source).ok())
            .flatten();
        let hash_bytes = match &repository.head {
            crate::types::HeadState::Attached { target, .. }
            | crate::types::HeadState::Detached { target } => target.as_str().len() / 2,
            crate::types::HeadState::Unborn { .. } => 0,
        };
        let cached = original.as_ref().and_then(|bytes| {
            self.prepare(
                bytes,
                hash_bytes,
                fs::metadata(&source).ok()?.modified().ok()?,
            )
            .ok()
        });
        if let Some((index, copied)) = cached {
            let result = status::load_with_index(
                executable,
                &repository.worktree,
                Some(repository.git_version),
                options,
                Some(&index),
                executor,
            );
            // A concurrent staging operation invalidates the copy, even if size/mtime match.
            if result.is_ok() && read_index(&source).ok().as_ref() == original.as_ref() {
                if install && copied && !self.installed {
                    if let Some(original) = &original {
                        self.install_refreshed(&index, &source, original);
                    }
                }
                return result;
            }
            self.source_hash = None;
        }
        status::load(
            executable,
            &repository.worktree,
            Some(repository.git_version),
            options,
            executor,
        )
    }

    /// Returns the private index and whether it was just copied from the user's index.
    fn prepare(
        &mut self,
        bytes: &[u8],
        hash_bytes: usize,
        modified: std::time::SystemTime,
    ) -> io::Result<(PathBuf, bool)> {
        if !cacheable_index(bytes, hash_bytes) {
            return Err(io::ErrorKind::Unsupported.into());
        }
        if self.directory.is_none() {
            self.directory = Some(create_directory()?);
        }
        let directory = self.directory.as_ref().expect("cache directory created");
        if !fs::symlink_metadata(directory)?.is_dir() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let index = directory.join("index");
        let hash: [u8; 32] = Sha256::digest(bytes).into();
        let copied = self.source_hash != Some(hash);
        if copied {
            // No process is using this session-owned path between refreshes.
            let _ = fs::remove_file(&index);
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&index)?;
            std::io::Write::write_all(&mut file, bytes)?;
            // Git compares entry timestamps with index mtime for racy-clean detection.
            file.set_times(fs::FileTimes::new().set_modified(modified))?;
            self.source_hash = Some(hash);
        }
        Ok((index, copied))
    }

    /// Replaces the user's index with Git's refreshed private copy when Git rewrote it and the
    /// user's index is still `original`. Like Git's optional locks, a held lock skips the write.
    fn install_refreshed(&mut self, private: &Path, source: &Path, original: &[u8]) {
        let Ok(refreshed) = read_index(private) else {
            return;
        };
        if refreshed == original {
            return;
        }
        if install(source, original, &refreshed, private).is_ok() {
            self.installed = true;
            // The user's index now matches the private copy, so it stays valid.
            self.source_hash = Some(Sha256::digest(&refreshed).into());
        }
    }
}

fn install(source: &Path, original: &[u8], refreshed: &[u8], private: &Path) -> io::Result<()> {
    let mut lock_name = source.as_os_str().to_owned();
    lock_name.push(".lock");
    let lock = PathBuf::from(lock_name);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)?;
    let written = (|| {
        // A writer may have replaced the index between the status check and taking the lock.
        if read_index(source)? != original {
            return Err(io::ErrorKind::Interrupted.into());
        }
        std::io::Write::write_all(&mut file, refreshed)?;
        // Git judged racily clean entries against the private copy's own write time.
        file.set_times(fs::FileTimes::new().set_modified(fs::metadata(private)?.modified()?))?;
        file.sync_all()?;
        drop(file);
        fs::rename(&lock, source)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&lock);
    }
    written
}

impl Drop for StatusCache {
    fn drop(&mut self) {
        if let Some(directory) = self.directory.take() {
            let _ = fs::remove_dir_all(directory);
        }
    }
}

fn read_index(path: &Path) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > MAX_INDEX_BYTES {
        return Err(io::ErrorKind::Unsupported.into());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_INDEX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_INDEX_BYTES {
        return Err(io::ErrorKind::OutOfMemory.into());
    }
    Ok(bytes)
}

fn create_directory() -> io::Result<PathBuf> {
    for _ in 0..16 {
        let sequence = NEXT_CACHE.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("devmil-status-{}-{sequence}", std::process::id()));
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::ErrorKind::AlreadyExists.into())
}

// https://git-scm.com/docs/index-format. Git remains responsible for checksum and
// semantic validation. We only skip entries to reject mandatory extensions such
// as split/sparse indices. Unknown versions use the uncached status path.
fn cacheable_index(bytes: &[u8], hash_bytes: usize) -> bool {
    fn check(bytes: &[u8], hash_bytes: usize) -> Option<()> {
        if !matches!(hash_bytes, 20 | 32) || bytes.get(..4)? != b"DIRC" {
            return None;
        }
        let number = |at: usize| -> Option<usize> {
            Some(u32::from_be_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?) as usize)
        };
        let version = number(4)?;
        if !matches!(version, 2 | 3) {
            return None;
        }
        let end = bytes.len().checked_sub(hash_bytes)?;
        let mut cursor = 12usize;
        for _ in 0..number(8)? {
            let start = cursor;
            cursor = cursor.checked_add(40 + hash_bytes)?;
            let flags = u16::from_be_bytes(bytes.get(cursor..cursor + 2)?.try_into().ok()?);
            cursor += 2;
            if flags & 0x4000 != 0 {
                if version == 2 {
                    return None;
                }
                cursor += 2;
            }
            cursor += bytes.get(cursor..end)?.iter().position(|byte| *byte == 0)? + 1;
            cursor = start + ((cursor - start + 7) & !7);
            if cursor > end {
                return None;
            }
        }
        while cursor < end {
            if !bytes.get(cursor)?.is_ascii_uppercase() {
                return None;
            }
            let length = number(cursor + 4)?;
            cursor = cursor.checked_add(8)?.checked_add(length)?;
        }
        (cursor == end).then_some(())
    }
    check(bytes, hash_bytes).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_refreshed_copy_until_source_bytes_change_and_cleans_up() {
        let bytes = [b"DIRC\0\0\0\x02\0\0\0\0".as_slice(), &[0; 20]].concat();
        let mut cache = StatusCache::default();
        let (index, copied) = cache.prepare(&bytes, 20, std::time::UNIX_EPOCH).unwrap();
        assert!(copied);
        assert_eq!(
            fs::metadata(&index).unwrap().modified().unwrap(),
            std::time::UNIX_EPOCH
        );
        fs::write(&index, b"stat-cache-refreshed").unwrap();
        assert_eq!(
            cache.prepare(&bytes, 20, std::time::UNIX_EPOCH).unwrap(),
            (index.clone(), false)
        );
        assert_eq!(fs::read(&index).unwrap(), b"stat-cache-refreshed");
        let changed = [b"DIRC\0\0\0\x02\0\0\0\0".as_slice(), &[1; 20]].concat();
        cache.prepare(&changed, 20, std::time::UNIX_EPOCH).unwrap();
        assert_eq!(fs::read(&index).unwrap(), changed);
        let directory = index.parent().unwrap().to_path_buf();
        drop(cache);
        assert!(!directory.exists());
    }

    #[test]
    fn extension_detection_skips_names_hashes_padding_and_extended_flags() {
        for hash_bytes in [20, 32] {
            for version in [2u32, 3] {
                let mut bytes = b"DIRC".to_vec();
                bytes.extend(version.to_be_bytes());
                bytes.extend(1u32.to_be_bytes());
                let start = bytes.len();
                bytes.extend(vec![0; 40 + hash_bytes]);
                bytes.extend(if version == 3 { 0x4004u16 } else { 4u16 }.to_be_bytes());
                if version == 3 {
                    bytes.extend(0u16.to_be_bytes());
                }
                bytes.extend(b"link\0");
                while !(bytes.len() - start).is_multiple_of(8) {
                    bytes.push(0);
                }
                let extensions = bytes.len();
                bytes.extend(vec![0; hash_bytes]);
                assert!(cacheable_index(&bytes, hash_bytes));
                bytes.splice(extensions..extensions, b"link\0\0\0\0".iter().copied());
                assert!(!cacheable_index(&bytes, hash_bytes));
                bytes[extensions..extensions + 4].copy_from_slice(b"TREE");
                assert!(cacheable_index(&bytes, hash_bytes));
                bytes.pop();
                assert!(!cacheable_index(&bytes, hash_bytes));
            }
        }
    }

    #[test]
    fn split_sparse_and_invalid_indices_keep_the_uncached_path() {
        for bytes in [
            b"DIRC\0\0\0\x02\0\0\0\0link".as_slice(),
            b"DIRC\0\0\0\x02\0\0\0\0sdir",
            b"invalid",
        ] {
            let mut cache = StatusCache::default();
            assert!(cache.prepare(bytes, 20, std::time::UNIX_EPOCH).is_err());
            assert!(cache.directory.is_none());
        }
    }
}
