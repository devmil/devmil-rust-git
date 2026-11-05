//! Long-lived repository model. Construction is cheap; `load` starts background work.

pub mod autosquash;
pub mod comparison;
pub mod extensions;
mod mutation;
mod reads;
pub mod statistics;
pub use mutation::RepositoryMutation;
pub mod commit;
mod commit_tools;
mod conflict_prediction;
mod content;
mod deep_graph;
mod graph;
mod graph_pages;
pub mod pages;
mod status_cache;
#[cfg(test)]
mod tests;
mod worker;
pub mod working_diff;
pub use reads::ReadTicket;

use crate::activity::{ActivityEvent, ActivityId, ActivityRecord};
use crate::loading::refs::RefsSectionData;
use crate::repository::DiscoveredRepository;
use crate::status::RepositoryStatus;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

static NEXT_ACTIVITY: AtomicU64 = AtomicU64::new(1);

/// Ordering of reads relative to status/ref snapshot publication.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RefreshReadPolicy {
    /// Every read waits for a running status/ref scan to publish.
    #[default]
    AfterSnapshot,
    /// Reads of immutable objects (history, commits) run while a scan is in progress.
    ConcurrentImmutable,
    /// As [`Self::ConcurrentImmutable`], and path-scoped working-tree reads (one file's
    /// working diff) also run during a scan. They ask Git directly instead of using the
    /// snapshot, so a slow status in a large worktree does not hold them back. A finished
    /// scan still discards their cached results.
    ConcurrentScoped,
}

/// What a status scan may do with the stat-cache refresh in its session-private index.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum IndexRefreshPolicy {
    /// Install a refreshed private copy over the user's index once, under Git's lock protocol.
    #[default]
    InstallOnce,
    /// Keep refreshes in the private copy. Read-only consumers use this so a status scan never
    /// replaces a repository file.
    PrivateOnly,
}

/// Optional application hooks. Callbacks run on the repository worker and must return promptly.
#[derive(Clone)]
pub struct RepositoryOptions {
    pub git_executable: OsString,
    pub discover_parents: bool,
    pub watch: bool,
    pub polling_fallback: bool,
    /// Serve immutable reads before status/ref refresh finishes. Consumers must accept
    /// a read result before the first Ready snapshot and later cache invalidation.
    pub refresh_read_policy: RefreshReadPolicy,
    pub watcher_factory: Option<Arc<dyn crate::watch::PlatformWatcherPort>>,
    pub debounce: Duration,
    pub event_capacity: usize,
    pub pending_read_capacity: usize,
    pub cache_entries: usize,
    pub cache_bytes: usize,
    /// Whether a status scan may write its refreshed stat cache back to the user's index.
    pub index_refresh: IndexRefreshPolicy,
    pub activity_sink: Option<Arc<dyn Fn(ActivityEvent) + Send + Sync>>,
    pub activity_id: Option<Arc<dyn Fn() -> ActivityId + Send + Sync>>,
    pub wakeup: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Temporary files the application writes inside the repository; changes to them never
    /// trigger a refresh. The default keeps the historical `.qspline-` rules.
    pub temporary_paths: crate::watch::TemporaryPathRules,
}

