//! Native stash argv and NUL-delimited path parsing.

use super::CommandValidationError;
use crate::activity::{ActivityArgument, ActivityCommand};
use crate::types::{ObjectId, RepositoryRelativePath};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// One exact command the family plans and the infrastructure adapter executes. `argv` is the
/// native argument vector; `activity` is the redaction-free preview built in the same
/// constructor, so preview and executed argv are provably the same value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeCommand {
    executable: OsString,
    working_directory: PathBuf,
    argv: Vec<OsString>,
    activity: ActivityCommand,
}

impl NativeCommand {
    /// Builds a command and its activity preview from the same inputs.
    #[must_use]
    pub fn new(
        executable: impl Into<OsString>,
        working_directory: impl Into<PathBuf>,
        argv: Vec<OsString>,
    ) -> Self {
        let executable = executable.into();
        let working_directory = working_directory.into();
        let activity = ActivityCommand {
            executable: executable.to_string_lossy().into_owned(),
            arguments: argv
                .iter()
                .map(|argument| ActivityArgument {
                    display: argument.to_string_lossy().into_owned(),
                    redacted: false,
                    native_lossless: OsStr::new(argument.to_string_lossy().as_ref())
                        == argument.as_os_str(),
                })
                .collect(),
            environment: Vec::new(),
            working_directory: Some(working_directory.to_string_lossy().into_owned()),
            stdin_bytes: 0,
        };
        Self {
            executable,
            working_directory,
            argv,
            activity,
        }
    }

    pub fn executable(&self) -> &OsStr {
        &self.executable
    }

    pub fn working_directory(&self) -> &PathBuf {
        &self.working_directory
    }

    pub fn argv(&self) -> &[OsString] {
        &self.argv
    }

    pub const fn activity(&self) -> &ActivityCommand {
        &self.activity
    }
}

/// Lists stash identities and reflog subjects in the format parsed by [`crate::refs::parse_stashes`].
#[must_use]
pub fn list_arguments() -> Vec<OsString> {
    ["stash", "list", "--format=%H%x00%gd%x00%gs%x00"]
        .map(OsString::from)
        .to_vec()
}

/// Builds stash push argv, preserving native path bytes after the option separator.
///
/// # Errors
/// Rejects NUL in the message. Consumer message and path-count bounds are separate.
pub fn push_arguments(
    message: Option<&str>,
    paths: &[RepositoryRelativePath],
    include_untracked: bool,
) -> Result<Vec<OsString>, CommandValidationError> {
    push_arguments_with_options(message, paths, include_untracked, false)
}

/// Builds stash push argv with the complete shared option set.
///
/// # Errors
/// Rejects NUL in the message. Consumer message and path-count bounds are separate.
pub fn push_arguments_with_options(
    message: Option<&str>,
    paths: &[RepositoryRelativePath],
    include_untracked: bool,
    keep_index: bool,
) -> Result<Vec<OsString>, CommandValidationError> {
    let mut argv = vec![OsString::from("stash"), OsString::from("push")];
    if include_untracked {
        argv.push("--include-untracked".into());
    }
    if keep_index {
        argv.push("--keep-index".into());
    }
    if let Some(message) = message {
        if message.contains('\0') {
            return Err(CommandValidationError("stash message contains NUL".into()));
        }
        argv.extend([OsString::from("-m"), OsString::from(message)]);
    }
    append_path_scope(&mut argv, paths);
    Ok(argv)
}

/// Stash mutations that consume an existing entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryOperation {
    Apply,
    /// Reapply both the index and working-tree changes.
    ApplyWithIndex,
    Pop,
    Drop,
}

/// Builds entry argv. Apply targets the immutable commit; pop/drop require a reflog selector.
///
/// Callers must re-resolve a pop/drop selector before execution to detect stack movement.
///
/// # Errors
/// Rejects a selector that is not in canonical stash reflog form.
pub fn entry_arguments(
    operation: EntryOperation,
    commit: &ObjectId,
    selector: &str,
) -> Result<Vec<OsString>, CommandValidationError> {
    validate_selector(selector, usize::MAX)?;
    let (verb, target) = match operation {
        EntryOperation::Apply | EntryOperation::ApplyWithIndex => ("apply", commit.as_str()),
        EntryOperation::Pop => ("pop", selector),
        EntryOperation::Drop => ("drop", selector),
    };
    let mut arguments = vec!["stash".into(), verb.into()];
    if operation == EntryOperation::ApplyWithIndex {
        arguments.push("--index".into());
    }
    arguments.push(target.into());
    Ok(arguments)
}

