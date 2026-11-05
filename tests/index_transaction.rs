mod support;

use devmil_git::commands::index::{read_tree_empty, stage_paths, PathspecForm};
use devmil_git::commands::invocation::GitInvocation;
use devmil_git::diff::model::DiffPath;
use devmil_git::index_transaction::{read_index_bytes, IndexTransaction, IndexTransactionError};
use std::ffi::OsStr;
use support::{executor, succeeded, Fixture};

fn run(fixture: &Fixture, invocation: &GitInvocation) -> bool {
    succeeded(&executor()(
        invocation.process_command(OsStr::new("git"), &fixture.repo()),
    ))
}

fn path(value: &str) -> DiffPath {
    DiffPath::from_bytes(value.as_bytes().to_vec()).unwrap()
}

#[test]
fn stages_privately_and_installs_only_over_the_expected_index() {
    let fixture = Fixture::new("index-transaction");
    let git_dir = fixture.repo().join(".git");
    fixture.write("a.txt", b"a\n");
    let transaction = IndexTransaction::begin_current(&git_dir).unwrap();
    let invocation =
        transaction.wrap(stage_paths(&[path("a.txt")], PathspecForm::Arguments).unwrap());
    assert_eq!(
        invocation.display_arguments()[..2],
        ["-c", "core.splitIndex=false"]
    );
    assert!(run(&fixture, &invocation));
    // The real index is untouched until installation.
    assert_eq!(fixture.git(&["diff", "--cached", "--name-only"]), "");
    transaction.install().unwrap();
    assert_eq!(fixture.git(&["diff", "--cached", "--name-only"]), "a.txt\n");
    assert!(std::fs::read_dir(&git_dir).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with("devmil-git-index-")));

    // A concurrent index change makes installation fail and keeps the newer index.
    fixture.write("b.txt", b"b\n");
    let transaction = IndexTransaction::begin_current(&git_dir).unwrap();
    assert!(run(
        &fixture,
        &transaction.wrap(stage_paths(&[path("b.txt")], PathspecForm::Arguments).unwrap())
    ));
    fixture.git(&["reset", "-q"]);
    let after_reset = read_index_bytes(&git_dir.join("index")).unwrap();
    assert_eq!(transaction.install(), Err(IndexTransactionError::Changed));
    assert_eq!(
        read_index_bytes(&git_dir.join("index")).unwrap(),
        after_reset
    );
    assert!(!git_dir.join("index.lock").exists());

    // A held lock is reported and left alone.
    let transaction = IndexTransaction::begin_current(&git_dir).unwrap();
    std::fs::write(git_dir.join("index.lock"), b"held").unwrap();
    assert_eq!(transaction.install(), Err(IndexTransactionError::Locked));
    assert_eq!(std::fs::read(git_dir.join("index.lock")).unwrap(), b"held");
}

#[test]
fn split_index_repositories_install_a_self_contained_index() {
    let fixture = Fixture::new("index-split");
    fixture.git(&["config", "core.splitIndex", "true"]);
    fixture.git(&["update-index", "--split-index"]);
    let git_dir = fixture.repo().join(".git");
    fixture.write("c.txt", b"c\n");
    let transaction = IndexTransaction::begin_current(&git_dir).unwrap();
    assert!(run(
        &fixture,
        &transaction.wrap(stage_paths(&[path("c.txt")], PathspecForm::StdinNul).unwrap())
    ));
    transaction.install().unwrap();
    assert_eq!(fixture.git(&["diff", "--cached", "--name-only"]), "c.txt\n");
    assert!(fixture.git_status(&["status"]).status.success());
}

#[test]
fn missing_index_starts_empty() {
    let fixture = Fixture::empty("index-missing");
    fixture.git(&["init", "-q"]);
    fixture.write("d.txt", b"d\n");
    let git_dir = fixture.repo().join(".git");
    assert!(!git_dir.join("index").exists());
    let transaction = IndexTransaction::begin_current(&git_dir).unwrap();
    assert!(transaction.starts_empty());
    assert!(run(&fixture, &transaction.wrap(read_tree_empty())));
    assert!(run(
        &fixture,
        &transaction.wrap(stage_paths(&[path("d.txt")], PathspecForm::Arguments).unwrap())
    ));
    transaction.install().unwrap();
    assert_eq!(fixture.git(&["ls-files"]), "d.txt\n");
}