impl Default for RepositoryOptions {
    fn default() -> Self {
        Self {
            git_executable: "git".into(),
            discover_parents: true,
            watch: true,
            polling_fallback: true,
            refresh_read_policy: RefreshReadPolicy::AfterSnapshot,
            watcher_factory: None,
            debounce: Duration::from_millis(100),
            event_capacity: 128,
            pending_read_capacity: 128,
            cache_entries: 256,
            cache_bytes: 64 * 1024 * 1024,
            index_refresh: IndexRefreshPolicy::InstallOnce,
            activity_sink: None,
            activity_id: None,
            wakeup: None,
            temporary_paths: crate::watch::TemporaryPathRules::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryPhase {
    Unloaded,
    Loading,
    Ready,
    Failed,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryError {
    pub kind: RepositoryErrorKind,
    pub message: String,
    pub activity: Option<crate::activity::ActivityId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryErrorKind {
    Canceled,
    TimedOut,
    MissingObject,
    TooLarge,
    InvalidOutput,
    MissingGit,
    Unsupported,
    InvalidOptions,
    Discovery,
    Read,
    Watch,
    Closed,
    Capacity,
}

impl std::fmt::Display for RepositoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for RepositoryError {}

impl RepositoryError {
    fn new(kind: RepositoryErrorKind, message: impl Into<String>) -> Self {
        let mut message = message.into();
        while message.len() > 512 {
            message.pop();
        }
        Self {
            kind,
            message,
            activity: None,
        }
    }
    #[must_use]
    pub fn with_activity(mut self, activity: Option<crate::activity::ActivityId>) -> Self {
        self.activity = activity;
        self
    }
}

/// An immutable, coherent publication. Failed refreshes retain the last successful data.
#[derive(Clone, Debug)]
pub struct RepositorySnapshot {
    pub revision: u64,
    pub phase: RepositoryPhase,
    pub repository: Option<DiscoveredRepository>,
    pub status: Option<Arc<RepositoryStatus>>,
    pub status_failure: Option<crate::loading::status::StatusFailure>,
    pub refs_failure: Option<crate::loading::refs::RefsFailureKind>,
    pub refs: Arc<Vec<RefsSectionData>>,
    pub status_activities: Arc<Vec<ActivityRecord>>,
    pub refs_activities: Arc<Vec<ActivityRecord>>,
    pub error: Option<RepositoryError>,
    pub watch_error: Option<RepositoryError>,
}

impl Default for RepositorySnapshot {
    fn default() -> Self {
        Self {
            revision: 0,
            phase: RepositoryPhase::Unloaded,
            repository: None,
            status: None,
            status_failure: None,
            refs_failure: None,
            refs: Arc::new(Vec::new()),
            status_activities: Arc::new(Vec::new()),
            refs_activities: Arc::new(Vec::new()),
            error: None,
            watch_error: None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum RepositoryEvent {
    /// The watcher lost events. Emitted before the model rescans; `revision` identifies the
    /// snapshot preceding recovery. Consumers can acknowledge the warning before showing recovery.
    WatchOverflow {
        revision: u64,
    },
    ReadCompleted {
        id: u64,
        cache_hit: bool,
    },
    Snapshot(Arc<RepositorySnapshot>),
    /// Events were overwritten while this subscriber was behind. This snapshot is current.
    Resynchronize(Arc<RepositorySnapshot>),
}

struct State {
    mutation: mutation::MutationState,
    reads: reads::ReadQueue,
    snapshot: Arc<RepositorySnapshot>,
    events: VecDeque<(u64, RepositoryEvent)>,
    next_event: u64,
    load_pending: bool,
    refresh_pending: bool,
    /// The worker has refresh, watcher or exclusion work in flight or scheduled.
    worker_busy: bool,
}

struct Shared {
    working_diffs: Mutex<working_diff::Streams>,
    history_indices: Mutex<graph_pages::HistorySessions>,
    state: Mutex<State>,
    changed: Condvar,
    cancel: AtomicBool,
    options: RepositoryOptions,
}

/// Owns repository workers and caches. Drop cancels outstanding work and joins the worker.
pub struct Repository {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
    path: PathBuf,
}

impl std::fmt::Debug for Repository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Repository")
            .field("phase", &self.snapshot().phase)
            .finish_non_exhaustive()
    }
}

impl Repository {
    pub fn git_executable(&self) -> &std::ffi::OsStr {
        &self.shared.options.git_executable
    }
    /// Reuses an identity already resolved by the shared discovery API.
    /// # Errors
    /// Rejects invalid repository options.
    pub fn from_discovered(
        repository: DiscoveredRepository,
        options: RepositoryOptions,
    ) -> Result<Self, RepositoryError> {
        let model = Self::new(repository.worktree.worktree_root().as_path(), options)?;
        let mut snapshot = (*lock(&model.shared.state).snapshot).clone();
        snapshot.repository = Some(repository);
        lock(&model.shared.state).snapshot = Arc::new(snapshot);
        Ok(model)
    }

    /// Refreshes the repository and resolves after the refreshed snapshot has been published.
    /// # Errors
    /// Returns queue or lifecycle errors. Refresh errors are contained in the returned snapshot.
    pub fn reload(&self) -> Result<ReadTicket<RepositorySnapshot>, RepositoryError> {
        self.refresh()?;
        let shared = Arc::clone(&self.shared);
        self.schedule(
            reads::RELOAD_KEY.to_vec(),
            usize::MAX,
            false,
            move |_, _, _| Ok((*lock(&shared.state).snapshot).clone()),
        )
    }
    /// Constructs an unloaded repository without touching Git or the filesystem.
    /// # Errors
    /// Rejects empty paths, zero event capacity and excessive debounce periods.
    pub fn new(
        path: impl Into<PathBuf>,
        options: RepositoryOptions,
    ) -> Result<Self, RepositoryError> {
        let path = path.into();
        if path.as_os_str().is_empty()
            || options.event_capacity == 0
            || options.pending_read_capacity == 0
            || options.cache_entries == 0
            || options.cache_bytes == 0
            || options.debounce > Duration::from_mins(1)
        {
            return Err(RepositoryError::new(
                RepositoryErrorKind::InvalidOptions,
                "invalid repository options",
            ));
        }
        Ok(Self {
            path,
            worker: Mutex::new(None),
            shared: Arc::new(Shared {
                working_diffs: Mutex::new(working_diff::Streams::default()),
                history_indices: Mutex::new(graph_pages::HistorySessions::default()),
                state: Mutex::new(State {
                    mutation: mutation::MutationState::default(),
                    reads: reads::ReadQueue::default(),
                    snapshot: Arc::new(RepositorySnapshot::default()),
                    events: VecDeque::new(),
                    next_event: 1,
                    load_pending: false,
                    refresh_pending: false,
                    worker_busy: false,
                }),
                changed: Condvar::new(),
                cancel: AtomicBool::new(false),
                options,
            }),
        })
    }

    /// Starts loading on the owned worker. Repeated calls coalesce into one pending load.
    /// # Errors
    /// Returns an error after close or if the worker cannot be created.
    pub fn load(&self) -> Result<(), RepositoryError> {
        if self.shared.cancel.load(Ordering::Acquire) {
            return Err(closed());
        }
        let mut worker = lock(&self.worker);
        if self.shared.cancel.load(Ordering::Acquire) {
            return Err(closed());
        }
        if worker.is_none() {
            let shared = Arc::clone(&self.shared);
            let path = self.path.clone();
            *worker = Some(
                std::thread::Builder::new()
                    .name("git-repository".into())
                    .spawn(move || worker::run(&path, &shared))
                    .map_err(|_| {
                        RepositoryError::new(
                            RepositoryErrorKind::Capacity,
                            "repository worker could not start",
                        )
                    })?,
            );
        }
        lock(&self.shared.state).load_pending = true;
        self.shared.changed.notify_all();
        Ok(())
    }

    /// Requests a refresh; duplicate requests are coalesced internally.
    /// # Errors
    /// Returns an error after close.
    pub fn refresh(&self) -> Result<(), RepositoryError> {
        if self.shared.cancel.load(Ordering::Acquire) {
            return Err(closed());
        }
        lock(&self.shared.state).refresh_pending = true;
        self.shared.changed.notify_all();
        Ok(())
    }

    pub fn snapshot(&self) -> Arc<RepositorySnapshot> {
        Arc::clone(&lock(&self.shared.state).snapshot)
    }

    /// Subscribes from the current snapshot without competing with other subscribers.
    pub fn subscribe(&self) -> RepositorySubscription {
        let state = lock(&self.shared.state);
        RepositorySubscription {
            shared: Arc::clone(&self.shared),
            cursor: state.next_event,
            initial: Some(Arc::clone(&state.snapshot)),
        }
    }

    /// Cancels work and waits for the repository worker and watcher to stop.
    pub fn close(&self) {
        // Set under the state lock: a worker parked by a mutation lease checks the flag and
        // then waits without a timeout, and would miss a wakeup sent between the two.
        {
            let _state = lock(&self.shared.state);
            self.shared.cancel.store(true, Ordering::Release);
        }
        self.shared.changed.notify_all();
        if let Some(worker) = lock(&self.worker).take() {
            let _ = worker.join();
        }
        graph_pages::invalidate_indices(&self.shared);
        let mut snapshot = (*lock(&self.shared.state).snapshot).clone();
        if snapshot.phase != RepositoryPhase::Closed {
            snapshot.phase = RepositoryPhase::Closed;
            publish(&self.shared, snapshot);
        }
    }
}

impl Drop for Repository {
    fn drop(&mut self) {
        self.close();
    }
}

pub struct RepositorySubscription {
    shared: Arc<Shared>,
    cursor: u64,
    initial: Option<Arc<RepositorySnapshot>>,
}

impl RepositorySubscription {
    pub fn try_recv(&mut self) -> Option<RepositoryEvent> {
        if let Some(snapshot) = self.initial.take() {
            return Some(RepositoryEvent::Snapshot(snapshot));
        }
        let state = lock(&self.shared.state);
        if state
            .events
            .front()
            .is_some_and(|(sequence, _)| self.cursor < *sequence)
        {
            self.cursor = state.next_event;
            return Some(RepositoryEvent::Resynchronize(Arc::clone(&state.snapshot)));
        }
        let (sequence, event) = state
            .events
            .iter()
            .find(|(sequence, _)| *sequence >= self.cursor)?;
        self.cursor = sequence.saturating_add(1);
        Some(event.clone())
    }

    pub fn recv_timeout(&mut self, timeout: Duration) -> Option<RepositoryEvent> {
        if let Some(event) = self.try_recv() {
            return Some(event);
        }
        let shared = Arc::clone(&self.shared);
        let state = lock(&shared.state);
        let guard = shared
            .changed
            .wait_timeout_while(state, timeout, |state| {
                state.next_event <= self.cursor && !shared.cancel.load(Ordering::Acquire)
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(guard);
        self.try_recv()
    }
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn closed() -> RepositoryError {
    RepositoryError::new(RepositoryErrorKind::Closed, "repository is closed")
}

fn publish(shared: &Shared, mut snapshot: RepositorySnapshot) {
    let mut state = lock(&shared.state);
    snapshot.revision = state.snapshot.revision.saturating_add(1);
    let snapshot = Arc::new(snapshot);
    state.snapshot = Arc::clone(&snapshot);
    drop(state);
    emit(shared, RepositoryEvent::Snapshot(snapshot));
}

fn emit(shared: &Shared, event: RepositoryEvent) {
    let mut state = lock(&shared.state);
    let sequence = state.next_event;
    state.next_event = state.next_event.saturating_add(1);
    while state.events.len() >= shared.options.event_capacity {
        state.events.pop_front();
    }
    state.events.push_back((sequence, event));
    drop(state);
    shared.changed.notify_all();
    if let Some(wakeup) = &shared.options.wakeup {
        wakeup();
    }
}
