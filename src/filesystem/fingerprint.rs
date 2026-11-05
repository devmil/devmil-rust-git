//! Exact worktree preconditions without retaining complete file contents.
use sha2::{Digest, Sha256};
use std::{fs, io::Read, path::Path};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PathFingerprint {
    Missing,
    File {
        bytes: u64,
        digest: [u8; 32],
        executable: bool,
    },
    Symlink(std::path::PathBuf),
    Directory,
    /// A display-time precondition. Capture exact content before a mutation.
    DeferredFile(FileStamp),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileStamp {
    bytes: u64,
    modified: Option<std::time::SystemTime>,
    executable: bool,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
    #[cfg(windows)]
    identity: (u64, u64, u32),
}

impl FileStamp {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        #[cfg(windows)]
        use std::os::windows::fs::MetadataExt as _;
        Self {
            bytes: metadata.len(),
            modified: metadata.modified().ok(),
            executable: executable(metadata),
            #[cfg(unix)]
            identity: (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            ),
            #[cfg(windows)]
            identity: (
                metadata.creation_time(),
                metadata.last_write_time(),
                metadata.file_attributes(),
            ),
        }
    }
}

/// Avoids reading oversized file content merely to display file information.
///
/// # Errors
/// Returns filesystem or exact-fingerprint failures for smaller files.
pub fn preview_fingerprint_path(
    path: &Path,
    maximum_content_bytes: u64,
) -> Result<PathFingerprint, std::io::Error> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() > maximum_content_bytes
        {
            return Ok(PathFingerprint::DeferredFile(FileStamp::from_metadata(
                &metadata,
            )));
        }
    }
    fingerprint_path(path)
}

/// Compares a preview stamp or an exact content fingerprint with current state.
///
/// # Errors
/// Returns filesystem or fingerprint failures.
pub fn fingerprint_matches(
    path: &Path,
    expected: &PathFingerprint,
) -> Result<bool, std::io::Error> {
    if let PathFingerprint::DeferredFile(stamp) = expected {
        let metadata = fs::symlink_metadata(path)?;
        return Ok(metadata.is_file()
            && !metadata.file_type().is_symlink()
            && &FileStamp::from_metadata(&metadata) == stamp);
    }
    Ok(&fingerprint_path(path)? == expected)
}

/// Reads regular files through one fixed buffer. Symlink targets are never followed.
///
/// # Errors
/// Rejects unsupported entries, read failures and files changing during capture.
pub fn fingerprint_path(path: &Path) -> Result<PathFingerprint, std::io::Error> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PathFingerprint::Missing)
        }
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        return fs::read_link(path).map(PathFingerprint::Symlink);
    }
    if metadata.is_dir() {
        return Ok(PathFingerprint::Directory);
    }
    if !metadata.is_file() {
        return Err(std::io::Error::other("unsupported worktree entry"));
    }
    let mut file = fs::File::open(path)?;
    if FileStamp::from_metadata(&file.metadata()?) != FileStamp::from_metadata(&metadata) {
        return Err(std::io::Error::other(
            "worktree entry changed while opening",
        ));
    }
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut digest = Sha256::new();
    let mut bytes = 0_u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        digest.update(&buffer[..count]);
    }
    let after = fs::symlink_metadata(path)?;
    if !after.is_file()
        || after.file_type().is_symlink()
        || bytes != metadata.len()
        || after.len() != metadata.len()
        || after.modified()? != metadata.modified()?
        || executable(&after) != executable(&metadata)
        || FileStamp::from_metadata(&after) != FileStamp::from_metadata(&metadata)
    {
        return Err(std::io::Error::other(
            "worktree entry changed during capture",
        ));
    }
    Ok(PathFingerprint::File {
        bytes,
        digest: digest.finalize().into(),
        executable: executable(&metadata),
    })
}

#[cfg(unix)]
fn executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fingerprints_detect_equal_size_content_changes() {
        let root = std::env::temp_dir().join(format!("git-fingerprint-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("file");
        assert_eq!(fingerprint_path(&path).unwrap(), PathFingerprint::Missing);
        fs::write(&path, b"before").unwrap();
        let before = fingerprint_path(&path).unwrap();
        let preview = preview_fingerprint_path(&path, 1).unwrap();
        assert!(matches!(preview, PathFingerprint::DeferredFile(_)));
        assert!(fingerprint_matches(&path, &preview).unwrap());
        fs::write(&path, b"after!").unwrap();
        // Coarse write clocks (Windows) can stamp both writes alike; date the
        // rewrite later, as a real later edit would be.
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(2))
            .unwrap();
        assert_ne!(fingerprint_path(&path).unwrap(), before);
        assert!(!fingerprint_matches(&path, &preview).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
}
