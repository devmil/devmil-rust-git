mod support;

use devmil_git::git_locks::{inspect_git_locks, is_lock_conflict, remove_git_lock, GitLockLimits};
use devmil_git::types::{RepositoryId, RepositoryPath, WorktreeId};
use support::Fixture;

#[test]
fn a_held_ref_lock_fails_git_with_a_recognised_conflict_until_removed() {
    let fixture = Fixture::new("held-ref-lock");
    let head = fixture.head();
    fixture.git(&["update-ref", "refs/remotes/origin/feature/x", &head]);
    let lock = fixture
        .repo()
        .join(".git/refs/remotes/origin/feature/x.lock");
    std::fs::write(&lock, b"").unwrap();

    let failed = fixture.git_status(&["update-ref", "-d", "refs/remotes/origin/feature/x"]);
    assert!(!failed.status.success());
    assert!(is_lock_conflict(&failed.stderr));

    let git_dir = RepositoryPath::new(fixture.repo().join(".git")).unwrap();
    let worktree = WorktreeId::new(
        RepositoryId::new(git_dir.clone()),
        RepositoryPath::new(fixture.repo()).unwrap(),
        git_dir,
    );
    let inspection = inspect_git_locks(&worktree, GitLockLimits::default()).unwrap();
    let [stale] = inspection.locks.as_slice() else {
        panic!("one lock expected");
    };
    remove_git_lock(stale).unwrap();
    assert!(fixture
        .git_status(&["update-ref", "-d", "refs/remotes/origin/feature/x"])
        .status
        .success());
}
