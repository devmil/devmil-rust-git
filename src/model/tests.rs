use super::*;
use std::fs;
use std::process::Command;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

static NEXT: AtomicU64 = AtomicU64::new(1);

#[cfg(unix)]
#[test]
fn dropping_last_read_ticket_reaps_child_and_skips_abandoned_queued_work() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let script = fixture.0.join("wait.sh");
    fs::write(
        &script,
        "#!/bin/sh\necho $$ > pid\nwhile :; do /bin/sleep 0.02; done\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let repository = fixture.repository(false);
    let root = fixture.0.clone();
    let first = repository
        .schedule(
            b"long-child".to_vec(),
            8,
            false,
            move |_, executor, cancellation| {
                let result = executor
                    .execute(crate::process::GitProcessCommand::git(script).current_dir(root));
                assert!(cancellation.is_cancelled());
                assert!(matches!(
                    result.activity.status,
                    crate::activity::ActivityStatus::Canceled { .. }
                ));
                Err::<u8, _>(RepositoryError::new(
                    RepositoryErrorKind::Canceled,
                    "canceled",
                ))
            },
        )
        .unwrap();
    let second: ReadTicket<u8> = repository
        .schedule(b"long-child".to_vec(), 8, false, |_, _, _| {
            panic!("duplicate read must share execution")
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fixture.0.join("pid").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let skipped = Arc::new(AtomicBool::new(false));
    let executed = Arc::clone(&skipped);
    let abandoned = repository
        .schedule(b"abandoned".to_vec(), 8, false, move |_, _, _| {
            executed.store(true, Ordering::Release);
            Ok(1_u8)
        })
        .unwrap();
    drop(abandoned);
    drop(first);
    assert!(second.recv_timeout(Duration::from_millis(30)).is_none());
    drop(second);
    let last = repository
        .schedule(b"after-cancellation".to_vec(), 8, false, |_, _, _| Ok(9_u8))
        .unwrap();
    assert_eq!(
        *last.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(),
        9
    );
    assert!(!skipped.load(Ordering::Acquire));
    let pid = fs::read_to_string(fixture.0.join("pid")).unwrap();
    assert!(!Command::new("kill")
        .args(["-0", pid.trim()])
        .output()
        .unwrap()
        .status
        .success());
}

struct OverflowWatcher {
    overflow: Arc<AtomicBool>,
    registered: Arc<AtomicBool>,
}
impl crate::watch::PlatformWatcherPort for OverflowWatcher {
    fn watch(
        &self,
        _: &crate::watch::RepositoryWatchPlan,
    ) -> Result<Box<dyn crate::watch::RepositoryWatcherPort>, crate::watch::WatchError> {
        self.registered.store(true, Ordering::Release);
        Ok(Box::new(Self {
            overflow: Arc::clone(&self.overflow),
            registered: Arc::clone(&self.registered),
        }))
    }
}
impl crate::watch::RepositoryWatcherPort for OverflowWatcher {
    fn wait(&mut self, _: Duration) -> Result<crate::watch::WatchBatch, crate::watch::WatchError> {
        Ok(crate::watch::WatchBatch {
            changes: Vec::new(),
            overflow: self.overflow.swap(false, Ordering::AcqRel),
        })
    }
}

#[test]
fn watcher_overflow_event_precedes_recovery_snapshot() {
    let fixture = Fixture::new();
    let overflow = Arc::new(AtomicBool::new(false));
    let registered = Arc::new(AtomicBool::new(false));
    let repository = Repository::new(
        &fixture.0,
        RepositoryOptions {
            watcher_factory: Some(Arc::new(OverflowWatcher {
                overflow: Arc::clone(&overflow),
                registered: Arc::clone(&registered),
            })),
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    repository.load().unwrap();
    let first_ready = wait_for(&repository, |snapshot| {
        snapshot.phase == RepositoryPhase::Ready
    });
    let ready = wait_for(&repository, |snapshot| {
        registered.load(Ordering::Acquire)
            && snapshot.phase == RepositoryPhase::Ready
            && snapshot.revision > first_ready.revision
    });
    let mut events = repository.subscribe();
    while events.try_recv().is_some() {}
    overflow.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut observed_overflow = false;
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(100)) {
            Some(RepositoryEvent::WatchOverflow { revision }) => {
                assert_eq!(revision, ready.revision);
                observed_overflow = true;
            }
            Some(RepositoryEvent::Snapshot(snapshot)) if snapshot.revision > ready.revision => {
                assert!(observed_overflow, "recovery was published before overflow");
                if snapshot.phase == RepositoryPhase::Ready {
                    return;
                }
            }
            _ => {}
        }
    }
    panic!("overflow recovery was not published");
}
#[test]
fn overflow_covered_by_a_requested_refresh_raises_no_warning() {
    let fixture = Fixture::new();
    let overflow = Arc::new(AtomicBool::new(false));
    let registered = Arc::new(AtomicBool::new(false));
    let repository = Repository::new(
        &fixture.0,
        RepositoryOptions {
            watcher_factory: Some(Arc::new(OverflowWatcher {
                overflow: Arc::clone(&overflow),
                registered: Arc::clone(&registered),
            })),
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    repository.load().unwrap();
    let first_ready = wait_for(&repository, |snapshot| {
        snapshot.phase == RepositoryPhase::Ready
    });
    let ready = wait_for(&repository, |snapshot| {
        registered.load(Ordering::Acquire)
            && snapshot.phase == RepositoryPhase::Ready
            && snapshot.revision > first_ready.revision
    });
    let mut events = repository.subscribe();
    while events.try_recv().is_some() {}
    // A checkout floods the watcher while the lease parks the worker.
    let lease = repository.begin_mutation().unwrap();
    overflow.store(true, Ordering::Release);
    drop(lease);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(100)) {
            Some(RepositoryEvent::WatchOverflow { .. }) => {
                panic!("the lease's own refresh already rescans what the watcher lost")
            }
            Some(RepositoryEvent::Snapshot(snapshot))
                if snapshot.revision > ready.revision
                    && snapshot.phase == RepositoryPhase::Ready =>
            {
                assert!(
                    !overflow.load(Ordering::Acquire),
                    "overflow was not consumed"
                );
                return;
            }
            _ => {}
        }
    }
    panic!("the mutation refresh was not published");
}

#[test]
fn reload_is_answered_ahead_of_reads_queued_before_it() {
    let fixture = Fixture::new();
    let repository = fixture.repository(false);
    repository.load().unwrap();
    wait_for(&repository, |snapshot| {
        snapshot.phase == RepositoryPhase::Ready
    });
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let lease = repository.begin_mutation().unwrap();
    // A slow read the window asked for while the mutation ran.
    let slow = repository
        .schedule(b"slow".to_vec(), 32, false, move |_, _, _| {
            let _ = release_rx.recv_timeout(Duration::from_secs(10));
            Ok(())
        })
        .unwrap();
    fs::write(fixture.0.join("pulled.txt"), b"new").unwrap();
    drop(lease);
    let reloaded = repository
        .reload()
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .expect("reload waited for the read queued ahead of it")
        .unwrap();
    assert!(reloaded
        .status
        .as_ref()
        .unwrap()
        .entries
        .iter()
        .any(|entry| entry.path().as_path() == std::path::Path::new("pulled.txt")));
    assert!(slow.try_recv().is_none(), "the slow read is still running");
    release_tx.send(()).unwrap();
    slow.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "devmil-model-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let output = Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(output.status.success());
        Self(root)
    }
    fn repository(&self, watch: bool) -> Repository {
        Repository::new(
            &self.0,
            RepositoryOptions {
                watch,
                ..RepositoryOptions::default()
            },
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn wait_for(
    repository: &Repository,
    test: impl Fn(&RepositorySnapshot) -> bool,
) -> Arc<RepositorySnapshot> {
    let mut subscription = repository.subscribe();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let snapshot = repository.snapshot();
        if test(&snapshot) {
            return snapshot;
        }
        let _ = subscription.recv_timeout(Duration::from_millis(100));
    }
    panic!(
        "repository did not reach expected state: {:?}",
        repository.snapshot()
    );
}

#[test]
fn construction_load_and_close_have_explicit_lifecycle() {
    let fixture = Fixture::new();
    let repository = fixture.repository(false);
    assert_eq!(repository.snapshot().phase, RepositoryPhase::Unloaded);
    repository.load().unwrap();
    let ready = wait_for(&repository, |snapshot| {
        snapshot.phase == RepositoryPhase::Ready
    });
    assert!(ready.repository.is_some());
    assert!(ready.status.as_ref().unwrap().entries.is_empty());
    let revision = ready.revision;
    repository.load().unwrap();
    assert_eq!(repository.snapshot().revision, revision);
    repository.close();
    assert_eq!(repository.snapshot().phase, RepositoryPhase::Closed);
    assert_eq!(
        repository.load().unwrap_err().kind,
        RepositoryErrorKind::Closed
    );
}

#[test]
fn native_watching_refreshes_status_without_application_polling() {
    let fixture = Fixture::new();
    let repository = fixture.repository(true);
    repository.load().unwrap();
    let ready = wait_for(&repository, |snapshot| {
        snapshot.phase == RepositoryPhase::Ready
    });
    assert!(ready.watch_error.is_none());
    fs::write(fixture.0.join("new.txt"), b"changed").unwrap();
    let changed = wait_for(&repository, |snapshot| {
        snapshot.status.as_ref().is_some_and(|status| {
            status
                .entries
                .iter()
                .any(|entry| entry.path().as_path() == std::path::Path::new("new.txt"))
        })
    });
    assert!(changed.revision > ready.revision);
}

#[test]
fn duplicate_inflight_reads_and_cache_hits_execute_once() {
    let fixture = Fixture::new();
    let repository = fixture.repository(false);
    let calls = Arc::new(AtomicUsize::new(0));
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let (continue_tx, continue_rx) = std::sync::mpsc::sync_channel(1);
    let called = Arc::clone(&calls);
    let first = repository
        .schedule(b"same".to_vec(), 128, true, move |_, _, _| {
            called.fetch_add(1, Ordering::Relaxed);
            started_tx.send(()).unwrap();
            continue_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(42_u64)
        })
        .unwrap();
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let second = repository
        .schedule(
            b"same".to_vec(),
            128,
            true,
            |_, _, _| -> Result<u64, RepositoryError> { panic!("duplicate job executed") },
        )
        .unwrap();
    continue_tx.send(()).unwrap();
    let first = first.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    let second = second
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    let cached = repository
        .schedule(
            b"same".to_vec(),
            128,
            true,
            |_, _, _| -> Result<u64, RepositoryError> { panic!("cached job executed") },
        )
        .unwrap();
    assert!(Arc::ptr_eq(
        &first,
        &cached
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap()
    ));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn lagging_subscriber_receives_current_resynchronization_snapshot() {
    let fixture = Fixture::new();
    let repository = Repository::new(
        &fixture.0,
        RepositoryOptions {
            watch: false,
            event_capacity: 1,
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    let mut events = repository.subscribe();
    assert!(events.try_recv().is_some());
    repository.load().unwrap();
    let ready = wait_for(&repository, |snapshot| {
        snapshot.phase == RepositoryPhase::Ready
    });
    assert!(
        matches!(events.try_recv(),Some(RepositoryEvent::Resynchronize(snapshot)) if snapshot.revision == ready.revision)
    );
}

#[test]
fn closing_unloaded_repository_publishes_closed() {
    let fixture = Fixture::new();
    let repository = fixture.repository(false);
    repository.close();
    assert_eq!(repository.snapshot().phase, RepositoryPhase::Closed);
}

fn cached_counter(
    repository: &Repository,
    key: &[u8],
    charge: usize,
    calls: &Arc<AtomicUsize>,
) -> usize {
    let calls = Arc::clone(calls);
    *repository
        .schedule(key.to_vec(), charge, true, move |_, _, _| {
            Ok(calls.fetch_add(1, Ordering::Relaxed))
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap()
}

#[test]
fn cache_evicts_least_recent_entry_and_never_retains_oversized_values() {
    let fixture = Fixture::new();
    let repository = Repository::new(
        &fixture.0,
        RepositoryOptions {
            watch: false,
            cache_entries: 2,
            cache_bytes: 4096,
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    assert_eq!(cached_counter(&repository, b"a", 32, &calls), 0);
    assert_eq!(cached_counter(&repository, b"b", 32, &calls), 1);
    assert_eq!(cached_counter(&repository, b"a", 32, &calls), 0);
    assert_eq!(cached_counter(&repository, b"c", 32, &calls), 2);
    assert_eq!(cached_counter(&repository, b"b", 32, &calls), 3);
    assert_eq!(cached_counter(&repository, b"large", 8192, &calls), 4);
    assert_eq!(cached_counter(&repository, b"large", 8192, &calls), 5);
}

#[test]
fn explicit_reload_invalidates_even_object_keyed_cached_reads() {
    let fixture = Fixture::new();
    let repository = fixture.repository(false);
    let calls = Arc::new(AtomicUsize::new(0));
    assert_eq!(cached_counter(&repository, b"object", 32, &calls), 0);
    repository
        .reload()
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert_eq!(cached_counter(&repository, b"object", 32, &calls), 1);
}

#[test]
fn close_cancels_active_work_and_resolves_queued_tickets() {
    let fixture = Fixture::new();
    let repository = fixture.repository(false);
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let active = repository
        .schedule(b"active".to_vec(), 1, false, move |_, _, cancellation| {
            started_tx.send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !cancellation.is_cancelled() {
                assert!(
                    Instant::now() < deadline,
                    "close never canceled active read"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            Err::<u64, _>(RepositoryError::new(
                RepositoryErrorKind::Canceled,
                "canceled",
            ))
        })
        .unwrap();
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let pending = repository
        .schedule(
            b"pending".to_vec(),
            1,
            false,
            |_, _, _| -> Result<u64, RepositoryError> { panic!("queued read ran after close") },
        )
        .unwrap();
    repository.close();
    assert_eq!(
        active
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap_err()
            .kind,
        RepositoryErrorKind::Canceled
    );
    assert_eq!(
        pending
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap_err()
            .kind,
        RepositoryErrorKind::Closed
    );
}

#[test]
fn subscribers_observe_the_same_publication_without_consuming_each_others_events() {
    let fixture = Fixture::new();
    let repository = fixture.repository(false);
    let mut first = repository.subscribe();
    let mut second = repository.subscribe();
    first.try_recv();
    second.try_recv();
    repository.load().unwrap();
    let ready = wait_for(&repository, |value| value.phase == RepositoryPhase::Ready);
    for subscriber in [&mut first, &mut second] {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline);
            if matches!(subscriber.recv_timeout(Duration::from_millis(100)), Some(RepositoryEvent::Snapshot(value)) if value.revision == ready.revision)
            {
                break;
            }
        }
    }
}

#[test]
fn mutation_lease_blocks_reads_rejects_nesting_and_refreshes_before_resuming() {
    let fixture = Fixture::new();
    let repository = fixture.repository(false);
    let calls = Arc::new(AtomicUsize::new(0));
    assert_eq!(cached_counter(&repository, b"before", 32, &calls), 0);
    let lease = repository.begin_mutation().unwrap();
    assert!(
        matches!(repository.begin_mutation(), Err(error) if error.kind == RepositoryErrorKind::InvalidOptions)
    );
    fs::write(fixture.0.join("mutation.txt"), b"changed").unwrap();
    let queued = repository
        .schedule(b"before".to_vec(), 32, true, |repository, _, _| {
            Ok(repository
                .worktree
                .worktree_root()
                .as_path()
                .join("mutation.txt")
                .exists())
        })
        .unwrap();
    assert!(queued.recv_timeout(Duration::from_millis(50)).is_none());
    drop(lease);
    assert!(*queued
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap());
    assert!(repository
        .snapshot()
        .status
        .as_ref()
        .unwrap()
        .entries
        .iter()
        .any(|entry| entry.path().as_path() == std::path::Path::new("mutation.txt")));
}

#[test]
fn mutation_acquisition_waits_for_active_work_and_honors_cancellation() {
    let fixture = Fixture::new();
    let repository = Arc::new(fixture.repository(false));
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let read = repository
        .schedule(b"running".to_vec(), 1, false, move |_, _, _| {
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(())
        })
        .unwrap();
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let waiting_model = repository.clone();
    let waiting_cancel = cancel.clone();
    let waiter = std::thread::spawn(move || {
        waiting_model
            .begin_mutation_with_cancellation(&*waiting_cancel)
            .err()
            .unwrap()
            .kind
    });
    cancel.store(true, Ordering::Release);
    assert_eq!(waiter.join().unwrap(), RepositoryErrorKind::Canceled);
    release_tx.send(()).unwrap();
    read.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    let lease = repository.begin_mutation().unwrap();
    let closing = repository.clone();
    std::thread::spawn(move || closing.close()).join().unwrap();
    assert_eq!(repository.snapshot().phase, RepositoryPhase::Closed);
    drop(lease);
}

#[test]
fn mutation_lease_is_not_granted_until_an_active_read_finishes() {
    let fixture = Fixture::new();
    let repository = Arc::new(fixture.repository(false));
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let read = repository
        .schedule(b"lease-ordering".to_vec(), 1, false, move |_, _, _| {
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(())
        })
        .unwrap();
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (acquired_tx, acquired_rx) = std::sync::mpsc::sync_channel(1);
    let waiting = Arc::clone(&repository);
    let waiter = std::thread::spawn(move || {
        let _lease = waiting.begin_mutation().unwrap();
        acquired_tx.send(()).unwrap();
    });
    assert!(acquired_rx.recv_timeout(Duration::from_millis(50)).is_err());
    release_tx.send(()).unwrap();
    read.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    acquired_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    waiter.join().unwrap();
}

#[test]
fn initial_load_and_refresh_in_one_iteration_release_reads() {
    let fixture = Fixture::new();
    let repository = Repository::new(
        &fixture.0,
        RepositoryOptions {
            watch: false,
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    // Hold the mutation lease so both requests reach the worker together.
    let lease = repository.begin_mutation().unwrap();
    repository.load().unwrap();
    repository.refresh().unwrap();
    drop(lease);
    let read = repository
        .schedule(b"coalesced-load".to_vec(), 1, false, |_, _, _| Ok(7_u8))
        .unwrap();
    assert_eq!(
        *read.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(),
        7
    );
    repository.close();
}

#[test]
fn immutable_history_passes_a_blocked_initial_status_but_mutable_reads_wait() {
    use crate::activity::ActivityEvent;
    use crate::loading::graph::{GraphHistoryOrder, GraphTraversalScope};
    let fixture = Fixture::new();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let blocked = Arc::new(AtomicBool::new(false));
    let sink_gate = Arc::clone(&gate);
    let sink_blocked = Arc::clone(&blocked);
    let repository = Repository::new(
        &fixture.0,
        RepositoryOptions {
            watch: false,
            refresh_read_policy: RefreshReadPolicy::ConcurrentImmutable,
            activity_sink: Some(Arc::new(move |event| {
                if let ActivityEvent::Created(record) = event {
                    if record
                        .command
                        .arguments
                        .first()
                        .is_some_and(|arg| arg.display == "status")
                    {
                        sink_blocked.store(true, Ordering::Release);
                        let (lock, ready) = &*sink_gate;
                        let released = lock.lock().unwrap();
                        // Bound the fixture even if an assertion in its owner fails.
                        drop(
                            ready
                                .wait_timeout_while(released, Duration::from_secs(10), |value| {
                                    !*value
                                })
                                .unwrap(),
                        );
                    }
                }
            })),
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    repository.load().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !blocked.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let mutable = repository
        .schedule(b"mutable".to_vec(), 1, false, |_, _, _| Ok(7_u8))
        .unwrap();
    let history = repository
        .history(
            GraphHistoryOrder::Topological,
            GraphTraversalScope::AllReferences,
        )
        .unwrap();
    let loaded = history.recv_timeout(Duration::from_secs(3));
    let mutable_before_release = mutable.try_recv();
    let loading = repository.snapshot().phase;
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    assert!(loaded.expect("history must not wait for status").is_ok());
    assert!(mutable_before_release.is_none());
    assert_eq!(loading, RepositoryPhase::Loading);
    assert_eq!(
        *mutable
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap(),
        7
    );
    assert_eq!(repository.snapshot().phase, RepositoryPhase::Ready);
    repository.close();
}

fn committed_fixture() -> (Fixture, impl Fn(&[&str]) -> Vec<u8>) {
    let fixture = Fixture::new();
    let root = fixture.0.clone();
    let git = move |args: &[&str]| {
        let output = Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
            ])
            .args(args)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(output.status.success());
        output.stdout
    };
    fs::write(fixture.0.join("tracked"), b"base\n").unwrap();
    git(&["add", "tracked"]);
    git(&["commit", "-qm", "base"]);
    (fixture, git)
}

/// Copy/download-like stat drift: unchanged content Git must re-read before trusting it.
fn drift_stat_data(path: &std::path::Path) {
    let earlier = std::time::SystemTime::now() - Duration::from_hours(1);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(earlier))
        .unwrap();
}

#[test]
fn status_cache_installs_a_refresh_once_and_observes_external_staging() {
    let (fixture, git) = committed_fixture();
    let index = fixture.0.join(".git/index");
    drift_stat_data(&fixture.0.join("tracked"));
    let original = fs::read(&index).unwrap();
    let staged_entries = git(&["ls-files", "-s"]);
    let repository = fixture.repository(false);
    repository.load().unwrap();
    let first = wait_for(&repository, |s| s.phase == RepositoryPhase::Ready);
    assert!(first.status.as_ref().unwrap().entries.is_empty());
    let refreshed = fs::read(&index).unwrap();
    assert_ne!(refreshed, original, "the refreshed stat data must be kept");
    assert_eq!(git(&["ls-files", "-s"]), staged_entries);
    assert!(!fixture.0.join(".git/index.lock").exists());
    fs::write(fixture.0.join("tracked"), b"edit\n").unwrap();
    repository.refresh().unwrap();
    let dirty = wait_for(&repository, |s| {
        s.revision > first.revision && s.phase == RepositoryPhase::Ready
    });
    assert_eq!(dirty.status.as_ref().unwrap().entries.len(), 1);
    assert_eq!(fs::read(&index).unwrap(), refreshed, "installed once");
    git(&["add", "tracked"]);
    let staged = fs::read(&index).unwrap();
    repository.refresh().unwrap();
    let next = wait_for(&repository, |s| {
        s.revision > dirty.revision && s.phase == RepositoryPhase::Ready
    });
    assert_eq!(next.status.as_ref().unwrap().entries.len(), 1);
    assert_ne!(
        next.status, dirty.status,
        "external staging must invalidate the private index"
    );
    assert_eq!(fs::read(&index).unwrap(), staged);
    repository.close();
}

#[test]
fn status_cache_never_installs_when_disabled() {
    let (fixture, _git) = committed_fixture();
    let index = fixture.0.join(".git/index");
    drift_stat_data(&fixture.0.join("tracked"));
    let original = fs::read(&index).unwrap();
    let repository = Repository::new(
        &fixture.0,
        RepositoryOptions {
            watch: false,
            index_refresh: super::IndexRefreshPolicy::PrivateOnly,
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    repository.load().unwrap();
    let first = wait_for(&repository, |s| s.phase == RepositoryPhase::Ready);
    assert!(first.status.as_ref().unwrap().entries.is_empty());
    repository.refresh().unwrap();
    let _ = wait_for(&repository, |s| {
        s.revision > first.revision && s.phase == RepositoryPhase::Ready
    });
    repository.close();
    assert_eq!(
        fs::read(&index).unwrap(),
        original,
        "the user's index is untouched"
    );
    assert!(!fixture.0.join(".git/index.lock").exists());
}

#[test]
fn status_cache_leaves_a_locked_index_untouched() {
    let (fixture, _git) = committed_fixture();
    let index = fixture.0.join(".git/index");
    let lock = fixture.0.join(".git/index.lock");
    drift_stat_data(&fixture.0.join("tracked"));
    let original = fs::read(&index).unwrap();
    fs::write(&lock, b"held by another Git process").unwrap();
    let repository = fixture.repository(false);
    repository.load().unwrap();
    let ready = wait_for(&repository, |s| s.phase == RepositoryPhase::Ready);
    assert!(ready.status.as_ref().unwrap().entries.is_empty());
    repository.close();
    assert_eq!(fs::read(&index).unwrap(), original);
    assert_eq!(fs::read(&lock).unwrap(), b"held by another Git process");
}

#[test]
fn initial_references_are_published_while_status_is_blocked() {
    use crate::activity::ActivityEvent;
    let (fixture, git) = committed_fixture();
    git(&["branch", "feature"]);
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let sink_gate = Arc::clone(&gate);
    let repository = Repository::new(
        &fixture.0,
        RepositoryOptions {
            watch: false,
            activity_sink: Some(Arc::new(move |event| {
                if let ActivityEvent::Created(record) = event {
                    if record
                        .command
                        .arguments
                        .iter()
                        .any(|arg| arg.display == "status")
                    {
                        let (lock, ready) = &*sink_gate;
                        let released = lock.lock().unwrap();
                        drop(
                            ready
                                .wait_timeout_while(released, Duration::from_secs(10), |value| {
                                    !*value
                                })
                                .unwrap(),
                        );
                    }
                }
            })),
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    repository.load().unwrap();
    let preview = wait_for(&repository, |s| !s.refs.is_empty());
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    assert_eq!(preview.phase, RepositoryPhase::Loading);
    assert!(preview.status.is_none());
    let ready = wait_for(&repository, |s| s.phase == RepositoryPhase::Ready);
    assert!(ready.revision > preview.revision);
    assert_eq!(ready.refs, preview.refs);
    assert!(ready.status.is_some());
    repository.close();
}

struct RecursiveWatcher {
    updatable: bool,
    registrations: Arc<AtomicUsize>,
    exclusions: Arc<Mutex<Vec<PathBuf>>>,
}
impl crate::watch::PlatformWatcherPort for RecursiveWatcher {
    fn covers_recursively(&self) -> bool {
        true
    }
    fn watch(
        &self,
        plan: &crate::watch::RepositoryWatchPlan,
    ) -> Result<Box<dyn crate::watch::RepositoryWatcherPort>, crate::watch::WatchError> {
        self.registrations.fetch_add(1, Ordering::AcqRel);
        plan.excluded_worktree_roots
            .clone_into(&mut self.exclusions.lock().unwrap());
        Ok(Box::new(Self {
            updatable: self.updatable,
            registrations: Arc::clone(&self.registrations),
            exclusions: Arc::clone(&self.exclusions),
        }))
    }
}
impl crate::watch::RepositoryWatcherPort for RecursiveWatcher {
    fn wait(&mut self, _: Duration) -> Result<crate::watch::WatchBatch, crate::watch::WatchError> {
        Ok(crate::watch::WatchBatch {
            changes: Vec::new(),
            overflow: false,
        })
    }
    fn update_exclusions(&mut self, excluded_worktree_roots: &[PathBuf]) -> bool {
        if self.updatable {
            excluded_worktree_roots.clone_into(&mut self.exclusions.lock().unwrap());
        }
        self.updatable
    }
}

/// Opens with a recursive watcher and returns (registrations, status runs, final exclusions).
fn open_with_recursive_watcher(updatable: bool) -> (usize, usize, Vec<PathBuf>) {
    use crate::activity::ActivityEvent;
    let (fixture, _git) = committed_fixture();
    fs::write(fixture.0.join(".gitignore"), b"build/\n").unwrap();
    fs::create_dir(fixture.0.join("build")).unwrap();
    fs::write(fixture.0.join("build/output"), b"generated").unwrap();
    let registrations = Arc::new(AtomicUsize::new(0));
    let exclusions = Arc::new(Mutex::new(Vec::new()));
    let statuses = Arc::new(AtomicUsize::new(0));
    let sink_statuses = Arc::clone(&statuses);
    let repository = Repository::new(
        &fixture.0,
        RepositoryOptions {
            watcher_factory: Some(Arc::new(RecursiveWatcher {
                updatable,
                registrations: Arc::clone(&registrations),
                exclusions: Arc::clone(&exclusions),
            })),
            activity_sink: Some(Arc::new(move |event| {
                if let ActivityEvent::Created(record) = event {
                    if record
                        .command
                        .arguments
                        .iter()
                        .any(|arg| arg.display == "status")
                    {
                        sink_statuses.fetch_add(1, Ordering::AcqRel);
                    }
                }
            })),
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    repository.load().unwrap();
    let ready = wait_for(&repository, |s| s.phase == RepositoryPhase::Ready);
    let deadline = Instant::now() + Duration::from_secs(10);
    while exclusions.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "exclusions were never applied");
        std::thread::sleep(Duration::from_millis(5));
    }
    if !updatable {
        wait_for(&repository, |s| {
            s.revision > ready.revision && s.phase == RepositoryPhase::Ready
        });
    }
    // Longer than the refresh debounce, so a scheduled rescan would have started.
    std::thread::sleep(Duration::from_millis(400));
    repository.close();
    let result = exclusions.lock().unwrap().clone();
    (
        registrations.load(Ordering::Acquire),
        statuses.load(Ordering::Acquire),
        result,
    )
}

#[test]
fn a_recursive_watch_before_the_first_scan_needs_no_gap_rescan() {
    let (registrations, statuses, exclusions) = open_with_recursive_watcher(true);
    assert_eq!(registrations, 1);
    assert_eq!(statuses, 1, "the early watch covers the first scan");
    assert!(exclusions.iter().any(|path| path.ends_with("build")));
}

#[test]
fn a_replaced_watch_rescans_the_registration_gap() {
    let (registrations, statuses, exclusions) = open_with_recursive_watcher(false);
    assert_eq!(registrations, 2);
    assert!(statuses >= 2, "replacement must rescan the gap");
    assert!(exclusions.iter().any(|path| path.ends_with("build")));
}

#[test]
fn a_requested_refresh_rediscovers_ignored_roots_without_replacing_the_watch() {
    let (fixture, _git) = committed_fixture();
    fs::write(fixture.0.join(".gitignore"), b"build/\n").unwrap();
    fs::create_dir(fixture.0.join("build")).unwrap();
    fs::write(fixture.0.join("build/output"), b"generated").unwrap();
    let registrations = Arc::new(AtomicUsize::new(0));
    let exclusions = Arc::new(Mutex::new(Vec::new()));
    let repository = Repository::new(
        &fixture.0,
        RepositoryOptions {
            watcher_factory: Some(Arc::new(RecursiveWatcher {
                updatable: true,
                registrations: Arc::clone(&registrations),
                exclusions: Arc::clone(&exclusions),
            })),
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    repository.load().unwrap();
    wait_for(&repository, |s| s.phase == RepositoryPhase::Ready);
    let excludes = |name: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !exclusions
            .lock()
            .unwrap()
            .iter()
            .any(|path| path.ends_with(name))
        {
            assert!(Instant::now() < deadline, "{name} was never excluded");
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    excludes("build");
    // A build writes a second ignored root after the watch started filtering.
    fs::write(fixture.0.join(".gitignore"), b"build/\nout/\n").unwrap();
    fs::create_dir(fixture.0.join("out")).unwrap();
    fs::write(fixture.0.join("out/artifact"), b"generated").unwrap();
    repository.refresh().unwrap();
    excludes("out");
    assert_eq!(registrations.load(Ordering::Acquire), 1);
    repository.close();
}

#[test]
fn commit_diff_never_runs_a_configured_textconv_filter() {
    let (fixture, git) = committed_fixture();
    fs::write(fixture.0.join(".gitattributes"), b"tracked diff=marker\n").unwrap();
    git(&[
        "config",
        "diff.marker.textconv",
        "sh -c 'touch textconv-ran; cat \"$0\"'",
    ]);
    git(&["add", ".gitattributes"]);
    git(&["commit", "-qm", "attributes"]);
    let parent = String::from_utf8(git(&["rev-parse", "HEAD"])).unwrap();
    fs::write(fixture.0.join("tracked"), b"changed\n").unwrap();
    git(&["commit", "-qam", "change"]);
    let head = String::from_utf8(git(&["rev-parse", "HEAD"])).unwrap();
    let repository = fixture.repository(false);
    repository.load().unwrap();
    wait_for(&repository, |s| s.phase == RepositoryPhase::Ready);
    let document = repository
        .commit_diff(
            crate::types::ObjectId::parse(head.trim()).unwrap(),
            crate::loading::commit::CommitDiffBase::FirstParent(
                crate::types::ObjectId::parse(parent.trim()).unwrap(),
            ),
            crate::loading::commit::CommitDiffOptions::default(),
        )
        .unwrap()
        .recv_timeout(Duration::from_secs(20))
        .expect("the diff finishes")
        .expect("the diff loads");
    repository.close();
    assert!(
        !fixture.0.join("textconv-ran").exists(),
        "history browsing must not execute repository-configured filters"
    );
    assert!(!document.files.is_empty());
}

/// Blocks the n-th `git status` until at least n scans are released.
struct StatusGate {
    /// Scans started and scans released.
    counts: Mutex<(usize, usize)>,
    changed: Condvar,
}
impl StatusGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            counts: Mutex::new((0, 0)),
            changed: Condvar::new(),
        })
    }
    fn sink(gate: &Arc<Self>) -> Arc<dyn Fn(crate::activity::ActivityEvent) + Send + Sync> {
        let gate = Arc::clone(gate);
        Arc::new(move |event: crate::activity::ActivityEvent| {
            if let crate::activity::ActivityEvent::Created(record) = event {
                if record
                    .command
                    .arguments
                    .iter()
                    .any(|arg| arg.display == "status")
                {
                    let mut counts = gate.counts.lock().unwrap();
                    counts.0 += 1;
                    let scan = counts.0;
                    gate.changed.notify_all();
                    // Bound the fixture even if an assertion in its owner fails.
                    drop(
                        gate.changed
                            .wait_timeout_while(counts, Duration::from_secs(10), |counts| {
                                counts.1 < scan
                            })
                            .unwrap(),
                    );
                }
            }
        })
    }
    fn wait_started(&self, scans: usize) {
        let counts = self.counts.lock().unwrap();
        let (counts, timeout) = self
            .changed
            .wait_timeout_while(counts, Duration::from_secs(5), |counts| counts.0 < scans)
            .unwrap();
        drop(counts);
        assert!(!timeout.timed_out(), "status scan {scans} did not start");
    }
    fn release(&self, scans: usize) {
        self.counts.lock().unwrap().1 = scans;
        self.changed.notify_all();
    }
}

fn modified_fixture() -> Fixture {
    let (fixture, git) = committed_fixture();
    fs::write(fixture.0.join("a.txt"), b"before\n").unwrap();
    git(&["add", "a.txt"]);
    git(&["commit", "-qm", "a"]);
    fs::write(fixture.0.join("a.txt"), b"after\n").unwrap();
    fixture
}

fn working_key(
    repository: &Repository,
    file: Option<&str>,
) -> crate::loading::working_diff::WorkingDiffKey {
    use crate::diff::model::{
        ContextLines, DiffFileId, DiffLayout, DiffPath, DiffWhitespacePolicy,
    };
    use crate::loading::working_diff::DiffEndpoint;
    crate::loading::working_diff::WorkingDiffKey {
        worktree: repository.snapshot().repository.clone().unwrap().worktree,
        base: DiffEndpoint::Index,
        target: DiffEndpoint::WorkingTree,
        file: file.map(|path| DiffFileId {
            old_path: Some(DiffPath::from_bytes(path).unwrap()),
            new_path: Some(DiffPath::from_bytes(path).unwrap()),
        }),
        layout: DiffLayout::Unified,
        context: ContextLines::new(3).unwrap(),
        whitespace: DiffWhitespacePolicy::Preserve,
        force_text: false,
    }
}

fn gated_repository(
    fixture: &Fixture,
    gate: &Arc<StatusGate>,
    policy: RefreshReadPolicy,
    watch: bool,
) -> Repository {
    Repository::new(
        &fixture.0,
        RepositoryOptions {
            watch,
            debounce: Duration::from_millis(20),
            refresh_read_policy: policy,
            activity_sink: Some(StatusGate::sink(gate)),
            ..RepositoryOptions::default()
        },
    )
    .unwrap()
}

#[test]
fn a_selected_file_diff_passes_a_blocked_status_scan_but_a_worktree_diff_waits() {
    let fixture = modified_fixture();
    let gate = StatusGate::new();
    let repository = gated_repository(&fixture, &gate, RefreshReadPolicy::ConcurrentScoped, false);
    repository.load().unwrap();
    gate.wait_started(1);
    let limits = crate::loading::working_diff::WorkingDiffLimits::default();
    let scoped = repository
        .working_diff(working_key(&repository, Some("a.txt")), limits)
        .unwrap();
    let worktree = repository
        .working_diff(working_key(&repository, None), limits)
        .unwrap();
    let scoped_result = scoped.recv_timeout(Duration::from_secs(5));
    let worktree_before_release = worktree.recv_timeout(Duration::from_millis(200));
    let phase = repository.snapshot().phase;
    gate.release(usize::MAX);
    let document = scoped_result
        .expect("a selected file's diff must not wait for status")
        .unwrap();
    assert_eq!(document.document.files.len(), 1);
    assert!(worktree_before_release.is_none());
    assert_eq!(phase, RepositoryPhase::Loading);
    assert!(worktree
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .is_ok());
    repository.close();
}

#[test]
fn concurrent_immutable_policy_keeps_a_selected_file_diff_behind_status() {
    let fixture = modified_fixture();
    let gate = StatusGate::new();
    let repository = gated_repository(
        &fixture,
        &gate,
        RefreshReadPolicy::ConcurrentImmutable,
        false,
    );
    repository.load().unwrap();
    gate.wait_started(1);
    let scoped = repository
        .working_diff(
            working_key(&repository, Some("a.txt")),
            crate::loading::working_diff::WorkingDiffLimits::default(),
        )
        .unwrap();
    let before_release = scoped.recv_timeout(Duration::from_millis(300));
    gate.release(usize::MAX);
    assert!(before_release.is_none());
    assert!(scoped.recv_timeout(Duration::from_secs(5)).unwrap().is_ok());
    repository.close();
}

/// A Git wrapper that holds the n-th `git status` process until `release-n` or
/// `release-all` exists in its gate directory. Unlike an activity-sink gate it blocks
/// only the scan, so the worker keeps ingesting watcher events meanwhile.
#[cfg(unix)]
struct ProcessGate(PathBuf);
#[cfg(unix)]
impl ProcessGate {
    fn new(fixture: &Fixture) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let gate = fixture.0.with_extension("gate");
        fs::create_dir_all(&gate).unwrap();
        let git = Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap();
        let git = String::from_utf8(git.stdout).unwrap().trim().to_owned();
        let script = gate.join("git");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ncase \" $* \" in\n*' status '*) n=$(ls '{dir}' | grep -c '^started-'); n=$((n+1)); : > '{dir}/started-'$n; while [ ! -f '{dir}/release-'$n ] && [ ! -f '{dir}/release-all' ]; do sleep 0.02; done;;\nesac\nexec '{git}' \"$@\"\n",
                dir = gate.display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        Self(gate)
    }
    fn executable(&self) -> std::ffi::OsString {
        self.0.join("git").into_os_string()
    }
    fn started(&self) -> usize {
        fs::read_dir(&self.0)
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("started-")
            })
            .count()
    }
    fn wait_started(&self, scans: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.started() < scans {
            assert!(
                Instant::now() < deadline,
                "status scan {scans} did not start"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn release(&self, scan: usize) {
        fs::write(self.0.join(format!("release-{scan}")), b"").unwrap();
    }
    fn release_all(&self) {
        fs::write(self.0.join("release-all"), b"").unwrap();
    }
}
#[cfg(unix)]
impl Drop for ProcessGate {
    fn drop(&mut self) {
        let _ = fs::write(self.0.join("release-all"), b"");
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
#[test]
fn a_read_queued_during_a_scan_is_answered_before_the_next_watcher_scan() {
    let fixture = modified_fixture();
    let gate = ProcessGate::new(&fixture);
    let repository = Repository::new(
        &fixture.0,
        RepositoryOptions {
            watch: true,
            debounce: Duration::from_millis(20),
            git_executable: gate.executable(),
            // Scans run on their own lane, so the worker keeps ingesting events meanwhile.
            refresh_read_policy: RefreshReadPolicy::ConcurrentImmutable,
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    // Opening scans, including an explicit backend's registration-gap rescan, run freely.
    gate.release_all();
    repository.load().unwrap();
    wait_for(&repository, |snapshot| {
        snapshot.phase == RepositoryPhase::Ready
    });
    std::thread::sleep(Duration::from_millis(500));
    fs::remove_file(gate.0.join("release-all")).unwrap();
    let opened = gate.started();
    fs::write(fixture.0.join("a.txt"), b"first watcher change\n").unwrap();
    gate.wait_started(opened + 1);
    let read = repository
        .schedule(b"waiting-read".to_vec(), 1, false, |_, _, _| Ok(7_u8))
        .unwrap();
    // The watcher makes another scan due while this one is still running.
    fs::write(fixture.0.join("a.txt"), b"second watcher change\n").unwrap();
    std::thread::sleep(Duration::from_millis(400));
    gate.release(opened + 1);
    let answered = read.recv_timeout(Duration::from_secs(3));
    // The fixture must really make a second scan due.
    gate.wait_started(opened + 2);
    gate.release_all();
    assert_eq!(
        *answered
            .expect("the next scan must not start ahead of a waiting read")
            .unwrap(),
        7
    );
    repository.close();
}
