mod support;

#[cfg(unix)]
use devmil_git::commands::commit::{classify_commit_stderr, CommitFailureKind, CommitTrace};
use devmil_git::commands::commit::{
    commit_invocation, commit_invocation_with_message, CommitMessageSource, CommitRequest,
    CommitSigning,
};
use devmil_git::commands::index::{
    apply_to_index, read_tree_empty, stage_paths, unstage_paths, PathspecForm, UnstageBase,
};
use devmil_git::commands::invocation::GitInvocation;
use devmil_git::commands::restore::{
    apply_patch, clean_paths, discard_worktree_paths, restore_worktree_from, ApplyOptions,
};
use devmil_git::diff::model::DiffPath;
use devmil_git::process::PrivateActivityResult;
use devmil_git::types::ObjectId;
use std::ffi::OsStr;
use support::{executor, succeeded, Fixture};

fn run(fixture: &Fixture, invocation: &GitInvocation) -> PrivateActivityResult {
    let command = invocation.process_command(OsStr::new("git"), &fixture.repo());
    assert_eq!(
        command.activity_command(),
        invocation.activity_command(OsStr::new("git"), &fixture.repo())
    );
    executor()(command)
}

fn path(value: &str) -> DiffPath {
    DiffPath::from_bytes(value.as_bytes().to_vec()).unwrap()
}

fn staged(fixture: &Fixture) -> String {
    fixture.git(&["diff", "--cached", "--name-only"])
}

#[test]
fn commit_amend_allow_empty_and_file_message() {
    let fixture = Fixture::new("commit-basic");
    fixture.write("a.txt", b"a\n");
    fixture.git(&["add", "a.txt"]);
    let request = CommitRequest {
        signing: CommitSigning::Disable,
        ..CommitRequest::default()
    };
    let result = run(
        &fixture,
        &commit_invocation_with_message(&request, b"first subject\n\nbody\n".to_vec()).unwrap(),
    );
    assert!(succeeded(&result));
    assert_eq!(
        fixture.git(&["log", "-1", "--format=%B"]),
        "first subject\n\nbody\n\n"
    );
    // Nothing staged: a plain commit fails, --allow-empty succeeds.
    assert!(!succeeded(&run(
        &fixture,
        &commit_invocation_with_message(&request, b"empty\n".to_vec()).unwrap()
    )));
    let empty = CommitRequest {
        allow_empty: true,
        ..request.clone()
    };
    assert!(succeeded(&run(
        &fixture,
        &commit_invocation_with_message(&empty, b"empty\n".to_vec()).unwrap()
    )));
    let count = fixture.git(&["rev-list", "--count", "HEAD"]);
    let message = fixture.root().join("message.txt");
    std::fs::write(&message, b"amended\n").unwrap();
    let amend = CommitRequest {
        amend: true,
        allow_empty: true,
        message: CommitMessageSource::File(message),
        author: Some("Other Person <other@example.invalid>".into()),
        ..request
    };
    let invocation = commit_invocation(&amend).unwrap();
    assert_eq!(invocation.display_arguments()[1], "--file=<message>");
    assert!(succeeded(&run(&fixture, &invocation)));
    assert_eq!(fixture.git(&["rev-list", "--count", "HEAD"]), count);
    assert_eq!(
        fixture.git(&["log", "-1", "--format=%s|%an"]),
        "amended|Other Person\n"
    );
}

#[cfg(unix)]
#[test]
fn hook_and_signing_failures_are_classified_from_trace2() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new("commit-trace");
    let hook = fixture.repo().join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\nexit 3\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    fixture.write("a.txt", b"a\n");
    fixture.git(&["add", "a.txt"]);
    let request = CommitRequest {
        signing: CommitSigning::Disable,
        ..CommitRequest::default()
    };
    let trace = CommitTrace::create(fixture.root()).unwrap();
    let invocation =
        trace.invocation(commit_invocation_with_message(&request, b"m\n".to_vec()).unwrap());
    let result = run(&fixture, &invocation);
    assert!(!succeeded(&result));
    let failure = trace.failure().unwrap();
    assert_eq!(failure.kind, CommitFailureKind::Hook);
    assert_eq!(failure.exit_code, Some(3));
    assert_eq!(
        classify_commit_stderr(&result.stderr.bytes, false),
        CommitFailureKind::Hook
    );
    let trace_path = trace.path().to_path_buf();
    drop(trace);
    assert!(!trace_path.exists());

    // --no-verify skips the hook; a failing signing program is a signing failure.
    let gpg = fixture.root().join("fake-gpg");
    std::fs::write(
        &gpg,
        "#!/bin/sh\necho 'gpg: signing failed: test' >&2\nexit 2\n",
    )
    .unwrap();
    std::fs::set_permissions(&gpg, std::fs::Permissions::from_mode(0o755)).unwrap();
    fixture.git(&["config", "gpg.program", gpg.to_str().unwrap()]);
    fixture.git(&["config", "user.signingKey", "TESTKEY"]);
    let signed = CommitRequest {
        no_verify: true,
        signing: CommitSigning::Require,
        ..CommitRequest::default()
    };
    let trace = CommitTrace::create(fixture.root()).unwrap();
    let result = run(
        &fixture,
        &trace.invocation(commit_invocation_with_message(&signed, b"m\n".to_vec()).unwrap()),
    );
    assert!(!succeeded(&result));
    assert_eq!(trace.failure().unwrap().kind, CommitFailureKind::Signing);
    assert_eq!(
        classify_commit_stderr(&result.stderr.bytes, true),
        CommitFailureKind::Signing
    );
}

