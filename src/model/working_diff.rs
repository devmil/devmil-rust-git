//! Cached working diffs with a bounded incremental first-file slot per in-flight read.
use super::{lock, ReadTicket, Repository, RepositoryError, RepositoryErrorKind};
use crate::diff::model::{DiffDocument, DiffFile, DiffFileId};
use crate::loading::working_diff::{
    DiffEndpoint, StreamingCommandExecutor, WorkingDiffKey, WorkingDiffLimits, WorkingDiffLoader,
    WorkingDiffRequest, WorkingDiffValue,
};
use crate::loading::CommandExecutor;
use crate::process::{GitProcessCommand, PrivateActivityResult, PrivateStdoutObserver};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

pub(super) type Streams = BTreeMap<Vec<u8>, Weak<Stream>>;
pub(super) struct Stream {
    first: Mutex<Option<(DiffFile, Vec<u8>)>>,
    failure: Mutex<Option<crate::diff::DiffError>>,
    readers: AtomicUsize,
    finished: AtomicBool,
}
impl crate::cancellation::Cancellation for Stream {
    fn is_cancelled(&self) -> bool {
        self.readers.load(Ordering::Acquire) == 0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkingDiffResult {
    pub document: DiffDocument,
    pub raw_bytes: Vec<u8>,
    pub first_raw_bytes: Vec<u8>,
}

/// One stream subscriber. Dropping the last subscriber cancels an unfinished Git read.
/// The repository wakeup hook fires when the first-file slot becomes available; poll
/// `try_first_file` after wakeup and `recv_timeout` for completion. Each slot is delivered once.
pub struct WorkingDiffTicket {
    result: ReadTicket<WorkingDiffResult>,
    stream: Arc<Stream>,
    first_taken: bool,
}
impl WorkingDiffTicket {
    pub fn try_first_file(&mut self) -> Option<(DiffFile, Vec<u8>)> {
        if self.first_taken {
            return None;
        }
        let first = lock(&self.stream.first).clone();
        self.first_taken = first.is_some();
        first
    }
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Option<Result<Arc<WorkingDiffResult>, crate::diff::DiffError>> {
        self.result.recv_timeout(timeout).map(|result| {
            result.map_err(|error| {
                lock(&self.stream.failure).clone().unwrap_or_else(|| {
                    crate::diff::DiffError::new(
                        match error.kind {
                            RepositoryErrorKind::Canceled | RepositoryErrorKind::Closed => {
                                crate::diff::DiffErrorKind::Canceled
                            }
                            RepositoryErrorKind::TooLarge => crate::diff::DiffErrorKind::TooLarge,
                            _ => crate::diff::DiffErrorKind::GitFailed,
                        },
                        error.message,
                    )
                })
            })
        })
    }
}
impl Drop for WorkingDiffTicket {
    fn drop(&mut self) {
        self.stream.readers.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Executor<'a> {
    inner: &'a mut dyn CommandExecutor,
    cancellation: &'a Stream,
}
impl StreamingCommandExecutor for Executor<'_> {
    fn execute(
        &mut self,
        command: GitProcessCommand,
        observer: PrivateStdoutObserver<'_>,
    ) -> PrivateActivityResult {
        self.inner
            .execute_observing(command, observer, Some(self.cancellation))
    }
}

impl Repository {
    fn diff_stream(&self, cache_key: &[u8]) -> Arc<Stream> {
        let mut streams = lock(&self.shared.working_diffs);
        streams.retain(|_, stream| stream.strong_count() > 0);
        let stream = streams
            .get(cache_key)
            .and_then(Weak::upgrade)
            .filter(|stream| {
                !stream.finished.load(Ordering::Acquire)
                    && stream.readers.load(Ordering::Acquire) > 0
            })
            .unwrap_or_else(|| {
                let stream = Arc::new(Stream {
                    first: Mutex::new(None),
                    failure: Mutex::new(None),
                    readers: AtomicUsize::new(0),
                    finished: AtomicBool::new(false),
                });
                streams.insert(cache_key.to_vec(), Arc::downgrade(&stream));
                stream
            });
        stream.readers.fetch_add(1, Ordering::AcqRel);
        stream
    }

    /// Schedules a bounded working-tree/index read, sharing an incremental first-file slot.
    /// Cache hits return the complete result; consumers may derive a first-file presentation.
    /// # Errors
    /// Rejects a closed model, full queue, or a key referring to another worktree.
    pub fn working_diff(
        &self,
        key: WorkingDiffKey,
        limits: WorkingDiffLimits,
    ) -> Result<WorkingDiffTicket, RepositoryError> {
        let cache_key = cache_key(&key, limits);
        let stream = self.diff_stream(&cache_key);
        let job_stream = Arc::clone(&stream);
        let executable = self.shared.options.git_executable.clone();
        let wakeup = self.shared.options.wakeup.clone();
        // One file's diff asks Git directly and need not wait for a status scan. A diff of
        // the whole worktree stays ordered behind it.
        let scoped = key.file.is_some();
        let weight = |value: &WorkingDiffResult| {
            super::commit::diff_bytes(&value.document)
                .saturating_add(value.raw_bytes.capacity())
                .saturating_add(value.first_raw_bytes.capacity())
        };
        let job = move |repository: &crate::repository::DiscoveredRepository,
                        executor: &mut dyn crate::loading::CommandExecutor,
                        _: &dyn crate::cancellation::Cancellation| {
            struct Finished<'a>(&'a Stream);
            impl Drop for Finished<'_> {
                fn drop(&mut self) {
                    self.0.finished.store(true, Ordering::Release);
                }
            }
            let _finished = Finished(&job_stream);
            if repository.worktree != key.worktree {
                return Err(RepositoryError::new(
                    RepositoryErrorKind::Read,
                    "diff key belongs to another worktree",
                ));
            }
            let mut executor = Executor {
                inner: executor,
                cancellation: &job_stream,
            };
            let loader = WorkingDiffLoader::new(&executable, &mut executor, limits);
            let mut completed = None;
            loader.read_diff(
                &WorkingDiffRequest {
                    key: &key,
                    cancellation: job_stream.as_ref(),
                },
                &mut |value| match value {
                    Ok(WorkingDiffValue::FirstFile { file, raw_bytes }) => {
                        *lock(&job_stream.first) = Some((file, raw_bytes));
                        if let Some(wakeup) = &wakeup {
                            wakeup();
                        }
                    }
                    Ok(WorkingDiffValue::Complete {
                        document,
                        raw_bytes,
                    }) => {
                        completed = Some(Ok(WorkingDiffResult {
                            document,
                            raw_bytes,
                            first_raw_bytes: lock(&job_stream.first)
                                .as_ref()
                                .map_or_else(Vec::new, |(_, bytes)| bytes.clone()),
                        }));
                    }
                    Err(error) => {
                        *lock(&job_stream.failure) = Some(error.clone());
                        completed = Some(Err(RepositoryError::new(
                            match error.kind {
                                crate::diff::DiffErrorKind::Canceled => {
                                    RepositoryErrorKind::Canceled
                                }
                                crate::diff::DiffErrorKind::TooLarge => {
                                    RepositoryErrorKind::TooLarge
                                }
                                _ => RepositoryErrorKind::Read,
                            },
                            error.detail(),
                        )));
                    }
                },
            );
            job_stream.finished.store(true, Ordering::Release);
            completed.unwrap_or_else(|| {
                Err(RepositoryError::new(
                    RepositoryErrorKind::Read,
                    "diff read returned no result",
                ))
            })
        };
        let result = if scoped {
            self.schedule_scoped_weighted(cache_key, weight, job)
        } else {
            self.schedule_weighted(cache_key, false, weight, job)
        };
        match result {
            Ok(result) => Ok(WorkingDiffTicket {
                result,
                stream,
                first_taken: false,
            }),
            Err(error) => {
                stream.readers.fetch_sub(1, Ordering::AcqRel);
                Err(error)
            }
        }
    }
}

