mod support;

use devmil_git::rebase::RebaseSequencerMode;
use devmil_git::status::{
    discover_operations, discover_operations_checked, operation_heads, OperationDiscoveryError,
    OperationKind,
};
use support::Fixture;

fn kinds(fixture: &Fixture) -> Vec<OperationKind> {
    discover_operations_checked(&fixture.repo().join(".git"))
        .unwrap()
        .operations
        .iter()
        .map(|operation| operation.kind)
        .collect()
}

fn conflicting_branches(fixture: &Fixture) {
    fixture.commit_file("file.txt", b"base\n", "file base");
    fixture.git(&["checkout", "-qb", "side"]);
    fixture.commit_file("file.txt", b"side\n", "side one");
    fixture.commit_file("other.txt", b"side\n", "side two");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.commit_file("file.txt", b"main\n", "main");
}

#[test]
fn merge_backend_rebase_exposes_sequencer_state() {
    let fixture = Fixture::new("ops-rebase-merge");
    conflicting_branches(&fixture);
    let main = fixture.head();
    fixture.git(&["checkout", "-q", "side"]);
    assert!(!fixture
        .git_status(&["rebase", "--merge", "main"])
        .status
        .success());
    let found = discover_operations_checked(&fixture.repo().join(".git")).unwrap();
    assert_eq!(found.operations[0].kind, OperationKind::Rebase);
    assert_eq!(found.operations[0].current, Some(1));
    assert_eq!(found.operations[0].total, Some(2));
    let rebase = found.rebase.unwrap();
    assert_eq!(rebase.mode(), RebaseSequencerMode::Merge);
    assert_eq!(rebase.onto().unwrap().as_str(), main);
    assert_eq!(rebase.head_name().unwrap().as_bytes(), b"refs/heads/side");
    assert!(!found.mailbox_apply);
    assert_eq!(
        discover_operations(&fixture.repo().join(".git")),
        found.operations
    );
}

#[test]
fn apply_backend_rebase_and_am_are_distinguished() {
    let fixture = Fixture::new("ops-rebase-apply");
    conflicting_branches(&fixture);
    fixture.git(&["checkout", "-q", "side"]);
    assert!(!fixture
        .git_status(&["rebase", "--apply", "main"])
        .status
        .success());
    let found = discover_operations_checked(&fixture.repo().join(".git")).unwrap();
    assert_eq!(kinds(&fixture), [OperationKind::Rebase]);
    let rebase = found.rebase.unwrap();
    assert_eq!(rebase.mode(), RebaseSequencerMode::Apply);
    assert_eq!(rebase.total_steps(), Some(2));
    assert_eq!(rebase.head_name().unwrap().as_bytes(), b"refs/heads/side");
    assert!(!found.mailbox_apply);
    fixture.git(&["rebase", "--abort"]);

    let patch = fixture.git(&["format-patch", "--stdout", "-1", "side~1"]);
    fixture.git(&["checkout", "-q", "main"]);
    std::fs::write(fixture.root().join("one.patch"), patch).unwrap();
    let patch_path = fixture.root().join("one.patch");
    assert!(!fixture
        .git_status(&["am", patch_path.to_str().unwrap()])
        .status
        .success());
    let found = discover_operations_checked(&fixture.repo().join(".git")).unwrap();
    assert!(found.mailbox_apply);
}

#[test]
fn sequencers_and_bisect_use_git_markers() {
    let fixture = Fixture::new("ops-markers");
    conflicting_branches(&fixture);
    assert!(kinds(&fixture).is_empty());
    assert!(!fixture.git_status(&["merge", "side"]).status.success());
    assert_eq!(kinds(&fixture), [OperationKind::Merge]);
    fixture.git(&["merge", "--abort"]);
    assert!(!fixture
        .git_status(&["cherry-pick", "side~1"])
        .status
        .success());
    assert_eq!(kinds(&fixture), [OperationKind::CherryPick]);
    fixture.git(&["cherry-pick", "--abort"]);
    fixture.git(&["bisect", "start"]);
    assert_eq!(kinds(&fixture), [OperationKind::Bisect]);
    assert!(fixture.repo().join(".git/BISECT_START").is_file());
    fixture.git(&["bisect", "reset"]);
    assert!(kinds(&fixture).is_empty());
}

