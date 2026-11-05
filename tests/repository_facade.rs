mod support;

use devmil_git::model::{RepositoryErrorKind, RepositoryPhase};
use devmil_git::status::StatusEntry;
use devmil_git::watch::TemporaryPathRules;
use devmil_git::{Repository, RepositoryOptions};
use std::time::{Duration, Instant};
use support::Fixture;

fn ready(repository: &Repository) {
    repository.load().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while repository.snapshot().phase != RepositoryPhase::Ready {
        assert!(Instant::now() < deadline, "load timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn paths(entries: &[StatusEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| entry.path().as_path().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn fresh_status_under_lease_sees_writes_the_snapshot_does_not() {
    let fixture = Fixture::new("facade-fresh");
    let repository = Repository::new(
        fixture.repo(),
        RepositoryOptions {
            watch: false,
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    ready(&repository);
    let before = std::fs::read(fixture.repo().join(".git/index")).unwrap();
    let lease = repository.begin_mutation().unwrap();
    fixture.write("new.txt", b"x");
    let status = lease.fresh_status(None).unwrap().unwrap();
    assert_eq!(paths(&status.entries), ["new.txt"]);
    assert!(repository
        .snapshot()
        .status
        .as_ref()
        .unwrap()
        .entries
        .is_empty());
    // The read does not rewrite the user's index.
    assert_eq!(
        std::fs::read(fixture.repo().join(".git/index")).unwrap(),
        before
    );
    assert_eq!(
        repository
            .wait_until_idle(Duration::ZERO, Duration::from_millis(50))
            .unwrap_err()
            .kind,
        RepositoryErrorKind::InvalidOptions
    );
    drop(lease);
    let snapshot = repository
        .wait_until_idle(Duration::from_millis(50), Duration::from_secs(10))
        .unwrap();
    assert_eq!(
        paths(&snapshot.status.as_ref().unwrap().entries),
        ["new.txt"]
    );
}

#[test]
fn wait_until_idle_absorbs_watched_writes_and_honours_temporary_rules() {
    let fixture = Fixture::new("facade-idle");
    let repository = Repository::new(
        fixture.repo(),
        RepositoryOptions {
            polling_fallback: true,
            debounce: Duration::from_millis(20),
            temporary_paths: TemporaryPathRules {
                prefixes: vec![".gitlama-".into()],
                suffixes: Vec::new(),
            },
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    ready(&repository);
    let settled = repository
        .wait_until_idle(Duration::from_millis(200), Duration::from_secs(20))
        .unwrap();
    fixture.write("watched.txt", b"x");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let snapshot = repository
            .wait_until_idle(Duration::from_millis(200), Duration::from_secs(20))
            .unwrap();
        if paths(&snapshot.status.as_ref().unwrap().entries) == ["watched.txt"] {
            break;
        }
        assert!(Instant::now() < deadline, "watched write never refreshed");
    }
    let revision = repository.snapshot().revision;
    assert!(revision > settled.revision);
    // A configured temporary file does not refresh the snapshot.
    fixture.write(".gitlama-patch-1", b"x");
    std::thread::sleep(Duration::from_millis(500));
    let after = repository
        .wait_until_idle(Duration::from_millis(200), Duration::from_secs(20))
        .unwrap();
    assert_eq!(after.revision, revision);
}
