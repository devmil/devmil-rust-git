mod support;

use devmil_git::clone::{
    clone_arguments, init_arguments, CloneDestination, CloneSource, InitialBranch,
};
use devmil_git::commands::bisect::{bisect_arguments, BisectRequest};
use devmil_git::commands::index::{
    checkout_conflict_side, restore_conflict_stages, stage_paths, ConflictSide, IndexStageEntry,
    PathspecForm,
};
use devmil_git::commands::invocation::GitInvocation;
use devmil_git::commands::ref_transaction::{RefExpectation, RefTransaction, RefUpdate};
use devmil_git::commands::submodule::{lfs_arguments, submodule_update_arguments, LfsRequest};
use devmil_git::commands::worktree::add_arguments;
use devmil_git::diff::model::DiffPath;
use devmil_git::rebase::{rebase_environment, rebase_onto_upstream_argv};
use devmil_git::types::{ObjectId, RefId, RepositoryPath, RepositoryRelativePath};
use std::ffi::{OsStr, OsString};
use std::path::Path;
use support::{executor, succeeded, Fixture};

fn run_in(directory: &Path, invocation: &GitInvocation) -> bool {
    succeeded(&executor()(
        invocation.process_command(OsStr::new("git"), directory),
    ))
}

fn run_os(directory: &Path, arguments: Vec<OsString>) -> bool {
    run_in(directory, &GitInvocation::visible(arguments))
}

fn oid(value: &str) -> ObjectId {
    ObjectId::parse(value.trim()).unwrap()
}

#[test]
fn init_uses_end_of_options_and_clone_checks_out_a_branch() {
    let fixture = Fixture::empty("init-clone");
    let destination =
        CloneDestination::new(RepositoryPath::new(fixture.root().join("-dash repo")).unwrap())
            .unwrap();
    let branch = InitialBranch::parse("trunk").unwrap();
    let arguments = init_arguments(&destination, false, Some(&branch));
    assert_eq!(arguments[3], "--");
    assert!(run_os(fixture.root(), arguments));
    let created = fixture.root().join("-dash repo");
    assert_eq!(
        support::git_in(&created, &["symbolic-ref", "HEAD"]),
        b"refs/heads/trunk\n"
    );
    let bare =
        CloneDestination::new(RepositoryPath::new(fixture.root().join("bare")).unwrap()).unwrap();
    assert!(run_os(fixture.root(), init_arguments(&bare, true, None)));
    assert!(fixture.root().join("bare/HEAD").is_file());

    support::git_in(&created, &["commit", "--allow-empty", "-qm", "one"]);
    support::git_in(&created, &["branch", "side"]);
    let source = CloneSource::local(&RepositoryPath::new(&created).unwrap()).unwrap();
    let clone =
        CloneDestination::new(RepositoryPath::new(fixture.root().join("clone")).unwrap()).unwrap();
    let side = InitialBranch::parse("side").unwrap();
    assert!(run_os(
        fixture.root(),
        clone_arguments(&source, &clone, Some(&side))
    ));
    assert_eq!(
        support::git_in(&fixture.root().join("clone"), &["symbolic-ref", "HEAD"]),
        b"refs/heads/side\n"
    );
}

