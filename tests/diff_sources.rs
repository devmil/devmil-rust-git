//! Exact diff source identity against real Git: object IDs from the diff read,
//! raw blob and worktree reads that run no repository-configured command, and
//! verification that rejects drift.
mod support;

use devmil_git::diff::model::{
    ContextLines, DiffFileId, DiffLayout, DiffPath, DiffWhitespacePolicy,
};
use devmil_git::diff::source::{
    patch_object_ids, verify_displayed_lines, DiffSide, SourceMismatch,
};
use devmil_git::loading::commit::{CommitDiffBase, CommitDiffOptions};
use devmil_git::loading::working_diff::{DiffEndpoint, WorkingDiffKey, WorkingDiffLimits};
use devmil_git::types::ObjectId;
use devmil_git::{Repository, RepositoryErrorKind, RepositoryOptions};
use std::time::Duration;
use support::Fixture;

const TIMEOUT: Duration = Duration::from_secs(20);

fn repository(fixture: &Fixture) -> Repository {
    let repository = Repository::new(
        fixture.repo(),
        RepositoryOptions {
            watch: false,
            polling_fallback: false,
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    repository.load().unwrap();
    for _ in 0..200 {
        if repository.snapshot().repository.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    repository
}

/// Configures a textconv driver and a smudge/clean filter whose commands
/// write `marker` when they run.
fn configure_marker_commands(fixture: &Fixture) -> std::path::PathBuf {
    let marker = fixture.root().join("marker");
    let command = format!("sh -c 'echo ran >> \"{}\"; cat'", marker.display());
    fixture.write(".gitattributes", b"*.rs diff=marked filter=marked\n");
    fixture.git(&["config", "diff.marked.textconv", &format!("{command} <")]);
    fixture.git(&["config", "filter.marked.smudge", &command]);
    fixture.git(&["config", "filter.marked.clean", &command]);
    marker
}

#[test]
#[cfg(unix)]
fn diff_sources_are_exact_stored_bytes_without_configured_commands() {
    let fixture = Fixture::new("diff-sources");
    let old_source = b"/* opened\n   closed */\nfn old() {}\nfn same() {}\n".as_slice();
    let new_source = b"/* opened\n   closed */\nfn new() {}\nfn same() {}\n".as_slice();
    fixture.commit_file("src/a.rs", old_source, "old");
    let base = fixture.head();
    let marker = configure_marker_commands(&fixture);
    fixture.commit_file("src/a.rs", new_source, "new");
    let commit = fixture.head();
    let repository = repository(&fixture);
    // Loading status may legitimately clean worktree files; only the reads
    // below must not run a configured command.
    for _ in 0..200 {
        if repository.snapshot().status.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = std::fs::remove_file(&marker);

    let document = repository
        .commit_diff(
            ObjectId::parse(commit).unwrap(),
            CommitDiffBase::FirstParent(ObjectId::parse(base).unwrap()),
            CommitDiffOptions::default(),
        )
        .unwrap()
        .recv_timeout(TIMEOUT)
        .unwrap()
        .unwrap();
    let file = document
        .files
        .iter()
        .find(|file| file.id.new_path.as_ref().map(DiffPath::as_bytes) == Some(b"src/a.rs"))
        .unwrap();
    let ids = patch_object_ids(file).unwrap();
    let read = |side: DiffSide| {
        repository
            .raw_blob(ids.side(side).unwrap().clone(), 1024)
            .unwrap()
            .recv_timeout(TIMEOUT)
            .unwrap()
            .unwrap()
    };
    let old = read(DiffSide::Old);
    let new = read(DiffSide::New);
    assert_eq!(old.as_slice(), old_source);
    assert_eq!(new.as_slice(), new_source);
    assert_eq!(
        verify_displayed_lines(file, DiffSide::Old, &old).unwrap(),
        [1, 2, 3, 4].into()
    );
    assert_eq!(
        verify_displayed_lines(file, DiffSide::New, &new).unwrap(),
        [1, 2, 3, 4].into()
    );
    assert_eq!(
        verify_displayed_lines(file, DiffSide::New, &old),
        Err(SourceMismatch::ChangedLine)
    );
    assert!(!marker.exists(), "a configured command ran");

    // A byte bound and a missing object are explicit failures.
    let small = repository
        .raw_blob(ids.new.clone().unwrap(), 4)
        .unwrap()
        .recv_timeout(TIMEOUT)
        .unwrap();
    assert_eq!(small.unwrap_err().kind, RepositoryErrorKind::TooLarge);
    let missing = repository
        .raw_blob(ObjectId::parse("1".repeat(40)).unwrap(), 1024)
        .unwrap()
        .recv_timeout(TIMEOUT)
        .unwrap();
    assert_eq!(
        missing.unwrap_err().kind,
        RepositoryErrorKind::MissingObject
    );
    assert!(!marker.exists(), "a configured command ran");
}

#[test]
#[cfg(unix)]
fn worktree_sources_are_proven_against_the_accepted_diff() {
    let fixture = Fixture::new("worktree-sources");
    fixture.commit_file("a.rs", b"fn a() {}\nfn b() {}\n", "base");
    fixture.write("a.rs", b"fn a() {}\nfn c() {}\n");
    let repository = repository(&fixture);
    let worktree = repository
        .snapshot()
        .repository
        .as_ref()
        .unwrap()
        .worktree
        .clone();
    let path = DiffPath::from_bytes(b"a.rs".to_vec()).unwrap();
    let information = repository
        .working_diff(
            WorkingDiffKey {
                worktree,
                base: DiffEndpoint::Index,
                target: DiffEndpoint::WorkingTree,
                file: Some(DiffFileId {
                    old_path: Some(path.clone()),
                    new_path: Some(path.clone()),
                }),
                layout: DiffLayout::Unified,
                context: ContextLines::DEFAULT,
                whitespace: DiffWhitespacePolicy::Preserve,
                force_text: false,
            },
            WorkingDiffLimits::default(),
        )
        .unwrap()
        .recv_timeout(TIMEOUT)
        .unwrap()
        .unwrap();
    let file = &information.document.files[0];
    let ids = patch_object_ids(file).unwrap();
    let index = repository
        .raw_blob(ids.old.clone().unwrap(), 1024)
        .unwrap()
        .recv_timeout(TIMEOUT)
        .unwrap()
        .unwrap();
    assert_eq!(index.as_slice(), b"fn a() {}\nfn b() {}\n");
    let read = || {
        repository
            .worktree_source(path.clone(), 1024)
            .unwrap()
            .recv_timeout(TIMEOUT)
            .unwrap()
    };
    let current = read().unwrap();
    assert!(verify_displayed_lines(file, DiffSide::New, &current).is_ok());

    // An equal-size edit with the same length is rejected by content.
    fixture.write("a.rs", b"fn a() {}\nfn d() {}\n");
    let drifted = read().unwrap();
    assert_eq!(
        verify_displayed_lines(file, DiffSide::New, &drifted),
        Err(SourceMismatch::ChangedLine)
    );

    // A symlink is never followed.
    std::fs::remove_file(fixture.repo().join("a.rs")).unwrap();
    std::os::unix::fs::symlink("/etc/hosts", fixture.repo().join("a.rs")).unwrap();
    assert_eq!(
        read().unwrap_err().kind,
        RepositoryErrorKind::InvalidOptions
    );
    std::fs::remove_file(fixture.repo().join("a.rs")).unwrap();
    assert_eq!(read().unwrap_err().kind, RepositoryErrorKind::MissingObject);
}
