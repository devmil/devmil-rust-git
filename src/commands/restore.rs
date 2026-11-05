//! Discard, restore and patch-apply invocations for the worktree.
//!
//! Path handling matches [`super::index`]: Git path bytes, native arguments redacted as
//! `<path>`, optional NUL-separated stdin, and `GIT_LITERAL_PATHSPECS=1`. Confirmation, undo
//! capture and precondition checks remain the caller's.

use super::index::{path_invocation as with_paths, PathspecForm};
use super::invocation::{CommandArgument, GitInvocation};
use super::CommandValidationError;
use crate::diff::model::DiffPath;
use crate::types::ObjectId;

/// `git restore --worktree -- <paths>`: discards unstaged changes of tracked paths by copying
/// them from the index.
///
/// # Errors
/// Rejects an empty selection or a path that is not a native name on this platform.
pub fn discard_worktree_paths(
    paths: &[DiffPath],
    form: PathspecForm,
) -> Result<GitInvocation, CommandValidationError> {
    with_paths(
        ["restore", "--worktree"]
            .map(CommandArgument::visible)
            .to_vec(),
        paths,
        form,
    )
}

/// `git restore --source=<commit> --worktree -- <paths>`: writes the paths' content from
/// `source` into the worktree without touching the index.
///
/// # Errors
/// Rejects an empty selection or a path that is not a native name on this platform.
pub fn restore_worktree_from(
    source: &ObjectId,
    paths: &[DiffPath],
    form: PathspecForm,
) -> Result<GitInvocation, CommandValidationError> {
    with_paths(
        vec![
            CommandArgument::visible("restore"),
            CommandArgument::visible(format!("--source={}", source.as_str())),
            CommandArgument::visible("--worktree"),
        ],
        paths,
        form,
    )
}

/// `git clean -f -d -- <paths>`: deletes untracked files and directories. `git clean` has no
/// pathspec-file option, so paths are always arguments.
///
/// # Errors
/// Rejects an empty selection or a path that is not a native name on this platform.
pub fn clean_paths(paths: &[DiffPath]) -> Result<GitInvocation, CommandValidationError> {
    with_paths(
        ["clean", "-f", "-d"].map(CommandArgument::visible).to_vec(),
        paths,
        PathspecForm::Arguments,
    )
}

/// How [`apply_patch`] treats the patch.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct ApplyOptions {
    /// `--check`: verify that the patch applies without changing anything.
    pub check: bool,
    /// `--unidiff-zero`: accept hunks without context, as row-level patches produce.
    pub unidiff_zero: bool,
}

/// `git apply [--check] --recount [--unidiff-zero] -` with `patch` on stdin: applies to the
/// worktree only. Use [`super::index::apply_to_index`] for the index.
#[must_use]
pub fn apply_patch(patch: Vec<u8>, options: ApplyOptions) -> GitInvocation {
    let mut arguments = vec!["apply"];
    if options.check {
        arguments.push("--check");
    }
    arguments.push("--recount");
    if options.unidiff_zero {
        arguments.push("--unidiff-zero");
    }
    arguments.push("-");
    GitInvocation::visible(arguments).stdin_bytes(patch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_and_apply_shapes_match_the_documented_forms() {
        let paths = [DiffPath::from_bytes(b"a".to_vec()).unwrap()];
        assert_eq!(
            discard_worktree_paths(&paths, PathspecForm::Arguments)
                .unwrap()
                .display_arguments(),
            ["restore", "--worktree", "--", "<path>"]
        );
        let commit = ObjectId::parse("a".repeat(40)).unwrap();
        assert_eq!(
            restore_worktree_from(&commit, &paths, PathspecForm::Arguments)
                .unwrap()
                .display_arguments()[1],
            format!("--source={}", "a".repeat(40))
        );
        assert_eq!(
            clean_paths(&paths).unwrap().display_arguments(),
            ["clean", "-f", "-d", "--", "<path>"]
        );
        assert_eq!(
            apply_patch(
                Vec::new(),
                ApplyOptions {
                    check: true,
                    unidiff_zero: false
                }
            )
            .display_arguments(),
            ["apply", "--check", "--recount", "-"]
        );
        assert_eq!(
            apply_patch(
                Vec::new(),
                ApplyOptions {
                    check: false,
                    unidiff_zero: true
                }
            )
            .display_arguments(),
            ["apply", "--recount", "--unidiff-zero", "-"]
        );
    }
}