#[test]
fn worktree_add_submodule_update_and_lfs() {
    let fixture = Fixture::new("extensions");
    let linked = fixture.root().join("linked");
    assert!(run_os(
        &fixture.repo(),
        add_arguments(&linked, "HEAD").unwrap()
    ));
    assert!(linked.join("base.txt").is_file());
    assert!(add_arguments(Path::new("relative"), "HEAD").is_err());
    assert!(add_arguments(&fixture.root().join("x"), "--detach").is_err());

    let library = Fixture::new("extensions-library");
    let url = library.repo().to_string_lossy().into_owned();
    fixture.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "-q",
        &url,
        "lib",
    ]);
    fixture.git(&["commit", "-qm", "submodule"]);
    fixture.git(&["submodule", "deinit", "-q", "--all"]);
    assert!(!fixture.repo().join("lib/base.txt").exists());
    let mut arguments: Vec<OsString> = vec!["-c".into(), "protocol.file.allow=always".into()];
    arguments.extend(submodule_update_arguments(
        &RepositoryRelativePath::new("lib").unwrap(),
        true,
    ));
    assert!(run_os(&fixture.repo(), arguments));
    assert!(fixture.repo().join("lib/base.txt").is_file());

    let lfs_available = std::process::Command::new("git")
        .args(["lfs", "version"])
        .output()
        .is_ok_and(|output| output.status.success());
    if lfs_available {
        assert!(run_os(
            &fixture.repo(),
            lfs_arguments(&LfsRequest::Track("*.bin".into())).unwrap()
        ));
        assert!(String::from_utf8(fixture.read(".gitattributes"))
            .unwrap()
            .contains("*.bin filter=lfs"));
        assert!(run_os(
            &fixture.repo(),
            lfs_arguments(&LfsRequest::Untrack("*.bin".into())).unwrap()
        ));
        assert!(!String::from_utf8(fixture.read(".gitattributes"))
            .unwrap()
            .contains("*.bin"));
    }
}

#[test]
fn bisect_session_runs_from_start_to_reset() {
    let fixture = Fixture::new("bisect");
    let good = oid(&fixture.head());
    fixture.commit_file("a", b"1", "one");
    fixture.commit_file("a", b"2", "two");
    let bad = oid(&fixture.commit_file("a", b"3", "three"));
    let repo = fixture.repo();
    let run =
        |request: BisectRequest| run_in(&repo, &GitInvocation::visible(bisect_arguments(&request)));
    assert!(run(BisectRequest::Start {
        bad: bad.clone(),
        good
    }));
    assert!(repo.join(".git/BISECT_START").is_file());
    assert!(run(BisectRequest::Skip));
    assert!(run(BisectRequest::Bad));
    assert!(run(BisectRequest::Reset));
    assert!(!repo.join(".git/BISECT_START").exists());
    assert_eq!(oid(&fixture.head()), bad);
}

#[test]
fn restack_replays_only_the_branch_commits_and_ignores_autostash() {
    let fixture = Fixture::new("restack");
    fixture.git(&["checkout", "-qb", "parent"]);
    let old_parent = fixture.commit_file("p", b"1", "parent one");
    fixture.git(&["checkout", "-qb", "child"]);
    fixture.commit_file("c", b"1", "child one");
    fixture.git(&["checkout", "-q", "parent"]);
    fixture.git(&["commit", "-q", "--amend", "-m", "parent rewritten"]);
    fixture.git(&["config", "rebase.autoStash", "true"]);
    fixture.write("base.txt", b"dirty\n");
    let argv = rebase_onto_upstream_argv("parent", &old_parent, "child").unwrap();
    let environment = rebase_environment();
    let mut invocation = GitInvocation::visible(argv.clone());
    for (name, value) in &environment {
        invocation = invocation.env(name, value);
    }
    // Auto-stash is pinned off, so the dirty worktree refuses the rebase.
    assert!(!run_in(&fixture.repo(), &invocation));
    fixture.git(&["checkout", "--", "base.txt"]);
    assert!(run_in(&fixture.repo(), &invocation));
    assert_eq!(
        fixture.git(&["log", "--format=%s", "parent..child"]),
        "child one\n"
    );
    assert!(rebase_onto_upstream_argv("parent", "--exec=x", "child").is_err());
}

