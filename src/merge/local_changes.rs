//! Which local changes stop Git from applying a commit's change in place.
//!
//! `git revert` and `git cherry-pick` (without `--no-commit`) refuse to start while the index
//! differs from `HEAD`, and their merge refuses to touch a path that has unstaged or untracked
//! local content. Local changes elsewhere stay where they are. [`local_change_overlap`] answers
//! that question mechanically from a status snapshot and the paths the commits change (see
//! [`crate::probes::changed_paths`]); whether to set the changes aside, ask or refuse belongs to
//! the application.

use crate::diff::model::DiffPath;
use crate::native_bytes::path_to_bytes;
use crate::status::{ChangeKind, StatusEntry};
use crate::types::RepositoryRelativePath;

/// Local changes that collide with an in-place revert or cherry-pick.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LocalChangeOverlap {
    /// Changed, renamed, conflicted or untracked local paths that are, contain or lie inside a
    /// path the commits change, in status order. A rename contributes when either side
    /// collides; its destination is reported.
    pub paths: Vec<RepositoryRelativePath>,
    /// Whether any change is staged. Git's sequencer refuses to commit over a dirty index even
    /// when no staged path collides.
    pub staged: bool,
}

impl LocalChangeOverlap {
    /// Whether Git can apply the change with the local changes left in place.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && !self.staged
    }
}

/// Classifies `entries` (a status snapshot) against `changed`, the paths the commits change.
///
/// Ignored entries never collide: Git treats ignored files as expendable. Paths compare as
/// raw bytes by whole components, so `dir` collides with `dir/file` and an untracked `dir/`
/// with a changed `dir/file`, while `dir2` does not collide with `dir`. The comparison is
/// exact: on a case-insensitive filesystem Git may still refuse a path that differs only in
/// case, before it changes anything.
#[must_use]
pub fn local_change_overlap(entries: &[StatusEntry], changed: &[DiffPath]) -> LocalChangeOverlap {
    let changed: Vec<&[u8]> = changed.iter().map(|path| trim(path.as_bytes())).collect();
    let collides = |path: &RepositoryRelativePath| {
        let local = path_to_bytes(path.as_path());
        let local = trim(&local);
        changed.iter().any(|changed| same_or_nested(local, changed))
    };
    let mut overlap = LocalChangeOverlap::default();
    for entry in entries {
        let (staged, hit) = match entry {
            StatusEntry::Ordinary(item) => (
                item.state.index != ChangeKind::Unmodified,
                collides(&item.path),
            ),
            StatusEntry::Rename(item) => (
                item.state.index != ChangeKind::Unmodified,
                collides(&item.path) || collides(&item.source),
            ),
            StatusEntry::Unmerged(item) => (true, collides(&item.path)),
            StatusEntry::Untracked(path) => (false, collides(path)),
            StatusEntry::Ignored(_) => (false, false),
        };
        overlap.staged |= staged;
        if hit {
            overlap.paths.push(entry.path().clone());
        }
    }
    overlap
}

fn trim(path: &[u8]) -> &[u8] {
    path.strip_suffix(b"/").unwrap_or(path)
}

fn same_or_nested(left: &[u8], right: &[u8]) -> bool {
    let (short, long) = if left.len() <= right.len() {
        (left, right)
    } else {
        (right, left)
    };
    long.starts_with(short) && (long.len() == short.len() || long[short.len()] == b'/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{FileState, OrdinaryStatus, RenameStatus, SubmoduleState};

    fn path(value: &str) -> RepositoryRelativePath {
        RepositoryRelativePath::new(value).unwrap()
    }

    fn diff(value: &str) -> DiffPath {
        DiffPath::from_bytes(value.as_bytes().to_vec()).unwrap()
    }

    fn ordinary(value: &str, index: ChangeKind, worktree: ChangeKind) -> StatusEntry {
        StatusEntry::Ordinary(OrdinaryStatus {
            path: path(value),
            state: FileState { index, worktree },
            submodule: SubmoduleState::default(),
        })
    }

    #[test]
    fn unrelated_unstaged_and_untracked_changes_do_not_overlap() {
        let entries = [
            ordinary("g.txt", ChangeKind::Unmodified, ChangeKind::Modified),
            StatusEntry::Untracked(path("new.txt")),
            StatusEntry::Untracked(path("dir2/")),
            StatusEntry::Ignored(path("f.txt")),
        ];
        let overlap = local_change_overlap(&entries, &[diff("f.txt"), diff("dir/a")]);
        assert!(overlap.is_empty(), "{overlap:?}");
    }

    #[test]
    fn colliding_paths_and_staged_changes_are_reported() {
        let entries = [
            ordinary("f.txt", ChangeKind::Unmodified, ChangeKind::Modified),
            ordinary("other.txt", ChangeKind::Modified, ChangeKind::Unmodified),
            StatusEntry::Untracked(path("dir/")),
            StatusEntry::Untracked(path("tree")),
        ];
        let overlap =
            local_change_overlap(&entries, &[diff("f.txt"), diff("dir/a"), diff("tree/leaf")]);
        assert_eq!(
            overlap.paths,
            vec![path("f.txt"), path("dir/"), path("tree")]
        );
        assert!(overlap.staged);
        assert!(!overlap.is_empty());
    }

    #[test]
    fn a_staged_change_alone_blocks_an_in_place_commit() {
        let entries = [ordinary(
            "other.txt",
            ChangeKind::Added,
            ChangeKind::Unmodified,
        )];
        let overlap = local_change_overlap(&entries, &[diff("f.txt")]);
        assert!(overlap.paths.is_empty());
        assert!(overlap.staged);
    }

    #[test]
    fn either_side_of_a_rename_collides() {
        let entries = [StatusEntry::Rename(RenameStatus {
            path: path("new.txt"),
            source: path("f.txt"),
            state: FileState {
                index: ChangeKind::Renamed,
                worktree: ChangeKind::Unmodified,
            },
            submodule: SubmoduleState::default(),
            score: 100,
            copy: false,
        })];
        let overlap = local_change_overlap(&entries, &[diff("f.txt")]);
        assert_eq!(overlap.paths, vec![path("new.txt")]);
    }
}
