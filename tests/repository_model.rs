use devmil_git::activity::ActivityEvent;
use devmil_git::loading::commit::{CommitDiffBase, CommitDiffOptions};
use devmil_git::model::{ReadTicket, RepositoryPhase};
use devmil_git::types::ObjectId;
use devmil_git::{Repository, RepositoryOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        let path = std::env::temp_dir().join(format!(
            "devmil-model-public-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        let fixture = Self(path);
        fixture.git(&["init", "-qb", "main"]);
        fixture
    }
    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.0)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().into()
    }
    fn commit(&self, contents: &[u8]) -> ObjectId {
        std::fs::write(self.0.join("tracked.txt"), contents).unwrap();
        self.git(&["add", "tracked.txt"]);
        self.git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "commit",
        ]);
        ObjectId::parse(self.git(&["rev-parse", "HEAD"])).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[allow(clippy::needless_pass_by_value)] // Consumes the single-result ticket at the test call site.
fn receive<T: Send + Sync + 'static>(ticket: ReadTicket<T>) -> Arc<T> {
    ticket
        .recv_timeout(Duration::from_secs(10))
        .expect("read timed out")
        .expect("read failed")
}
fn ready(repository: &Repository) {
    repository.load().unwrap();
    let mut events = repository.subscribe();
    let deadline = Instant::now() + Duration::from_secs(10);
    while repository.snapshot().phase != RepositoryPhase::Ready {
        assert!(
            Instant::now() < deadline,
            "load timed out: {:?}",
            repository.snapshot()
        );
        events.recv_timeout(Duration::from_millis(100));
    }
}