#[test]
fn conflict_side_checkout_staging_and_stage_restore() {
    let fixture = Fixture::new("conflict-index");
    fixture.commit_file("f", b"base\n", "base f");
    fixture.git(&["checkout", "-qb", "side"]);
    fixture.commit_file("f", b"theirs\n", "theirs");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.commit_file("f", b"ours\n", "ours");
    assert!(!fixture.git_status(&["merge", "side"]).status.success());
    let stages: Vec<(u32, ObjectId)> = fixture
        .git(&["ls-files", "-s", "--", "f"])
        .lines()
        .map(|line| {
            let mut fields = line.split_whitespace();
            let mode = u32::from_str_radix(fields.next().unwrap(), 8).unwrap();
            (mode, oid(fields.next().unwrap()))
        })
        .collect();
    let path = DiffPath::from_bytes(b"f".to_vec()).unwrap();
    assert!(run_in(
        &fixture.repo(),
        &checkout_conflict_side(&path, ConflictSide::Theirs).unwrap()
    ));
    assert_eq!(fixture.read("f"), b"theirs\n");
    assert!(run_in(
        &fixture.repo(),
        &stage_paths(std::slice::from_ref(&path), PathspecForm::Arguments).unwrap()
    ));
    assert_eq!(fixture.git(&["ls-files", "-u"]), "");
    let entry = |index: usize| {
        Some(IndexStageEntry {
            mode: stages[index].0,
            object: stages[index].1.clone(),
        })
    };
    assert!(run_in(
        &fixture.repo(),
        &restore_conflict_stages(&path, &[entry(0), entry(1), entry(2)]).unwrap()
    ));
    assert_eq!(fixture.git(&["ls-files", "-u"]).lines().count(), 3);
    assert!(run_in(
        &fixture.repo(),
        &checkout_conflict_side(&path, ConflictSide::Ours).unwrap()
    ));
    assert_eq!(fixture.read("f"), b"ours\n");
}

#[test]
fn ref_transactions_apply_atomically_with_compare_and_swap() {
    let fixture = Fixture::new("ref-transaction");
    let base = oid(&fixture.head());
    let next = oid(&fixture.commit_file("n", b"n", "next"));
    let reference = |name: &str| RefId::parse(name).unwrap();
    let mut transaction = RefTransaction::new();
    transaction
        .push(RefUpdate::Update {
            reference: reference("refs/heads/created"),
            new: base.clone(),
            expected: RefExpectation::Missing,
        })
        .unwrap()
        .push(RefUpdate::Update {
            reference: reference("refs/heads/main"),
            new: base.clone(),
            expected: RefExpectation::Value(next.clone()),
        })
        .unwrap();
    assert!(run_in(&fixture.repo(), &transaction.invocation().unwrap()));
    assert_eq!(
        oid(&fixture.git(&["rev-parse", "refs/heads/created"])),
        base
    );
    assert_eq!(oid(&fixture.head()), base);

    // A stale expectation rejects the whole transaction.
    let mut stale = RefTransaction::new();
    stale
        .push(RefUpdate::Delete {
            reference: reference("refs/heads/created"),
            expected: RefExpectation::Any,
        })
        .unwrap()
        .push(RefUpdate::Update {
            reference: reference("refs/heads/main"),
            new: next.clone(),
            expected: RefExpectation::Value(next.clone()),
        })
        .unwrap();
    assert!(!run_in(&fixture.repo(), &stale.invocation().unwrap()));
    assert!(fixture
        .git_status(&["show-ref", "--verify", "--quiet", "refs/heads/created"])
        .status
        .success());
    let mut verify = RefTransaction::new();
    verify
        .push(RefUpdate::Verify {
            reference: reference("refs/heads/missing"),
            expected: RefExpectation::Missing,
        })
        .unwrap()
        .push(RefUpdate::Delete {
            reference: reference("refs/heads/created"),
            expected: RefExpectation::Value(base),
        })
        .unwrap();
    assert!(run_in(&fixture.repo(), &verify.invocation().unwrap()));
    assert!(!fixture
        .git_status(&["show-ref", "--verify", "--quiet", "refs/heads/created"])
        .status
        .success());
}
