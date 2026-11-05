//! Exclusive access for mutations performed outside the repository worker.
use super::{
    closed, lock, Arc, Duration, Ordering, Repository, RepositoryError, RepositoryErrorKind, Shared,
};
use crate::cancellation::Cancellation;
use std::marker::PhantomData;
use std::rc::Rc;
use std::thread::ThreadId;

#[derive(Default)]
pub(super) struct MutationState {
    owner: Option<ThreadId>,
    waiters: usize,
    worker_active: usize,
}

/// An exclusive, thread-bound mutation lease. Drop invalidates cached reads and requests a full
/// refresh before queued reads resume. Keep this lease for the entire external Git operation.
/// A lease does not prevent closing the model or cancel the caller's external process.
#[must_use = "keep the lease alive until the external mutation finishes"]
pub struct RepositoryMutation {
    shared: Arc<Shared>,
    _thread_bound: PhantomData<Rc<()>>,
}
impl Drop for RepositoryMutation {
    fn drop(&mut self) {
        let mut state = lock(&self.shared.state);
        state.mutation.owner = None;
        if !self.shared.cancel.load(Ordering::Acquire) {
            state.refresh_pending = true;
        }
        drop(state);
        self.shared.changed.notify_all();
    }
}

impl Repository {
    /// Waits for active reads and refreshes, then suspends worker activity until the lease drops.
    /// Concurrent callers serialize; acquisition order is unspecified. Nested acquisition on the
    /// same thread is rejected. Do not call this from a repository worker callback.
    /// # Errors
    /// Returns `Closed` after shutdown and `InvalidOptions` for nested acquisition.
    pub fn begin_mutation(&self) -> Result<RepositoryMutation, RepositoryError> {
        self.acquire_mutation(None)
    }

    /// Like [`Self::begin_mutation`], with caller cancellation while waiting.
    /// # Errors
    /// Also returns `Canceled` when the supplied cancellation becomes set before acquisition.
    pub fn begin_mutation_with_cancellation(
        &self,
        cancellation: &dyn Cancellation,
    ) -> Result<RepositoryMutation, RepositoryError> {
        self.acquire_mutation(Some(cancellation))
    }

