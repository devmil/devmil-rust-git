//! Clone and repository-initialization values and validation.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Component, Path};

use crate::types::RepositoryPath;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloneSourceKind {
    Http,
    Ssh,
    Git,
    File,
    ScpLike,
    Local,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloneSource {
    value: OsString,
    kind: CloneSourceKind,
}

impl CloneSource {
    /// # Errors
    ///
    /// Rejects unsupported, oversized, control-character, and credential-bearing sources.
    pub fn parse(value: impl Into<String>) -> Result<Self, CloneValidationError> {
        let value = value.into();
        if value.is_empty() || value.len() > 8_192 || value.chars().any(char::is_control) {
            return Err(CloneValidationError::InvalidSource);
        }
        let kind = classify_source(&value)?;
        if has_embedded_password(&value) {
            return Err(CloneValidationError::EmbeddedCredential);
        }
        Ok(Self {
            value: value.into(),
            kind,
        })
    }

    /// Creates a byte-safe local source without converting its native path to display text.
    ///
    /// # Errors
    ///
    /// The local source must be absolute.
    pub fn local(path: &RepositoryPath) -> Result<Self, CloneValidationError> {
        if !path.as_path().is_absolute() {
            return Err(CloneValidationError::InvalidSource);
        }
        Ok(Self {
            value: path.as_path().as_os_str().to_owned(),
            kind: CloneSourceKind::Local,
        })
    }

    pub fn as_os_str(&self) -> &OsStr {
        &self.value
    }
    pub const fn kind(&self) -> CloneSourceKind {
        self.kind
    }
}

fn classify_source(value: &str) -> Result<CloneSourceKind, CloneValidationError> {
    if value.starts_with("https://") || value.starts_with("http://") {
        Ok(CloneSourceKind::Http)
    } else if value.starts_with("ssh://") {
        Ok(CloneSourceKind::Ssh)
    } else if value.starts_with("git://") {
        Ok(CloneSourceKind::Git)
    } else if value.starts_with("file://") {
        Ok(CloneSourceKind::File)
    } else if Path::new(value).is_absolute() {
        Ok(CloneSourceKind::Local)
    } else if value
        .split_once(':')
        .is_some_and(|(host, path)| !host.is_empty() && !host.contains('/') && !path.is_empty())
    {
        Ok(CloneSourceKind::ScpLike)
    } else {
        Err(CloneValidationError::InvalidSource)
    }
}

fn has_embedded_password(value: &str) -> bool {
    value
        .split_once("://")
        .and_then(|(_, rest)| rest.split('/').next())
        .and_then(|authority| authority.split_once('@').map(|(user, _)| user))
        .is_some_and(|user| user.contains(':'))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloneDestination(RepositoryPath);

impl CloneDestination {
    /// # Errors
    ///
    /// Rejects non-absolute and lexically non-normalized destinations.
    pub fn new(path: RepositoryPath) -> Result<Self, CloneValidationError> {
        if !path.as_path().is_absolute()
            || path.as_path().parent().is_none()
            || path
                .as_path()
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            Err(CloneValidationError::InvalidDestination)
        } else {
            Ok(Self(path))
        }
    }
    pub fn path(&self) -> &RepositoryPath {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitialBranch(String);
impl InitialBranch {
    /// # Errors
    ///
    /// Rejects names that cannot safely be passed as an initial branch argument.
    pub fn parse(value: impl Into<String>) -> Result<Self, CloneValidationError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 255
            || value.starts_with('-')
            || value.contains(char::is_whitespace)
            || value.contains("..")
            || value.contains("@{")
        {
            Err(CloneValidationError::InvalidInitialBranch)
        } else {
            Ok(Self(value))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityWarning {
    Configured,
    MissingName,
    MissingEmail,
    MissingNameAndEmail,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloneValidationError {
    InvalidSource,
    EmbeddedCredential,
    InvalidDestination,
    InvalidInitialBranch,
}
impl fmt::Display for CloneValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidSource => "clone source is not a supported URL or path",
            Self::EmbeddedCredential => "clone URL must not contain a password or token",
            Self::InvalidDestination => "clone destination must be an absolute normalized path",
            Self::InvalidInitialBranch => "initial branch name is invalid",
        })
    }
}
impl std::error::Error for CloneValidationError {}

/// Builds `git init [--bare] [-b <branch>] -- <destination>`.
///
/// The `--` keeps a destination that starts with `-` from being read as an option. The caller
/// runs it from any existing directory (for example the destination's parent) and owns the
/// "destination is empty" policy.
#[must_use]
pub fn init_arguments(
    destination: &CloneDestination,
    bare: bool,
    initial_branch: Option<&InitialBranch>,
) -> Vec<OsString> {
    let mut arguments = vec![OsString::from("init")];
    if bare {
        arguments.push("--bare".into());
    }
    if let Some(branch) = initial_branch {
        arguments.push("-b".into());
        arguments.push(branch.as_str().into());
    }
    arguments.push("--".into());
    arguments.push(destination.path().as_path().as_os_str().to_owned());
    arguments
}

/// Builds `git clone --progress [--branch <branch>] -- <source> <destination>`.
///
/// Prefix the transfer configuration from [`crate::auth::transfer_configuration`] and run it
/// with [`crate::auth::transfer_environment`] from the destination's parent. `branch` selects
/// the branch to check out instead of the remote's `HEAD`; `git clone` has no option to name
/// the branch of an empty clone, which follows the remote's `HEAD`.
#[must_use]
pub fn clone_arguments(
    source: &CloneSource,
    destination: &CloneDestination,
    branch: Option<&InitialBranch>,
) -> Vec<OsString> {
    let mut arguments = vec![OsString::from("clone"), OsString::from("--progress")];
    if let Some(branch) = branch {
        arguments.push("--branch".into());
        arguments.push(branch.as_str().into());
    }
    arguments.push("--".into());
    arguments.push(source.as_os_str().to_owned());
    arguments.push(destination.path().as_path().as_os_str().to_owned());
    arguments
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    const ABSOLUTE_SOURCE: &str = r"C:\tmp\a";
    #[cfg(not(windows))]
    const ABSOLUTE_SOURCE: &str = "/tmp/a";
    #[cfg(windows)]
    const ABSOLUTE_DESTINATION: &str = r"C:\tmp\repo";
    #[cfg(not(windows))]
    const ABSOLUTE_DESTINATION: &str = "/tmp/repo";

    #[test]
    fn validates_sources_and_rejects_embedded_passwords() {
        for value in [
            "https://example.test/a.git",
            "git@example.test:a.git",
            "ssh://git@example.test/a",
            "file:///tmp/a",
            ABSOLUTE_SOURCE,
        ] {
            assert!(CloneSource::parse(value).is_ok(), "{value}");
        }
        assert_eq!(
            CloneSource::parse("https://user:secret@example.test/a"),
            Err(CloneValidationError::EmbeddedCredential)
        );
        assert!(CloneSource::parse("relative/path").is_err());
    }
    #[test]
    fn validates_destination_and_branch() {
        assert!(CloneDestination::new(RepositoryPath::new(ABSOLUTE_DESTINATION).unwrap()).is_ok());
        assert!(CloneDestination::new(RepositoryPath::new("relative").unwrap()).is_err());
        assert!(InitialBranch::parse("main").is_ok());
        assert!(InitialBranch::parse("bad..name").is_err());
    }
}
