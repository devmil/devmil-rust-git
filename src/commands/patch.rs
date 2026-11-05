//! Patch export argv for working-tree, comparison, commit, and range sources.
//!
//! Consumers own scope selection, range validation, output bounds, and the destination file.

use super::stash::append_path_scope;
use crate::types::{ObjectId, RepositoryRelativePath};
use std::ffi::OsString;

/// What a patch export reads. Diff sources write a unified diff; commit sources write a
/// `format-patch` mail series on standard output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatchExportSource {
    /// Unstaged changes, or the staged changes with `staged`.
    Worktree { staged: bool },
    /// `HEAD` versus the working tree.
    Head,
    /// The difference between two commits.
    Comparison { old: ObjectId, new: ObjectId },
    /// One commit with its message.
    Commit(ObjectId),
    /// A first-parent chain with its messages. `from_root` includes the root commit, which has
    /// no parent to start the range from. Callers verify the chain before building arguments.
    FirstParentRange {
        oldest: ObjectId,
        newest: ObjectId,
        from_root: bool,
    },
}

/// Builds patch export argv, limiting the patch to `paths` after the option separator.
#[must_use]
pub fn export_arguments(
    source: &PatchExportSource,
    paths: &[RepositoryRelativePath],
) -> Vec<OsString> {
    let mut argv: Vec<OsString> = match source {
        PatchExportSource::Worktree { staged } => {
            let mut argv = vec!["diff".into()];
            if *staged {
                argv.push("--cached".into());
            }
            argv
        }
        PatchExportSource::Head => vec!["diff".into(), "HEAD".into()],
        PatchExportSource::Comparison { old, new } => {
            vec!["diff".into(), old.as_str().into(), new.as_str().into()]
        }
        PatchExportSource::Commit(commit) => vec![
            "format-patch".into(),
            "-1".into(),
            "--stdout".into(),
            commit.as_str().into(),
        ],
        PatchExportSource::FirstParentRange {
            oldest,
            newest,
            from_root,
        } => {
            let mut argv = vec![
                "format-patch".into(),
                "--stdout".into(),
                "--first-parent".into(),
            ];
            if *from_root {
                argv.extend(["--root".into(), newest.as_str().into()]);
            } else {
                argv.push(format!("{}^..{}", oldest.as_str(), newest.as_str()).into());
            }
            argv
        }
    };
    append_path_scope(&mut argv, paths);
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(digit: &str) -> ObjectId {
        ObjectId::parse(digit.repeat(40)).unwrap()
    }

    fn path(value: &str) -> RepositoryRelativePath {
        RepositoryRelativePath::new(value).unwrap()
    }

    fn text(source: &PatchExportSource, paths: &[RepositoryRelativePath]) -> Vec<String> {
        export_arguments(source, paths)
            .into_iter()
            .map(|argument| argument.into_string().unwrap())
            .collect()
    }

    #[test]
    fn diff_sources_build_unified_diff_arguments() {
        assert_eq!(
            text(&PatchExportSource::Worktree { staged: false }, &[]),
            ["diff"]
        );
        assert_eq!(
            text(
                &PatchExportSource::Worktree { staged: true },
                &[path("src/a.rs"), path("-option-like")]
            ),
            ["diff", "--cached", "--", "src/a.rs", "-option-like"]
        );
        assert_eq!(text(&PatchExportSource::Head, &[]), ["diff", "HEAD"]);
        let (old, new) = (id("a"), id("b"));
        assert_eq!(
            text(
                &PatchExportSource::Comparison {
                    old: old.clone(),
                    new: new.clone()
                },
                &[path("docs/read me.md")]
            ),
            ["diff", old.as_str(), new.as_str(), "--", "docs/read me.md"]
        );
    }

    #[test]
    fn commit_sources_build_format_patch_arguments() {
        let (oldest, newest) = (id("c"), id("d"));
        assert_eq!(
            text(
                &PatchExportSource::Commit(newest.clone()),
                &[path("src/a.rs")]
            ),
            [
                "format-patch",
                "-1",
                "--stdout",
                newest.as_str(),
                "--",
                "src/a.rs"
            ]
        );
        assert_eq!(
            text(
                &PatchExportSource::FirstParentRange {
                    oldest: oldest.clone(),
                    newest: newest.clone(),
                    from_root: false
                },
                &[]
            ),
            [
                "format-patch".to_owned(),
                "--stdout".to_owned(),
                "--first-parent".to_owned(),
                format!("{}^..{}", oldest.as_str(), newest.as_str()),
            ]
        );
        assert_eq!(
            text(
                &PatchExportSource::FirstParentRange {
                    oldest,
                    newest: newest.clone(),
                    from_root: true
                },
                &[]
            ),
            [
                "format-patch",
                "--stdout",
                "--first-parent",
                "--root",
                newest.as_str()
            ]
        );
    }
}
