//! Capability-relative working-tree file probes.
//!
//! Windows path traversal opens each component without following reparse points. The open
//! directory handle, rather than an ambient joined path, anchors the next component.

#[cfg(any(windows, test))]
use std::io;
#[cfg(any(windows, test))]
use std::path::{Component, Path};

#[cfg(any(windows, test))]
fn normal_components(path: &Path) -> io::Result<Vec<&std::ffi::OsStr>> {
    let components = path
        .components()
        .map(|component| match component {
            Component::Normal(component) => Ok(component),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "working-tree probe path must contain only normal relative components",
            )),
        })
        .collect::<io::Result<Vec<_>>>()?;
    if components.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "working-tree probe path must be non-empty",
        ));
    }
    Ok(components)
}

/// Opens one regular file below `worktree_root` without traversing an intermediate reparse point.
///
/// The caller must treat every error as a failed probe. This adapter deliberately refuses device,
/// root, current-directory, parent-directory, symlink, junction, mount-point, and other reparse
/// components.
///
/// # Errors
///
/// Returns an I/O error when a component is invalid, cannot be opened, is a reparse point, or the
/// final target is not a regular file.
#[cfg(windows)]
pub fn open_regular_file(worktree_root: &Path, relative_path: &Path) -> io::Result<std::fs::File> {
    use cap_fs_ext::{
        FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt, OsMetadataExt as _,
    };
    use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt as _};
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_SHARE_READ_WRITE: u32 = 0x0000_0003;

    let components = normal_components(relative_path)?;
    let mut root_options = std::fs::OpenOptions::new();
    root_options
        .read(true)
        .share_mode(FILE_SHARE_READ_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let root_file = root_options.open(worktree_root)?;
    let root_metadata = root_file.metadata()?;
    if !root_metadata.is_dir()
        || root_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "working-tree root is not a non-reparse directory",
        ));
    }
    let mut directory = Dir::from_std_file(root_file);

    for component in &components[..components.len() - 1] {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .maybe_dir(true)
            .follow(FollowSymlinks::No);
        options.share_mode(FILE_SHARE_READ_WRITE);
        let opened = directory.open_with(component, &options)?;
        let metadata = opened.metadata()?;
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "working-tree probe parent is not a non-reparse directory",
            ));
        }
        directory = Dir::from_std_file(opened.into_std());
    }

    let mut options = OpenOptions::new();
    options
        .read(true)
        .maybe_dir(true)
        .follow(FollowSymlinks::No);
    options.share_mode(FILE_SHARE_READ_WRITE);
    let opened = directory.open_with(components[components.len() - 1], &options)?;
    let metadata = opened.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "working-tree probe target is not a non-reparse regular file",
        ));
    }
    Ok(opened.into_std())
}

#[cfg(test)]
mod tests {
    use super::normal_components;
    use std::path::Path;

    #[test]
    fn component_contract_rejects_every_lexical_escape() {
        assert!(normal_components(Path::new("src/main.rs")).is_ok());
        for path in ["", ".", "..", "../outside", "src/../outside"] {
            assert!(normal_components(Path::new(path)).is_err(), "{path}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn opens_nested_regular_file_and_rejects_absolute_paths() {
        let root =
            std::env::temp_dir().join(format!("devmil-worktree-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/file.bin"), b"text\0binary").unwrap();

        let mut file = super::open_regular_file(&root, Path::new("nested/file.bin")).unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes).unwrap();
        assert_eq!(bytes, b"text\0binary");
        // Windows refuses to delete a directory holding an open file.
        drop(file);
        assert!(super::open_regular_file(&root, Path::new(r"C:\outside")).is_err());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn rejects_intermediate_junction_and_junction_root() {
        let base =
            std::env::temp_dir().join(format!("devmil-worktree-reparse-{}", std::process::id()));
        let root = base.join("worktree");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.bin"), b"outside\0binary").unwrap();
        let junction = root.join("junction");
        let status = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "the Windows test host must permit junctions"
        );

        assert!(super::open_regular_file(&root, Path::new("junction/secret.bin")).is_err());
        assert!(super::open_regular_file(&junction, Path::new("secret.bin")).is_err());

        std::fs::remove_dir_all(base).unwrap();
    }
}