#[test]
fn public_commit_reads_batch_and_cache_without_extra_git_calls() {
    let fixture = Fixture::new();
    let first = fixture.commit(b"before\n");
    let second = fixture.commit(b"after\n");
    let count = Arc::new(AtomicUsize::new(0));
    let sink_count = count.clone();
    let repository = Repository::new(
        &fixture.0,
        RepositoryOptions {
            watch: false,
            activity_sink: Some(Arc::new(move |event| {
                if matches!(event, ActivityEvent::Created(_)) {
                    sink_count.fetch_add(1, Ordering::SeqCst);
                }
            })),
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    ready(&repository);
    let before = count.load(Ordering::SeqCst);
    let ordered = vec![second.clone(), first, second.clone()];
    let batch = receive(repository.commit_details(ordered.clone()).unwrap());
    assert_eq!(batch.len(), 3);
    assert_eq!(batch[0], batch[2]);
    assert_eq!(count.load(Ordering::SeqCst), before + 1);
    let hit = receive(repository.commit_details(ordered).unwrap());
    assert!(Arc::ptr_eq(&batch, &hit));
    assert_eq!(count.load(Ordering::SeqCst), before + 1);
    let detail = receive(repository.commit_detail(second.clone()).unwrap());
    assert_eq!(detail.commit, second);
    let files = receive(
        repository
            .commit_files(second.clone(), CommitDiffBase::EmptyTree)
            .unwrap(),
    );
    assert_eq!(files.len(), 1);
    let patch = receive(
        repository
            .commit_diff(
                second.clone(),
                CommitDiffBase::EmptyTree,
                CommitDiffOptions::default(),
            )
            .unwrap(),
    );
    assert_eq!(patch.files.len(), 1);
    assert_eq!(count.load(Ordering::SeqCst), before + 6);
    receive(repository.commit_detail(second.clone()).unwrap());
    receive(
        repository
            .commit_files(second.clone(), CommitDiffBase::EmptyTree)
            .unwrap(),
    );
    receive(
        repository
            .commit_diff(
                second,
                CommitDiffBase::EmptyTree,
                CommitDiffOptions::default(),
            )
            .unwrap(),
    );
    assert_eq!(count.load(Ordering::SeqCst), before + 6);
}

#[test]
fn linked_worktrees_keep_status_independent_and_share_repository_identity() {
    let fixture = Fixture::new();
    fixture.commit(b"tracked\n");
    let linked = fixture.0.join("linked");
    fixture.git(&[
        "worktree",
        "add",
        "-qb",
        "linked-branch",
        linked.to_str().unwrap(),
    ]);
    std::fs::write(linked.join("only-linked.txt"), b"linked").unwrap();
    let main = Repository::new(
        &fixture.0,
        RepositoryOptions {
            watch: false,
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    let other = Repository::new(
        &linked,
        RepositoryOptions {
            watch: false,
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    ready(&main);
    ready(&other);
    let main_snapshot = main.snapshot();
    let other_snapshot = other.snapshot();
    let main_identity = &main_snapshot.repository.as_ref().unwrap().worktree;
    let other_identity = &other_snapshot.repository.as_ref().unwrap().worktree;
    assert_eq!(main_identity.repository(), other_identity.repository());
    assert_ne!(main_identity.git_dir(), other_identity.git_dir());
    assert!(!main_snapshot
        .status
        .as_ref()
        .unwrap()
        .entries
        .iter()
        .any(|entry| entry.path().as_path() == Path::new("only-linked.txt")));
    assert!(other_snapshot
        .status
        .as_ref()
        .unwrap()
        .entries
        .iter()
        .any(|entry| entry.path().as_path() == Path::new("only-linked.txt")));
}

#[test]
fn unborn_bare_repository_loads_without_worktree_commands() {
    let fixture = Fixture::new();
    let bare = fixture.0.join("bare.git");
    fixture.git(&["init", "-q", "--bare", bare.to_str().unwrap()]);
    let repository = Repository::new(
        &bare,
        RepositoryOptions {
            watch: false,
            ..RepositoryOptions::default()
        },
    )
    .unwrap();

    ready(&repository);
    let snapshot = repository.snapshot();
    assert!(snapshot.status.is_none());
    assert!(snapshot.error.is_none());
}

#[test]
fn untracked_replacement_of_a_staged_rename_remains_a_file_scoped_addition() {
    use devmil_git::diff::model::{
        ContextLines, DiffFileId, DiffLayout, DiffPath, DiffWhitespacePolicy, FileChangeKind,
    };
    use devmil_git::loading::working_diff::{DiffEndpoint, WorkingDiffKey, WorkingDiffLimits};
    let fixture = Fixture::new();
    fixture.commit(b"original implementation\n");
    fixture.git(&["mv", "tracked.txt", "native.txt"]);
    std::fs::write(
        fixture.0.join("tracked.txt"),
        b"replacement implementation\n",
    )
    .unwrap();
    std::fs::write(fixture.0.join(".gitignore"), b"ignored.txt\n").unwrap();
    std::fs::write(fixture.0.join("ignored.txt"), b"ignored\n").unwrap();
    let repository = Repository::new(
        &fixture.0,
        RepositoryOptions {
            watch: false,
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    ready(&repository);
    let worktree = repository
        .snapshot()
        .repository
        .as_ref()
        .unwrap()
        .worktree
        .clone();
    let key = |path: &[u8]| WorkingDiffKey {
        worktree: worktree.clone(),
        base: DiffEndpoint::Index,
        target: DiffEndpoint::WorkingTree,
        file: Some(DiffFileId {
            old_path: None,
            new_path: Some(DiffPath::from_bytes(path.to_vec()).unwrap()),
        }),
        layout: DiffLayout::Unified,
        context: ContextLines::new(3).unwrap(),
        whitespace: DiffWhitespacePolicy::Preserve,
        force_text: false,
    };
    let result = repository
        .working_diff(key(b"tracked.txt"), WorkingDiffLimits::default())
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    let file = &result.document.files[0];
    assert_eq!(file.kind, FileChangeKind::Untracked);
    assert!(file.id.old_path.is_none());
    assert_eq!(
        file.id.new_path.as_ref().unwrap().as_bytes(),
        b"tracked.txt"
    );
    assert!(result
        .raw_bytes
        .windows(b"replacement implementation".len())
        .any(|window| window == b"replacement implementation"));
    for path in [b"native.txt".as_slice(), b"ignored.txt", b"missing.txt"] {
        assert!(repository
            .working_diff(key(path), WorkingDiffLimits::default())
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .is_err());
    }
    assert_eq!(
        fixture.git(&["show", ":native.txt"]),
        "original implementation"
    );
    repository.close();
}
