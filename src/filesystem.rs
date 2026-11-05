//! Capability-based Git directory identity checks.

pub mod fingerprint;
pub mod worktree_probe;

use std::{fs, path::Path};

#[cfg(unix)]
pub fn same_cap_metadata(left: &cap_std::fs::Metadata, right: &cap_std::fs::Metadata) -> bool {
    use cap_std::fs::MetadataExt;
    left.file_type().is_symlink() == right.file_type().is_symlink()
        && left.is_file() == right.is_file()
        && left.len() == right.len()
        && left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.mode() == right.mode()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(windows)]
pub fn same_cap_metadata(left: &cap_std::fs::Metadata, right: &cap_std::fs::Metadata) -> bool {
    use cap_std::fs::MetadataExt;
    left.file_type().is_symlink() == right.file_type().is_symlink()
        && left.is_file() == right.is_file()
        && left.file_attributes() == right.file_attributes()
        && left.creation_time() == right.creation_time()
        && left.last_write_time() == right.last_write_time()
        && left.file_size() == right.file_size()
}

#[cfg(not(any(unix, windows)))]
pub fn same_cap_metadata(left: &cap_std::fs::Metadata, right: &cap_std::fs::Metadata) -> bool {
    left.file_type().is_symlink() == right.file_type().is_symlink()
        && left.is_file() == right.is_file()
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
}

/// Pins a Git directory and checks identity across acquisition.
/// The hook runs after the initial inspection and is useful for controlled race tests.
///
/// # Errors
/// Rejects symlinks, replaced directories, and inaccessible paths.
pub fn open_common_git_capability<F>(
    common: &Path,
    after_before: F,
) -> Result<cap_std::fs::Dir, std::io::Error>
where
    F: FnOnce(),
{
    let initial_handle = cap_std::fs::Dir::open_ambient_dir(common, cap_std::ambient_authority())
        .and_then(|directory| directory.dir_metadata())?;
    let before = fs::symlink_metadata(common)?;
    if !before.is_dir() || before.file_type().is_symlink() {
        return Err(std::io::Error::other(
            "common Git directory is not a stable directory",
        ));
    }
    after_before();
    let ambient_before = cap_std::fs::Dir::open_ambient_dir(common, cap_std::ambient_authority())
        .and_then(|directory| directory.dir_metadata())?;
    if !same_initial_root_metadata(&before, &ambient_before)
        || !same_cap_root_metadata(&initial_handle, &ambient_before)
    {
        return Err(std::io::Error::other(
            "common Git directory changed after initial metadata inspection",
        ));
    }
    let common_parent = common
        .parent()
        .ok_or_else(|| std::io::Error::other("common Git directory has no parent"))?;
    let common_name = common
        .file_name()
        .ok_or_else(|| std::io::Error::other("common Git directory has no leaf name"))?;
    let trusted_parent =
        cap_std::fs::Dir::open_ambient_dir(common_parent, cap_std::ambient_authority())?;
    let root = trusted_parent.open_dir(common_name)?;
    let opened = root.dir_metadata()?;
    let reopened = cap_std::fs::Dir::open_ambient_dir(common, cap_std::ambient_authority())
        .and_then(|directory| directory.dir_metadata())?;
    if !same_cap_root_metadata(&ambient_before, &opened)
        || !same_cap_root_metadata(&opened, &reopened)
    {
        return Err(std::io::Error::other(
            "common Git directory changed while opening recovery ledger",
        ));
    }
    Ok(root)
}

#[cfg(unix)]
pub fn same_initial_root_metadata(
    initial: &std::fs::Metadata,
    ambient: &cap_std::fs::Metadata,
) -> bool {
    use cap_std::fs::MetadataExt as CapMetadataExt;
    use std::os::unix::fs::MetadataExt as StdMetadataExt;
    StdMetadataExt::dev(initial) == CapMetadataExt::dev(ambient)
        && StdMetadataExt::ino(initial) == CapMetadataExt::ino(ambient)
}

#[cfg(windows)]
pub fn same_initial_root_metadata(
    initial: &std::fs::Metadata,
    ambient: &cap_std::fs::Metadata,
) -> bool {
    use cap_std::fs::MetadataExt;
    use std::os::windows::fs::MetadataExt as StdMetadataExt;
    initial.file_attributes() == ambient.file_attributes()
        && initial.creation_time() == ambient.creation_time()
        && initial.last_write_time() == ambient.last_write_time()
}

#[cfg(not(any(unix, windows)))]
pub fn same_initial_root_metadata(
    initial: &std::fs::Metadata,
    ambient: &cap_std::fs::Metadata,
) -> bool {
    initial.is_dir() == ambient.is_dir() && initial.modified().ok() == ambient.modified().ok()
}

#[cfg(unix)]
pub fn same_cap_root_metadata(left: &cap_std::fs::Metadata, right: &cap_std::fs::Metadata) -> bool {
    use cap_std::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
pub fn same_cap_root_metadata(left: &cap_std::fs::Metadata, right: &cap_std::fs::Metadata) -> bool {
    use cap_fs_ext::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(any(unix, windows)))]
pub fn same_cap_root_metadata(left: &cap_std::fs::Metadata, right: &cap_std::fs::Metadata) -> bool {
    left.is_dir() == right.is_dir() && left.modified().ok() == right.modified().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn replaced_directory_is_rejected_between_inspection_and_acquisition() {
        let parent = std::env::temp_dir().join(format!(
            "devmil-capability-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let common = parent.join("git");
        fs::create_dir_all(&common).unwrap();
        fs::write(common.join("identity"), b"original").unwrap();
        assert!(open_common_git_capability(&common, || {}).is_ok());
        let result = open_common_git_capability(&common, || {
            fs::rename(&common, parent.join("original")).unwrap();
            fs::create_dir(&common).unwrap();
            fs::write(common.join("identity"), b"replacement").unwrap();
        });
        assert!(result.is_err());
        assert_eq!(fs::read(common.join("identity")).unwrap(), b"replacement");
        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_git_directory_is_rejected() {
        let parent = std::env::temp_dir().join(format!(
            "devmil-capability-link-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let real = parent.join("real");
        let link = parent.join("link");
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(open_common_git_capability(&link, || {}).is_err());
        fs::remove_dir_all(parent).unwrap();
    }
}

mod identity;
pub use identity::same_worktree_path;