/// Validates a stash reflog selector with a consumer-selected byte bound.
///
/// # Errors
/// Rejects selectors other than stash@{n}, where n is a nonempty decimal index.
pub fn validate_selector(selector: &str, maximum: usize) -> Result<(), CommandValidationError> {
    let valid = selector.len() <= maximum
        && selector
            .strip_prefix("stash@{")
            .and_then(|rest| rest.strip_suffix('}'))
            .is_some_and(|index| {
                !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())
            });
    if valid {
        Ok(())
    } else {
        Err(CommandValidationError(
            "stash selector must have the bounded form stash@{n}".into(),
        ))
    }
}

/// Appends a literal path scope without decoding native filesystem bytes.
pub fn append_path_scope(arguments: &mut Vec<OsString>, paths: &[RepositoryRelativePath]) {
    if !paths.is_empty() {
        arguments.push("--".into());
        arguments.extend(
            paths
                .iter()
                .map(|path| path.as_path().as_os_str().to_owned()),
        );
    }
}

/// Parses complete NUL-separated repository paths from Git -z output.
///
/// # Errors
/// Rejects truncated fields, invalid relative paths, and non-UTF-8 paths on non-Unix hosts.
pub fn parse_z_paths(bytes: &[u8]) -> Result<Vec<RepositoryRelativePath>, CommandValidationError> {
    if !bytes.is_empty() && !bytes.ends_with(&[0]) {
        return Err(CommandValidationError(
            "Git path output is missing its NUL terminator".into(),
        ));
    }
    bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .map(|field| {
            #[cfg(unix)]
            let path = {
                use std::os::unix::ffi::OsStringExt;
                PathBuf::from(OsString::from_vec(field.to_vec()))
            };
            #[cfg(not(unix))]
            let path = PathBuf::from(
                std::str::from_utf8(field)
                    .map_err(|_| CommandValidationError("Git path is not valid UTF-8".into()))?,
            );
            RepositoryRelativePath::new(path).map_err(|_| {
                CommandValidationError("Git returned a path that is not repository-relative".into())
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_apply_uses_exact_stash_object() {
        let commit = ObjectId::parse("a".repeat(40)).unwrap();
        assert_eq!(
            entry_arguments(EntryOperation::ApplyWithIndex, &commit, "stash@{0}").unwrap(),
            ["stash", "apply", "--index", commit.as_str()].map(OsString::from)
        );
    }

    #[test]
    fn apply_uses_commit_while_pop_and_drop_use_reflog_selector() {
        let commit = ObjectId::parse("a".repeat(40)).unwrap();
        assert_eq!(
            entry_arguments(EntryOperation::Apply, &commit, "stash@{2}").unwrap()[2],
            commit.as_str()
        );
        for operation in [EntryOperation::Pop, EntryOperation::Drop] {
            assert_eq!(
                entry_arguments(operation, &commit, "stash@{2}").unwrap()[2],
                "stash@{2}"
            );
        }
        assert!(entry_arguments(EntryOperation::Drop, &commit, "--all").is_err());
    }

    #[test]
    fn push_keeps_literal_paths_after_separator() {
        let paths = vec![RepositoryRelativePath::new("--literal").unwrap()];
        assert_eq!(
            push_arguments_with_options(Some("message"), &paths, true, true).unwrap(),
            [
                "stash",
                "push",
                "--include-untracked",
                "--keep-index",
                "-m",
                "message",
                "--",
                "--literal"
            ]
            .map(OsString::from)
        );
        assert!(parse_z_paths(b"path-without-terminator").is_err());
        assert!(parse_z_paths(b"../escape\0").is_err());
        assert_eq!(
            push_arguments(None, &[], false).unwrap(),
            ["stash", "push"].map(OsString::from)
        );
    }

    #[cfg(unix)]
    #[test]
    fn raw_paths_survive_parsing_argv_and_native_command() {
        use std::os::unix::ffi::OsStrExt;
        let paths = parse_z_paths(b"path-\xff\0").unwrap();
        let args = push_arguments(None, &paths, false).unwrap();
        let command = NativeCommand::new("git", "/repo", args);
        assert_eq!(command.argv().last().unwrap().as_bytes(), b"path-\xff");
        assert!(!command.activity().arguments.last().unwrap().native_lossless);
    }
}
