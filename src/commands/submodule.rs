//! Submodule and Git LFS invocations.

use super::CommandValidationError;
use crate::types::RepositoryRelativePath;
use std::ffi::OsString;

/// Longest accepted LFS track pattern, in bytes.
pub const MAX_LFS_PATTERN_BYTES: usize = 1024;

/// Builds `git submodule update --recursive [--init] -- <path>`.
#[must_use]
pub fn submodule_update_arguments(path: &RepositoryRelativePath, init: bool) -> Vec<OsString> {
    let mut arguments: Vec<OsString> =
        vec!["submodule".into(), "update".into(), "--recursive".into()];
    if init {
        arguments.push("--init".into());
    }
    arguments.push("--".into());
    arguments.push(path.as_path().as_os_str().to_owned());
    arguments
}

/// Whether [`lfs_arguments`] adds or removes a tracked pattern, or downloads content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LfsRequest {
    /// `git lfs track -- <pattern>`
    Track(String),
    /// `git lfs untrack -- <pattern>`
    Untrack(String),
    /// `git lfs pull`
    Pull,
}

/// Builds a Git LFS invocation. The caller checks that Git LFS is installed
/// ([`crate::loading::extensions`]).
///
/// # Errors
/// Rejects a pattern that is empty, longer than [`MAX_LFS_PATTERN_BYTES`] or contains control
/// characters.
pub fn lfs_arguments(request: &LfsRequest) -> Result<Vec<OsString>, CommandValidationError> {
    let (verb, pattern) = match request {
        LfsRequest::Track(pattern) => ("track", pattern),
        LfsRequest::Untrack(pattern) => ("untrack", pattern),
        LfsRequest::Pull => return Ok(vec!["lfs".into(), "pull".into()]),
    };
    if pattern.is_empty()
        || pattern.len() > MAX_LFS_PATTERN_BYTES
        || pattern.chars().any(char::is_control)
    {
        return Err(CommandValidationError(
            "LFS pattern is empty, oversized or contains control characters".into(),
        ));
    }
    Ok(vec!["lfs".into(), verb.into(), "--".into(), pattern.into()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submodule_and_lfs_arguments_end_options_before_values() {
        let path = RepositoryRelativePath::new("libs/-dash").unwrap();
        assert_eq!(
            submodule_update_arguments(&path, true),
            [
                "submodule",
                "update",
                "--recursive",
                "--init",
                "--",
                "libs/-dash"
            ]
        );
        assert_eq!(
            lfs_arguments(&LfsRequest::Track("-x.bin".into())).unwrap(),
            ["lfs", "track", "--", "-x.bin"]
        );
        assert!(lfs_arguments(&LfsRequest::Untrack("bad\npattern".into())).is_err());
        assert_eq!(lfs_arguments(&LfsRequest::Pull).unwrap(), ["lfs", "pull"]);
    }
}