#[test]
fn ambiguous_and_malformed_state_fails_closed() {
    let fixture = Fixture::new("ops-ambiguous");
    let git_dir = fixture.repo().join(".git");
    let head = fixture.head();
    std::fs::create_dir(git_dir.join("rebase-merge")).unwrap();
    std::fs::create_dir(git_dir.join("rebase-apply")).unwrap();
    assert_eq!(
        discover_operations_checked(&git_dir),
        Err(OperationDiscoveryError::Ambiguous)
    );
    // The lenient reader keeps its historical answer.
    assert_eq!(discover_operations(&git_dir)[0].kind, OperationKind::Rebase);
    std::fs::remove_dir(git_dir.join("rebase-apply")).unwrap();
    std::fs::remove_dir(git_dir.join("rebase-merge")).unwrap();

    std::fs::write(git_dir.join("MERGE_HEAD"), format!("{head}\n")).unwrap();
    std::fs::write(git_dir.join("REVERT_HEAD"), format!("{head}\n")).unwrap();
    assert_eq!(
        discover_operations_checked(&git_dir),
        Err(OperationDiscoveryError::Ambiguous)
    );
    std::fs::remove_file(git_dir.join("REVERT_HEAD")).unwrap();
    std::fs::write(git_dir.join("MERGE_HEAD"), b"not an object\n").unwrap();
    assert_eq!(
        discover_operations_checked(&git_dir),
        Err(OperationDiscoveryError::Malformed)
    );
    std::fs::remove_file(git_dir.join("MERGE_HEAD")).unwrap();
    std::fs::create_dir(git_dir.join("CHERRY_PICK_HEAD")).unwrap();
    assert_eq!(
        discover_operations_checked(&git_dir),
        Err(OperationDiscoveryError::UnsafeState)
    );
}

#[cfg(unix)]
#[test]
fn symlinked_state_is_rejected() {
    let fixture = Fixture::new("ops-symlink");
    let git_dir = fixture.repo().join(".git");
    let outside = fixture.root().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, git_dir.join("rebase-merge")).unwrap();
    assert_eq!(
        discover_operations_checked(&git_dir),
        Err(OperationDiscoveryError::UnsafeState)
    );
    std::fs::remove_file(git_dir.join("rebase-merge")).unwrap();
    std::fs::write(outside.join("marker"), fixture.head()).unwrap();
    std::os::unix::fs::symlink(outside.join("marker"), git_dir.join("MERGE_HEAD")).unwrap();
    assert_eq!(
        discover_operations_checked(&git_dir),
        Err(OperationDiscoveryError::UnsafeState)
    );
}

#[test]
fn operation_heads_read_the_applied_commits_strictly() {
    let fixture = Fixture::new("ops-heads");
    conflicting_branches(&fixture);
    let git_dir = fixture.repo().join(".git");
    assert!(operation_heads(&git_dir, OperationKind::Merge)
        .unwrap()
        .is_empty());
    let side = fixture.git(&["rev-parse", "side"]);
    assert!(!fixture.git_status(&["merge", "side"]).status.success());
    let heads = operation_heads(&git_dir, OperationKind::Merge).unwrap();
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].as_str(), side.trim());
    assert!(operation_heads(&git_dir, OperationKind::Revert)
        .unwrap()
        .is_empty());
    assert!(operation_heads(&git_dir, OperationKind::Rebase)
        .unwrap()
        .is_empty());
    std::fs::write(git_dir.join("MERGE_HEAD"), b"not an object\n").unwrap();
    assert_eq!(
        operation_heads(&git_dir, OperationKind::Merge),
        Err(OperationDiscoveryError::Malformed)
    );
}