#[test]
fn staging_both_pathspec_forms_and_literal_names() {
    let fixture = Fixture::new("stage");
    fixture.write("[o]ther.txt", b"bracket\n");
    fixture.write("other.txt", b"other\n");
    fixture.write("dir/nested.txt", b"nested\n");
    for form in [PathspecForm::Arguments, PathspecForm::StdinNul] {
        assert!(succeeded(&run(
            &fixture,
            &stage_paths(&[path("[o]ther.txt"), path("dir")], form).unwrap()
        )));
        // The literal name stages only itself.
        assert_eq!(staged(&fixture), "[o]ther.txt\ndir/nested.txt\n");
        assert!(succeeded(&run(
            &fixture,
            &unstage_paths(&[path("[o]ther.txt"), path("dir")], UnstageBase::Head, form).unwrap()
        )));
        assert_eq!(staged(&fixture), "");
    }
    // Deletions are staged too.
    std::fs::remove_file(fixture.repo().join("base.txt")).unwrap();
    assert!(succeeded(&run(
        &fixture,
        &stage_paths(&[path("base.txt")], PathspecForm::Arguments).unwrap()
    )));
    assert_eq!(staged(&fixture), "base.txt\n");
}

#[test]
fn unborn_unstage_and_patch_staging() {
    let fixture = Fixture::empty("stage-unborn");
    fixture.git(&["init", "-q"]);
    fixture.write("new.txt", b"one\ntwo\n");
    fixture.git(&["add", "new.txt"]);
    assert!(succeeded(&run(
        &fixture,
        &unstage_paths(
            &[path("new.txt")],
            UnstageBase::Unborn,
            PathspecForm::StdinNul
        )
        .unwrap()
    )));
    assert_eq!(staged(&fixture), "");
    std::fs::remove_file(fixture.repo().join(".git/index")).unwrap();
    assert!(succeeded(&run(&fixture, &read_tree_empty())));
    fixture.git(&["add", "new.txt"]);
    fixture.git(&["commit", "-qm", "base"]);
    // A zero-context patch, as row-level staging produces, applies to the index only.
    fixture.write("new.txt", b"one\nchanged\nmore\n");
    let zero_context = fixture.git(&["diff", "-U0", "--", "new.txt"]).into_bytes();
    assert!(succeeded(&run(&fixture, &apply_to_index(zero_context))));
    assert_eq!(fixture.git(&["show", ":new.txt"]), "one\nchanged\nmore\n");
    assert_eq!(fixture.git(&["diff", "--name-only"]), "");
}

#[test]
fn discard_restore_clean_and_apply() {
    let fixture = Fixture::new("restore");
    let base = ObjectId::parse(fixture.head()).unwrap();
    fixture.commit_file("base.txt", b"second\n", "second");
    fixture.write("base.txt", b"dirty\n");
    assert!(succeeded(&run(
        &fixture,
        &discard_worktree_paths(&[path("base.txt")], PathspecForm::StdinNul).unwrap()
    )));
    assert_eq!(fixture.read("base.txt"), b"second\n");
    assert!(succeeded(&run(
        &fixture,
        &restore_worktree_from(&base, &[path("base.txt")], PathspecForm::Arguments).unwrap()
    )));
    assert_eq!(fixture.read("base.txt"), b"base\n");
    assert_eq!(staged(&fixture), "");
    let patch = fixture.git(&["diff"]).into_bytes();
    fixture.git(&["checkout", "--", "base.txt"]);
    let check = ApplyOptions {
        check: true,
        unidiff_zero: false,
    };
    assert!(succeeded(&run(
        &fixture,
        &apply_patch(patch.clone(), check)
    )));
    assert_eq!(fixture.read("base.txt"), b"second\n");
    assert!(succeeded(&run(
        &fixture,
        &apply_patch(patch.clone(), ApplyOptions::default())
    )));
    assert_eq!(fixture.read("base.txt"), b"base\n");
    assert!(!succeeded(&run(&fixture, &apply_patch(patch, check))));
    fixture.write("junk/file", b"x");
    fixture.write("keep", b"x");
    assert!(succeeded(&run(
        &fixture,
        &clean_paths(&[path("junk")]).unwrap()
    )));
    assert!(!fixture.repo().join("junk").exists());
    assert!(fixture.repo().join("keep").exists());
}
