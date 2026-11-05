use devmil_git::{
    diff::model::{
        ContextLines, DiffFallback, DiffFileId, DiffLayout, DiffPath, DiffWhitespacePolicy,
    },
    loading::{
        commit::{CommitDiffBase, CommitDiffOptions},
        comparison::{ComparisonLimits, ComparisonQuery},
        diff_preview::MAX_INLINE_FILE_BYTES,
        working_diff::{DiffEndpoint, WorkingDiffKey, WorkingDiffLimits},
    },
    types::ObjectId,
    Repository, RepositoryOptions,
};
use std::{fs, path::PathBuf, process::Command, time::Duration};

struct Fixture(PathBuf);
impl Fixture {
    fn git(&self, arguments: &[&str]) -> Vec<u8> {
        let result = Command::new("git")
            .current_dir(&self.0)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        result.stdout
    }
    fn commit(&self) -> ObjectId {
        self.git(&["add", "."]);
        self.git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--no-gpg-sign",
            "-qm",
            "fixture",
        ]);
        ObjectId::parse(
            String::from_utf8(self.git(&["rev-parse", "HEAD"]))
                .unwrap()
                .trim()
                .to_owned(),
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn large_sides_show_information_in_working_staged_history_and_comparison_diffs() {
    let fixture =
        Fixture(std::env::temp_dir().join(format!("large-diff-info-{}", std::process::id())));
    fs::create_dir_all(&fixture.0).unwrap();
    fixture.git(&["init", "-b", "main"]);
    fs::write(fixture.0.join("small.txt"), b"before\n").unwrap();
    let base = fixture.commit();
    let large = fixture.0.join("release[1].apk");
    fs::File::create(&large)
        .unwrap()
        .set_len(MAX_INLINE_FILE_BYTES + 1)
        .unwrap();
    let repository = Repository::new(
        &fixture.0,
        RepositoryOptions {
            watch: false,
            polling_fallback: false,
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    repository.load().unwrap();
    wait_ready(&repository, 0);
    let worktree = repository
        .snapshot()
        .repository
        .as_ref()
        .unwrap()
        .worktree
        .clone();
    let path = DiffPath::from_bytes(b"release[1].apk".to_vec()).unwrap();
    let key = WorkingDiffKey {
        worktree,
        base: DiffEndpoint::Index,
        target: DiffEndpoint::WorkingTree,
        file: Some(DiffFileId {
            old_path: None,
            new_path: Some(path.clone()),
        }),
        layout: DiffLayout::Unified,
        context: ContextLines::DEFAULT,
        whitespace: DiffWhitespacePolicy::Preserve,
        force_text: false,
    };
    let information = repository
        .working_diff(key.clone(), WorkingDiffLimits::default())
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert_eq!(information.document.files.len(), 1);
    assert!(
        matches!(information.document.files[0].fallback, Some(DiffFallback::TooLarge { byte_count, preview_line_limit: 0, .. }) if byte_count == MAX_INLINE_FILE_BYTES + 1)
    );
    assert!(information.document.files[0].hunks.is_empty());

    fs::write(fixture.0.join("small.txt"), b"after\n").unwrap();
    let target = fixture.commit();
    for whitespace in [
        DiffWhitespacePolicy::Preserve,
        DiffWhitespacePolicy::IgnoreAll,
    ] {
        let diff = repository
            .commit_diff(
                target.clone(),
                CommitDiffBase::FirstParent(base.clone()),
                CommitDiffOptions {
                    whitespace,
                    ..CommitDiffOptions::default()
                },
            )
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert_eq!(diff.files.len(), 2);
        assert!(diff
            .files
            .iter()
            .any(|file| file.fallback.is_some() && file.hunks.is_empty()));
        assert!(diff.files.iter().any(|file| !file.hunks.is_empty()));
        for (base, target) in [
            (base.clone(), target.clone()),
            (target.clone(), base.clone()),
        ] {
            let comparison = repository
                .comparison(
                    ComparisonQuery {
                        base,
                        target,
                        context: ContextLines::DEFAULT,
                        whitespace,
                        layout: DiffLayout::SideBySide,
                    },
                    ComparisonLimits::default(),
                )
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap();
            let diff = &comparison.outcome.as_ref().unwrap().diff;
            assert_eq!(diff.files.len(), 2);
            assert_eq!(diff.layout, DiffLayout::SideBySide);
            assert!(diff
                .files
                .iter()
                .any(|file| matches!(file.fallback, Some(DiffFallback::TooLarge { .. }))));
        }
    }
    fs::OpenOptions::new()
        .write(true)
        .open(&large)
        .unwrap()
        .set_len(MAX_INLINE_FILE_BYTES + 2)
        .unwrap();
    fs::write(fixture.0.join("small.txt"), b"working\n").unwrap();
    let revision = repository.snapshot().revision;
    repository.refresh().unwrap();
    wait_ready(&repository, revision);
    let tracked = WorkingDiffKey {
        file: None,
        ..key.clone()
    };
    let diff = repository
        .working_diff(tracked.clone(), WorkingDiffLimits::default())
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert_eq!(diff.document.files.len(), 2);
    assert!(diff
        .document
        .files
        .iter()
        .any(|file| matches!(file.fallback, Some(DiffFallback::TooLarge { .. }))));
    fixture.git(&["add", "."]);
    let revision = repository.snapshot().revision;
    repository.refresh().unwrap();
    wait_ready(&repository, revision);
    let staged = WorkingDiffKey {
        base: DiffEndpoint::Head,
        target: DiffEndpoint::Index,
        ..tracked
    };
    let diff = repository
        .working_diff(staged, WorkingDiffLimits::default())
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert_eq!(diff.document.files.len(), 2);
    assert!(diff
        .document
        .files
        .iter()
        .any(|file| matches!(file.fallback, Some(DiffFallback::TooLarge { .. }))));
    repository.close();
}

fn wait_ready(repository: &Repository, after: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while repository.snapshot().phase != devmil_git::model::RepositoryPhase::Ready
        || repository.snapshot().revision <= after
    {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
}