    fn acquire_mutation(
        &self,
        cancellation: Option<&dyn Cancellation>,
    ) -> Result<RepositoryMutation, RepositoryError> {
        self.load()?;
        let owner = std::thread::current().id();
        let mut state = lock(&self.shared.state);
        if state.mutation.owner == Some(owner) {
            return Err(RepositoryError::new(
                RepositoryErrorKind::InvalidOptions,
                "repository mutation leases cannot be nested",
            ));
        }
        state.mutation.waiters += 1;
        loop {
            let failure = if self.shared.cancel.load(Ordering::Acquire) {
                Some(closed())
            } else if cancellation.is_some_and(Cancellation::is_cancelled) {
                Some(RepositoryError::new(
                    RepositoryErrorKind::Canceled,
                    "mutation lease acquisition canceled",
                ))
            } else {
                None
            };
            if let Some(error) = failure {
                state.mutation.waiters -= 1;
                drop(state);
                self.shared.changed.notify_all();
                return Err(error);
            }
            if state.mutation.owner.is_none() && state.mutation.worker_active == 0 {
                state.mutation.waiters -= 1;
                state.mutation.owner = Some(owner);
                return Ok(RepositoryMutation {
                    shared: Arc::clone(&self.shared),
                    _thread_bound: PhantomData,
                });
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, Duration::from_millis(25))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

pub(super) struct WorkerActivity(Arc<Shared>);
impl Drop for WorkerActivity {
    fn drop(&mut self) {
        lock(&self.0.state).mutation.worker_active -= 1;
        self.0.changed.notify_all();
    }
}
pub(super) fn enter_worker(shared: &Arc<Shared>) -> Option<WorkerActivity> {
    let mut state = lock(&shared.state);
    while state.mutation.owner.is_some() || state.mutation.waiters != 0 {
        if shared.cancel.load(Ordering::Acquire) {
            return None;
        }
        state = shared
            .changed
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    if shared.cancel.load(Ordering::Acquire) {
        return None;
    }
    state.mutation.worker_active += 1;
    Some(WorkerActivity(Arc::clone(shared)))
}

/// Fork a read lease while the worker already holds one. Mutations wait for both lanes.
pub(super) fn fork_worker(shared: &Arc<Shared>) -> WorkerActivity {
    lock(&shared.state).mutation.worker_active += 1;
    WorkerActivity(Arc::clone(shared))
}

impl RepositoryMutation {
    /// Reads repository status now, on the calling thread, while this lease keeps the worker
    /// suspended. Cached snapshots and the session-private index are bypassed, and Git runs
    /// with `GIT_OPTIONAL_LOCKS=0`, so the read never rewrites the user's index under any
    /// [`super::IndexRefreshPolicy`]. Commands use the repository's executable, activity sink
    /// and identifiers. Returns `Ok(None)` for a bare repository.
    ///
    /// # Errors
    /// Returns `Closed` after shutdown, `Discovery` before the repository was discovered, and
    /// the mapped status failure otherwise.
    pub fn fresh_status(
        &self,
        cancellation: Option<&dyn Cancellation>,
    ) -> Result<Option<crate::status::RepositoryStatus>, RepositoryError> {
        use crate::loading::status::{load, StatusFailure, StatusOptions};
        if self.shared.cancel.load(Ordering::Acquire) {
            return Err(closed());
        }
        let repository = lock(&self.shared.state)
            .snapshot
            .repository
            .clone()
            .ok_or_else(|| {
                RepositoryError::new(
                    RepositoryErrorKind::Discovery,
                    "the repository has not been discovered",
                )
            })?;
        let options = StatusOptions {
            bare: repository.kind == crate::repository::RepositoryKind::Bare,
            ..StatusOptions::default()
        };
        let mut executor =
            |command| super::worker::execute_with_cancellation(&self.shared, command, cancellation);
        load(
            &self.shared.options.git_executable,
            &repository.worktree,
            Some(repository.git_version),
            options,
            &mut executor,
        )
        .map_err(|failure| {
            let (kind, message) = match failure {
                StatusFailure::MissingGit => (RepositoryErrorKind::MissingGit, "Git is missing"),
                StatusFailure::UnsupportedGit => (
                    RepositoryErrorKind::Unsupported,
                    "Git is too old for status",
                ),
                StatusFailure::Canceled => (RepositoryErrorKind::Canceled, "status canceled"),
                StatusFailure::InvalidOutput => (
                    RepositoryErrorKind::InvalidOutput,
                    "status output is malformed",
                ),
                StatusFailure::GitFailed => (RepositoryErrorKind::Read, "status failed"),
            };
            RepositoryError::new(kind, message)
        })
    }
}

impl Repository {
    /// Waits until the worker has been idle for `quiet`: no load, refresh, watcher
    /// registration, debounced watch event or queued read pending. Use a `quiet` period longer
    /// than the watch debounce after a write so its own file events are absorbed. Returns the
    /// snapshot current at that moment.
    ///
    /// # Errors
    /// Returns `TimedOut` when the worker is not quiet within `timeout`, `Closed` after
    /// shutdown, and `InvalidOptions` when the calling thread holds this repository's mutation
    /// lease (the worker cannot become idle while it waits for that lease).
    pub fn wait_until_idle(
        &self,
        quiet: Duration,
        timeout: Duration,
    ) -> Result<Arc<super::RepositorySnapshot>, RepositoryError> {
        let started = std::time::Instant::now();
        let mut idle_since: Option<std::time::Instant> = None;
        let mut state = lock(&self.shared.state);
        if state.mutation.owner == Some(std::thread::current().id()) {
            return Err(RepositoryError::new(
                RepositoryErrorKind::InvalidOptions,
                "waiting for idle while holding the mutation lease would never finish",
            ));
        }
        loop {
            if self.shared.cancel.load(Ordering::Acquire) {
                return Err(closed());
            }
            let now = std::time::Instant::now();
            let idle = !state.load_pending
                && !state.refresh_pending
                && !state.worker_busy
                && state.mutation.owner.is_none()
                && state.mutation.worker_active == 0
                && !super::reads::has_pending(&state);
            if idle {
                let since = *idle_since.get_or_insert(now);
                if now.duration_since(since) >= quiet {
                    return Ok(Arc::clone(&state.snapshot));
                }
            } else {
                idle_since = None;
            }
            let elapsed = now.duration_since(started);
            if elapsed >= timeout {
                return Err(RepositoryError::new(
                    RepositoryErrorKind::TimedOut,
                    "the repository did not become idle in time",
                ));
            }
            let step = timeout
                .saturating_sub(elapsed)
                .min(Duration::from_millis(10));
            state = self
                .shared
                .changed
                .wait_timeout(state, step)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}
