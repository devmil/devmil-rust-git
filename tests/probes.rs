mod support;

use devmil_git::probes::{
    has_changes, is_ancestor, is_published, published_commits, resolve, unmerged_index,
    unmerged_paths, ChangeScope, CommitRange, ProbeFailure,
};
use devmil_git::types::ObjectId;
use std::ffi::OsStr;
use support::{executor, Fixture};

fn oid(value: &str) -> ObjectId {
    ObjectId::parse(value.trim()).unwrap()
}

#[test]
fn resolve_and_ancestry_answer_without_errors_for_missing_revisions() {
    let fixture = Fixture::new("probe-resolve");
    let git = OsStr::new("git");
    let repo = fixture.repo();
    let base = oid(&fixture.head());
    let next = oid(&fixture.commit_file("a.txt", b"a\n", "next"));
    let mut run = executor();
    assert_eq!(
        resolve(git, &repo, "HEAD", &mut run).unwrap(),
        Some(next.clone())
    );
    assert_eq!(
        resolve(git, &repo, "HEAD~1^{commit}", &mut run).unwrap(),
        Some(base.clone())
    );
    assert_eq!(
        resolve(git, &repo, "refs/heads/missing", &mut run).unwrap(),
        None
    );
    assert_eq!(
        resolve(git, &repo, "--all", &mut run),
        Err(ProbeFailure::InvalidArgument)
    );
    assert!(is_ancestor(git, &repo, &base, &next, &mut run).unwrap());
    assert!(!is_ancestor(git, &repo, &next, &base, &mut run).unwrap());
    assert!(is_ancestor(git, &repo, &next, &next, &mut run).unwrap());
    let missing = oid(&"1".repeat(40));
    assert!(is_ancestor(git, &repo, &missing, &next, &mut run).is_err());
}

#[test]
fn unborn_head_does_not_resolve() {
    let fixture = Fixture::empty("probe-unborn");
    fixture.git(&["init", "-q"]);
    let mut run = executor();
    assert_eq!(
        resolve(OsStr::new("git"), &fixture.repo(), "HEAD", &mut run).unwrap(),
        None
    );
    fixture.write("new.txt", b"x");
    assert!(has_changes(
        OsStr::new("git"),
        &fixture.repo(),
        ChangeScope::Untracked,
        &mut run
    )
    .unwrap());
    fixture.git(&["add", "new.txt"]);
    assert!(has_changes(
        OsStr::new("git"),
        &fixture.repo(),
        ChangeScope::Staged,
        &mut run
    )
    .unwrap());
}

#[test]
fn change_scopes_are_independent() {
    let fixture = Fixture::new("probe-changes");
    let git = OsStr::new("git");
    let repo = fixture.repo();
    let mut run = executor();
    let all = |run: &mut dyn devmil_git::loading::CommandExecutor| {
        [
            ChangeScope::Staged,
            ChangeScope::Unstaged,
            ChangeScope::Untracked,
        ]
        .map(|scope| has_changes(git, &repo, scope, run).unwrap())
    };
    assert_eq!(all(&mut run), [false, false, false]);
    fixture.write(".gitignore", b"ignored\n");
    fixture.write("ignored", b"x");
    fixture.git(&["add", ".gitignore"]);
    assert_eq!(all(&mut run), [true, false, false]);
    fixture.git(&["commit", "-qm", "ignore"]);
    fixture.write("base.txt", b"changed\n");
    assert_eq!(all(&mut run), [false, true, false]);
    fixture.write("dir/untracked", b"x");
    assert_eq!(all(&mut run), [false, true, true]);
}

#[test]
fn unmerged_paths_lists_each_conflicted_path_once_with_raw_bytes() {
    let fixture = Fixture::new("probe-unmerged");
    let git = OsStr::new("git");
    let name = if cfg!(unix) {
        "conflict \u{e9} path.txt"
    } else {
        "conflict.txt"
    };
    fixture.commit_file(name, b"base\n", "conflict base");
    fixture.git(&["checkout", "-qb", "side"]);
    fixture.commit_file(name, b"side\n", "side");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.commit_file(name, b"main\n", "main");
    let mut run = executor();
    assert!(unmerged_paths(git, &fixture.repo(), &mut run)
        .unwrap()
        .is_empty());
    assert!(!fixture.git_status(&["merge", "side"]).status.success());
    let paths = unmerged_paths(git, &fixture.repo(), &mut run).unwrap();
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].as_bytes(), name.as_bytes());
    let index = unmerged_index(git, &fixture.repo(), &mut run).unwrap();
    let stages = &index[&paths[0]];
    assert!(stages.iter().all(Option::is_some));
    let ours = fixture.git(&["rev-parse", &format!(":2:{name}")]);
    assert_eq!(stages[1].as_ref().unwrap().object.as_str(), ours.trim());
    fixture.git(&["merge", "--abort"]);
    assert!(unmerged_index(git, &fixture.repo(), &mut run)
        .unwrap()
        .is_empty());
}

#[test]
fn published_commits_are_those_reachable_from_remote_tracking_refs() {
    let fixture = Fixture::new("probe-published");
    let git = OsStr::new("git");
    let repo = fixture.repo();
    let base = oid(&fixture.head());
    let one = oid(&fixture.commit_file("one", b"1", "one"));
    // Simulate a fetched remote-tracking ref without a network.
    fixture.git(&["update-ref", "refs/remotes/origin/main", one.as_str()]);
    let two = oid(&fixture.commit_file("two", b"2", "two"));
    let mut run = executor();
    let range = CommitRange {
        base: Some(base.clone()),
        tip: two.clone(),
    };
    assert_eq!(
        published_commits(git, &repo, &range, 10, &mut run).unwrap(),
        vec![one.clone()]
    );
    assert_eq!(
        published_commits(git, &repo, &range, 1, &mut run),
        Err(ProbeFailure::TooMany)
    );
    let whole = CommitRange {
        base: None,
        tip: two.clone(),
    };
    assert_eq!(
        published_commits(git, &repo, &whole, 10, &mut run).unwrap(),
        vec![one.clone(), base.clone()]
    );
    assert!(is_published(git, &repo, &one, &mut run).unwrap());
    assert!(!is_published(git, &repo, &two, &mut run).unwrap());
    let empty = CommitRange {
        base: Some(two.clone()),
        tip: two,
    };
    assert!(published_commits(git, &repo, &empty, 0, &mut run)
        .unwrap()
        .is_empty());
}
