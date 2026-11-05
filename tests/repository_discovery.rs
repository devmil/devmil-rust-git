use devmil_git::activity::{ActivityId, ActivityRecord, ActivityStatus};
use devmil_git::process::{ActivityGitRunner, GitProcessCommand, ProcessArgument};
use devmil_git::repository::{
    DiscoveryErrorKind, DiscoveryRequest, RepositoryDiscovery, RepositoryKind,
};
use devmil_git::types::{HeadState, RepositoryPath};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "devmil-discovery-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }

    fn git(&self, args: &[&str]) {
        let record = execute(
            GitProcessCommand::git("git")
                .current_dir(&self.0)
                .args(args.iter().copied().map(ProcessArgument::visible)),
        );
        assert!(
            matches!(record.status, ActivityStatus::Succeeded { .. }),
            "{record:?}"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn execute(command: GitProcessCommand) -> ActivityRecord {
    ActivityGitRunner::run(command, ActivityId::new(1).unwrap(), None, &mut |_| {})
}

fn request(path: &Path) -> DiscoveryRequest {
    DiscoveryRequest::new(RepositoryPath::new(path).unwrap())
}

#[test]
fn discovers_unborn_nested_file_bare_and_linked_worktree_identities() {
    let fixture = Fixture::new();
    fixture.git(&["init", "--quiet", "--initial-branch=main"]);
    let discovery = RepositoryDiscovery::default();
    let original = discovery
        .discover(&request(&fixture.0), &mut execute)
        .unwrap();
    assert_eq!(original.kind, RepositoryKind::Worktree);
    assert!(matches!(original.head, HeadState::Unborn { .. }));
    std::fs::create_dir(fixture.0.join("nested")).unwrap();
    let file = fixture.0.join("nested/file.txt");
    std::fs::write(&file, b"x").unwrap();
    assert_eq!(
        discovery
            .discover(&request(&file), &mut execute)
            .unwrap()
            .worktree,
        original.worktree
    );
    let mut exact = request(&file);
    exact.discover_parents = false;
    assert_eq!(
        discovery.discover(&exact, &mut execute).unwrap_err().kind,
        DiscoveryErrorKind::ParentDiscoveryDisabled
    );

    fixture.git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "--quiet",
        "--allow-empty",
        "--no-gpg-sign",
        "-m",
        "initial",
    ]);
    fixture.git(&["worktree", "add", "--quiet", "-b", "linked", "linked"]);
    let linked = discovery
        .discover(&request(&fixture.0.join("linked")), &mut execute)
        .unwrap();
    assert_eq!(linked.worktree.repository(), original.worktree.repository());
    assert_ne!(linked.worktree.git_dir(), original.worktree.git_dir());
    assert!(matches!(linked.head, HeadState::Attached { .. }));
    fixture.git(&["init", "--quiet", "--bare", "bare.git"]);
    let bare = discovery
        .discover(&request(&fixture.0.join("bare.git")), &mut execute)
        .unwrap();
    assert_eq!(bare.kind, RepositoryKind::Bare);
    assert_eq!(bare.worktree.worktree_root(), bare.worktree.git_dir());
}

#[test]
fn discovery_maps_missing_git_cancellation_and_truncation_without_exposing_stderr() {
    let fixture = Fixture::new();
    let missing = RepositoryDiscovery::new(fixture.0.join("missing-git"));
    assert_eq!(
        missing
            .discover(&request(&fixture.0), &mut execute)
            .unwrap_err()
            .kind,
        DiscoveryErrorKind::GitMissing
    );
    for (status, truncated, expected) in [
        (
            ActivityStatus::Canceled { exit_code: None },
            false,
            DiscoveryErrorKind::Canceled,
        ),
        (
            ActivityStatus::Succeeded { exit_code: 0 },
            true,
            DiscoveryErrorKind::InvalidGitOutput,
        ),
    ] {
        let failure = RepositoryDiscovery::default()
            .discover(&request(&fixture.0), &mut |command| {
                let mut record = ActivityRecord::started(
                    ActivityId::new(1).unwrap(),
                    command.activity_command(),
                    0,
                );
                record.stdout.truncated = truncated;
                record.stderr.bytes = b"fixture-private-diagnostic".to_vec();
                record.finish(status.clone(), 1);
                record
            })
            .unwrap_err();
        assert_eq!(failure.kind, expected);
        assert!(!format!("{failure:?}").contains("fixture-private-diagnostic"));
    }
}

// macOS filesystems reject non-UTF-8 names at creation, before Git is involved.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn discovery_preserves_non_utf8_native_repository_paths() {
    use std::os::unix::ffi::OsStringExt;
    let fixture = Fixture::new();
    let repository = fixture
        .0
        .join(std::ffi::OsString::from_vec(b"repo-\xff".to_vec()));
    std::fs::create_dir(&repository).unwrap();
    let record = execute(
        GitProcessCommand::git("git")
            .current_dir(&repository)
            .args([
                ProcessArgument::visible("init"),
                ProcessArgument::visible("--quiet"),
            ]),
    );
    assert!(matches!(record.status, ActivityStatus::Succeeded { .. }));
    let discovered = RepositoryDiscovery::default()
        .discover(&request(&repository), &mut execute)
        .unwrap();
    assert_eq!(
        discovered.worktree.worktree_root().as_path(),
        std::fs::canonicalize(repository).unwrap()
    );
}