fn field(key: &mut Vec<u8>, value: &[u8]) {
    key.extend_from_slice(&value.len().to_le_bytes());
    key.extend_from_slice(value);
}
fn endpoint(key: &mut Vec<u8>, value: &DiffEndpoint) {
    match value {
        DiffEndpoint::EmptyTree => key.push(0),
        DiffEndpoint::Head => key.push(1),
        DiffEndpoint::Index => key.push(2),
        DiffEndpoint::WorkingTree => key.push(3),
        DiffEndpoint::Commit(object) => {
            key.push(4);
            field(key, object.as_str().as_bytes());
        }
    }
}
fn paths(key: &mut Vec<u8>, value: Option<&DiffFileId>) {
    key.push(u8::from(value.is_some()));
    if let Some(value) = value {
        for path in [&value.old_path, &value.new_path] {
            key.push(u8::from(path.is_some()));
            if let Some(path) = path {
                field(key, path.as_bytes());
            }
        }
    }
}
fn cache_key(value: &WorkingDiffKey, limits: WorkingDiffLimits) -> Vec<u8> {
    let mut key = b"working-diff-v1".to_vec();
    field(
        &mut key,
        value
            .worktree
            .worktree_root()
            .as_path()
            .as_os_str()
            .as_encoded_bytes(),
    );
    endpoint(&mut key, &value.base);
    endpoint(&mut key, &value.target);
    paths(&mut key, value.file.as_ref());
    field(
        &mut key,
        format!(
            "{:?}:{:?}:{:?}:{}:{limits:?}",
            value.layout, value.context, value.whitespace, value.force_text
        )
        .as_bytes(),
    );
    key
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::diff::model::{ContextLines, DiffLayout, DiffWhitespacePolicy};
    use crate::model::{RepositoryOptions, RepositoryPhase};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::Instant;

    static NEXT: AtomicUsize = AtomicUsize::new(1);
    struct Fixture {
        root: PathBuf,
        model: Repository,
        calls: Arc<AtomicUsize>,
        wakeups: Arc<AtomicUsize>,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "devmil-stream-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            let git = |args: &[&str]| {
                let result = Command::new("git")
                    .current_dir(&root)
                    .args(args)
                    .output()
                    .unwrap();
                assert!(
                    result.status.success(),
                    "{}",
                    String::from_utf8_lossy(&result.stderr)
                );
            };
            git(&["init", "-b", "main"]);
            fs::write(root.join("a.txt"), "before\n").unwrap();
            fs::write(root.join("b.txt"), "before\n").unwrap();
            git(&["add", "."]);
            git(&[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-m",
                "initial",
            ]);
            fs::write(root.join("a.txt"), "after\n").unwrap();
            fs::write(root.join("b.txt"), "after\n").unwrap();
            let executable = root.join(".git/gated-git");
            fs::write(&executable, "#!/bin/sh\ncase \" $* \" in\n*' --patch '*) /usr/bin/git \"$@\"; echo $$ > .git/diff-pid; while [ ! -f .git/release ]; do /bin/sleep 0.02; done;;\n*) exec /usr/bin/git \"$@\";;\nesac\n").unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&calls);
            let wakeups = Arc::new(AtomicUsize::new(0));
            let awakened = Arc::clone(&wakeups);
            let model = Repository::new(
                &root,
                RepositoryOptions {
                    watch: false,
                    git_executable: executable.into_os_string(),
                    activity_sink: Some(Arc::new(move |event| {
                        if let crate::activity::ActivityEvent::Created(record) = event {
                            if record
                                .command
                                .arguments
                                .iter()
                                .any(|arg| arg.display == "--patch")
                            {
                                counted.fetch_add(1, Ordering::AcqRel);
                            }
                        }
                    })),
                    wakeup: Some(Arc::new(move || {
                        awakened.fetch_add(1, Ordering::AcqRel);
                    })),
                    ..RepositoryOptions::default()
                },
            )
            .unwrap();
            model.load().unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while model.snapshot().phase != RepositoryPhase::Ready {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            Self {
                root,
                model,
                calls,
                wakeups,
            }
        }
        fn key(&self) -> WorkingDiffKey {
            WorkingDiffKey {
                worktree: self
                    .model
                    .snapshot()
                    .repository
                    .as_ref()
                    .unwrap()
                    .worktree
                    .clone(),
                base: DiffEndpoint::Index,
                target: DiffEndpoint::WorkingTree,
                file: None,
                layout: DiffLayout::Unified,
                context: ContextLines::new(3).unwrap(),
                whitespace: DiffWhitespacePolicy::Preserve,
                force_text: false,
            }
        }
        fn early(ticket: &mut WorkingDiffTicket) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while ticket.try_first_file().is_none() {
                assert!(Instant::now() < deadline, "no incremental file");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.model.close();
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn early_file_wakes_subscribers_and_completed_diff_is_cached() {
        let fixture = Fixture::new();
        let wakes = fixture.wakeups.load(Ordering::Acquire);
        let mut first = fixture
            .model
            .working_diff(fixture.key(), WorkingDiffLimits::default())
            .unwrap();
        let mut second = fixture
            .model
            .working_diff(fixture.key(), WorkingDiffLimits::default())
            .unwrap();
        Fixture::early(&mut first);
        Fixture::early(&mut second);
        assert!(fixture.wakeups.load(Ordering::Acquire) > wakes);
        assert!(
            first.recv_timeout(Duration::ZERO).is_none(),
            "first file must precede process exit"
        );
        assert_eq!(fixture.calls.load(Ordering::Acquire), 1);
        fs::write(fixture.root.join(".git/release"), "").unwrap();
        let result = first.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(result.document.files.len(), 2);
        assert!(second.recv_timeout(Duration::from_secs(5)).unwrap().is_ok());
        let cached = fixture
            .model
            .working_diff(fixture.key(), WorkingDiffLimits::default())
            .unwrap();
        assert_eq!(
            *cached
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap(),
            *result
        );
        assert_eq!(fixture.calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn dropping_last_stream_subscriber_cancels_process_and_unblocks_worker() {
        let fixture = Fixture::new();
        let mut first = fixture
            .model
            .working_diff(fixture.key(), WorkingDiffLimits::default())
            .unwrap();
        let second = fixture
            .model
            .working_diff(fixture.key(), WorkingDiffLimits::default())
            .unwrap();
        Fixture::early(&mut first);
        drop(first);
        assert!(second.recv_timeout(Duration::from_millis(30)).is_none());
        drop(second);
        let next = fixture
            .model
            .schedule(b"after-stream".to_vec(), 1, false, |_, _, _| Ok(7_u8))
            .unwrap();
        assert_eq!(
            *next.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(),
            7
        );
        let pid = fs::read_to_string(fixture.root.join(".git/diff-pid")).unwrap();
        assert!(
            !Command::new("kill")
                .args(["-0", pid.trim()])
                .output()
                .unwrap()
                .status
                .success(),
            "Git process must be reaped"
        );
    }
}
