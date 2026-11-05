//! Forced text diffs against real Git: files Git considers binary, by content
//! or by attributes, return text hunks only when the caller asks for them.
mod support;

use devmil_git::diff::model::{
    ContextLines, DiffDocument, DiffFallback, DiffFileId, DiffLayout, DiffLineOrigin, DiffPath,
    DiffWhitespacePolicy,
};
use devmil_git::loading::commit::{CommitDiffBase, CommitDiffOptions, CommitDiffScope};
use devmil_git::loading::working_diff::{DiffEndpoint, WorkingDiffKey, WorkingDiffLimits};
use devmil_git::types::ObjectId;
use devmil_git::{Repository, RepositoryOptions};
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
        if repository.snapshot().status.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    repository
}

fn added_sources(document: &DiffDocument) -> Vec<Vec<u8>> {
    let [file] = document.files.as_slice() else {
        panic!("expected one file");
    };
    assert_eq!(file.fallback, None);
    file.hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter(|line| line.origin == DiffLineOrigin::Addition)
        .map(|line| line.source.clone())
        .collect()
}

#[test]
fn forced_text_diffs_files_git_considers_binary() {
    let fixture = Fixture::new("force-text");
    fixture.write(".gitattributes", b"*.dat binary\n");
    fixture.commit_file("marked.dat", b"old line\n", "marked");
    fixture.commit_file("zero.bin", b"old\0line\n", "zero");
    let base = fixture.head();
    fixture.write("marked.dat", b"new line\n");
    fixture.write("zero.bin", b"new\0line\n");
    fixture.git(&["add", "marked.dat", "zero.bin"]);
    fixture.git(&["commit", "-q", "-m", "changed"]);
    let commit = fixture.head();
    fixture.write("marked.dat", b"new worktree line\n");
    fixture.write("zero.bin", b"new\0worktree\n");
    let repository = repository(&fixture);

    for (name, expected_commit, expected_worktree) in [
        (
            b"marked.dat".as_slice(),
            b"new line".as_slice(),
            b"new worktree line".as_slice(),
        ),
        (b"zero.bin", b"new\0line", b"new\0worktree"),
    ] {
        let path = DiffPath::from_bytes(name.to_vec()).unwrap();
        let commit_diff = |force_text| {
            repository
                .commit_diff(
                    ObjectId::parse(&commit).unwrap(),
                    CommitDiffBase::FirstParent(ObjectId::parse(&base).unwrap()),
                    CommitDiffOptions {
                        scope: CommitDiffScope::File {
                            old_path: path.clone(),
                            new_path: path.clone(),
                        },
                        force_text,
                        ..CommitDiffOptions::default()
                    },
                )
                .unwrap()
                .recv_timeout(TIMEOUT)
                .unwrap()
                .unwrap()
        };
        let detected = commit_diff(false);
        assert!(matches!(
            detected.files[0].fallback,
            Some(DiffFallback::Binary { .. })
        ));
        assert_eq!(added_sources(&commit_diff(true)), [expected_commit]);

        let working_diff = |force_text| {
            repository
                .working_diff(
                    WorkingDiffKey {
                        worktree: repository
                            .snapshot()
                            .repository
                            .as_ref()
                            .unwrap()
                            .worktree
                            .clone(),
                        base: DiffEndpoint::Index,
                        target: DiffEndpoint::WorkingTree,
                        file: Some(DiffFileId {
                            old_path: Some(path.clone()),
                            new_path: Some(path.clone()),
                        }),
                        layout: DiffLayout::Unified,
                        context: ContextLines::DEFAULT,
                        whitespace: DiffWhitespacePolicy::Preserve,
                        force_text,
                    },
                    WorkingDiffLimits::default(),
                )
                .unwrap()
                .recv_timeout(TIMEOUT)
                .unwrap()
                .unwrap()
                .document
                .clone()
        };
        assert!(matches!(
            working_diff(false).files[0].fallback,
            Some(DiffFallback::Binary { .. })
        ));
        assert_eq!(added_sources(&working_diff(true)), [expected_worktree]);
    }
}
