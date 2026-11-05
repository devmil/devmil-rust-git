use super::{
    closed, emit, lock, Arc, DiscoveredRepository, Duration, Ordering, Repository, RepositoryError,
    RepositoryErrorKind, RepositoryEvent, Shared, State, VecDeque,
};
use crate::cancellation::Cancellation;
use crate::loading::CommandExecutor;
use std::any::Any;
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, SyncSender};

type Value = Arc<dyn Any + Send + Sync>;
type Reply = Result<Value, RepositoryError>;
type Subscriber = (u64, SyncSender<Reply>, Arc<AtomicBool>);
type Job = Box<
    dyn FnOnce(
            &DiscoveredRepository,
            &mut dyn CommandExecutor,
            &dyn Cancellation,
        ) -> Result<(Value, usize), RepositoryError>
        + Send,
>;

/// Queue key of [`Repository::reload`]'s ticket.
pub(super) const RELOAD_KEY: &[u8] = b"snapshot-reload";

pub(super) struct PendingRead {
    key: Vec<u8>,
    immutable: bool,
    /// A path-scoped working-tree read that does not depend on the status snapshot.
    scoped: bool,
    /// Whether a result enters the read cache; concurrent subscribers share it either way.
    cacheable: bool,
    job: Job,
}
/// How a scheduled read relates to status scans.
#[derive(Clone, Copy)]
struct Admission {
    immutable: bool,
    scoped: bool,
}
/// Which queued reads the worker may start in its current state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReadAdmission {
    All,
    /// A scan is running; only immutable reads may start.
    Immutable,
    /// A scan is running; immutable and path-scoped working-tree reads may start.
    Scoped,
}
impl ReadAdmission {
    fn admits(self, read: &PendingRead) -> bool {
        match self {
            Self::All => true,
            Self::Immutable => read.immutable,
            Self::Scoped => read.immutable || read.scoped,
        }
    }
}
#[derive(Clone, Copy)]
enum ReadPriority {
    Interactive,
    Background,
}
pub(super) struct ReadQueue {
    interactive: VecDeque<PendingRead>,
    background: VecDeque<PendingRead>,
    subscribers: BTreeMap<Vec<u8>, Vec<Subscriber>>,
    next_ticket: u64,
}
impl Default for ReadQueue {
    fn default() -> Self {
        Self {
            interactive: VecDeque::new(),
            background: VecDeque::new(),
            subscribers: BTreeMap::new(),
            next_ticket: 1,
        }
    }
}

/// One typed asynchronous repository read. Dropping the last subscriber cancels shared work.
pub struct ReadTicket<T> {
    pub id: u64,
    receiver: Receiver<Reply>,
    marker: PhantomData<T>,
    canceled: Arc<AtomicBool>,
}
impl<T> Drop for ReadTicket<T> {
    fn drop(&mut self) {
        self.canceled.store(true, Ordering::Release);
    }
}
impl<T: Any + Send + Sync> ReadTicket<T> {
    pub fn try_recv(&self) -> Option<Result<Arc<T>, RepositoryError>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(typed(result)),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(closed())),
        }
    }
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Result<Arc<T>, RepositoryError>> {
        match self.receiver.recv_timeout(timeout) {
            Ok(result) => Some(typed(result)),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => Some(Err(closed())),
        }
    }
}
fn typed<T: Any + Send + Sync>(result: Reply) -> Result<Arc<T>, RepositoryError> {
    result?.downcast().map_err(|_| {
        RepositoryError::new(RepositoryErrorKind::Read, "repository cache type mismatch")
    })
}

impl Repository {
    pub(crate) fn schedule<T: Any + Send + Sync>(
        &self,
        key: Vec<u8>,
        charge: usize,
        immutable: bool,
        job: impl FnOnce(
                &DiscoveredRepository,
                &mut dyn CommandExecutor,
                &dyn Cancellation,
            ) -> Result<T, RepositoryError>
            + Send
            + 'static,
    ) -> Result<ReadTicket<T>, RepositoryError> {
        self.schedule_weighted(key, immutable, move |_| charge, job)
    }

    pub(crate) fn schedule_weighted<T: Any + Send + Sync>(
        &self,
        key: Vec<u8>,
        immutable: bool,
        weight: impl FnOnce(&T) -> usize + Send + 'static,
        job: impl FnOnce(
                &DiscoveredRepository,
                &mut dyn CommandExecutor,
                &dyn Cancellation,
            ) -> Result<T, RepositoryError>
            + Send
            + 'static,
    ) -> Result<ReadTicket<T>, RepositoryError> {
        self.schedule_weighted_at(
            ReadPriority::Interactive,
            key,
            Admission {
                immutable,
                scoped: false,
            },
            true,
            weight,
            job,
        )
    }

    /// An interactive read limited to explicit worktree paths. Under
    /// [`super::RefreshReadPolicy::ConcurrentScoped`] it runs while a status scan is in
    /// progress; its cached result is discarded when that scan finishes.
    pub(crate) fn schedule_scoped_weighted<T: Any + Send + Sync>(
        &self,
        key: Vec<u8>,
        weight: impl FnOnce(&T) -> usize + Send + 'static,
        job: impl FnOnce(
                &DiscoveredRepository,
                &mut dyn CommandExecutor,
                &dyn Cancellation,
            ) -> Result<T, RepositoryError>
            + Send
            + 'static,
    ) -> Result<ReadTicket<T>, RepositoryError> {
        self.schedule_weighted_at(
            ReadPriority::Interactive,
            key,
            Admission {
                immutable: false,
                scoped: true,
            },
            true,
            weight,
            job,
        )
    }

    pub(crate) fn schedule_background_weighted<T: Any + Send + Sync>(
        &self,
        key: Vec<u8>,
        immutable: bool,
        weight: impl FnOnce(&T) -> usize + Send + 'static,
        job: impl FnOnce(
                &DiscoveredRepository,
                &mut dyn CommandExecutor,
                &dyn Cancellation,
            ) -> Result<T, RepositoryError>
            + Send
            + 'static,
    ) -> Result<ReadTicket<T>, RepositoryError> {
        self.schedule_weighted_at(
            ReadPriority::Background,
            key,
            Admission {
                immutable,
                scoped: false,
            },
            true,
            weight,
            job,
        )
    }

    /// A background read whose result is never cached: each request after the
    /// previous one completed reads current state again.
    pub(crate) fn schedule_background_uncached<T: Any + Send + Sync>(
        &self,
        key: Vec<u8>,
        job: impl FnOnce(
                &DiscoveredRepository,
                &mut dyn CommandExecutor,
                &dyn Cancellation,
            ) -> Result<T, RepositoryError>
            + Send
            + 'static,
    ) -> Result<ReadTicket<T>, RepositoryError> {
        self.schedule_weighted_at(
            ReadPriority::Background,
            key,
            Admission {
                immutable: false,
                scoped: false,
            },
            false,
            |_| 0,
            job,
        )
    }

    fn schedule_weighted_at<T: Any + Send + Sync>(
        &self,
        priority: ReadPriority,
        key: Vec<u8>,
        admission: Admission,
        cacheable: bool,
        weight: impl FnOnce(&T) -> usize + Send + 'static,
        job: impl FnOnce(
                &DiscoveredRepository,
                &mut dyn CommandExecutor,
                &dyn Cancellation,
            ) -> Result<T, RepositoryError>
            + Send
            + 'static,
    ) -> Result<ReadTicket<T>, RepositoryError> {
        self.load()?;
        let mut state = lock(&self.shared.state);
        if self.shared.cancel.load(Ordering::Acquire) {
            return Err(closed());
        }
        if state.reads.subscribers.len() >= self.shared.options.pending_read_capacity {
            return Err(RepositoryError::new(
                RepositoryErrorKind::Capacity,
                "repository read queue is full",
            ));
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        let id = state.reads.next_ticket;
        state.reads.next_ticket = id.checked_add(1).ok_or_else(|| {
            RepositoryError::new(
                RepositoryErrorKind::Capacity,
                "repository request IDs exhausted",
            )
        })?;
        let subscribers = state.reads.subscribers.entry(key.clone()).or_default();
        if subscribers.len() >= self.shared.options.pending_read_capacity {
            return Err(RepositoryError::new(
                RepositoryErrorKind::Capacity,
                "too many duplicate repository reads",
            ));
        }
        let first = subscribers.is_empty();
        let canceled = Arc::new(AtomicBool::new(false));
        subscribers.push((id, sender, Arc::clone(&canceled)));
        if first {
            let pending = PendingRead {
                key,
                immutable: admission.immutable,
                scoped: admission.scoped,
                cacheable,
                job: Box::new(move |repository, executor, cancellation| {
                    job(repository, executor, cancellation).map(|value| {
                        let charge = weight(&value).saturating_add(std::mem::size_of::<Entry>());
                        (Arc::new(value) as Value, charge)
                    })
                }),
            };
            match priority {
                ReadPriority::Interactive => state.reads.interactive.push_back(pending),
                ReadPriority::Background => state.reads.background.push_back(pending),
            }
        }
        drop(state);
        self.shared.changed.notify_all();
        Ok(ReadTicket {
            id,
            receiver,
            marker: PhantomData,
            canceled,
        })
    }
}

struct Entry {
    value: Value,
    charge: usize,
    immutable: bool,
    used: u64,
}
#[derive(Default)]
pub(super) struct ReadCache {
    entries: BTreeMap<Vec<u8>, Entry>,
    bytes: usize,
    clock: u64,
}
impl ReadCache {
    pub(super) fn invalidate(&mut self, full: bool) {
        self.entries.retain(|_, entry| entry.immutable && !full);
        self.bytes = self.entries.values().map(|entry| entry.charge).sum();
    }
    pub(super) fn process_one(
        &mut self,
        shared: &Shared,
        executor: &mut dyn CommandExecutor,
        admission: ReadAdmission,
    ) -> bool {
        let (job, repository) = {
            let mut state = lock(&shared.state);
            (
                take_read(&mut state.reads.interactive, admission)
                    .or_else(|| take_read(&mut state.reads.background, admission)),
                state.snapshot.repository.clone(),
            )
        };
        let Some(job) = job else {
            return false;
        };
        self.clock = self.clock.saturating_add(1);
        let mut charge = 0;
        let cache_hit = self.entries.contains_key(&job.key);
        let cancellation = ReadCancellation {
            shared,
            key: &job.key,
            canceled: AtomicBool::new(false),
        };
        let result = if let Some(entry) = self.entries.get_mut(&job.key) {
            entry.used = self.clock;
            Ok(Arc::clone(&entry.value))
        } else if shared.cancel.load(Ordering::Acquire) {
            Err(closed())
        } else if cancellation.is_cancelled() {
            Err(RepositoryError::new(
                RepositoryErrorKind::Canceled,
                "repository read canceled",
            ))
        } else if let Some(repository) = repository {
            let mut executor = CancellableExecutor {
                inner: executor,
                cancellation: &cancellation,
            };
            (job.job)(&repository, &mut executor, &cancellation).map(|(value, retained)| {
                charge = retained.saturating_add(job.key.capacity());
                value
            })
        } else {
            Err(RepositoryError::new(
                RepositoryErrorKind::Discovery,
                "repository has not loaded",
            ))
        };
        if !cache_hit && job.cacheable {
            if let Ok(value) = &result {
                if charge <= shared.options.cache_bytes / 2 {
                    while !self.entries.is_empty()
                        && (self.entries.len() >= shared.options.cache_entries
                            || self.bytes.saturating_add(charge) > shared.options.cache_bytes / 2)
                    {
                        let key = self
                            .entries
                            .iter()
                            .min_by_key(|(_, entry)| entry.used)
                            .map(|(key, _)| key.clone());
                        if let Some(key) = key {
                            if let Some(entry) = self.entries.remove(&key) {
                                self.bytes -= entry.charge;
                            }
                        }
                    }
                    self.bytes += charge;
                    self.entries.insert(
                        job.key.clone(),
                        Entry {
                            value: Arc::clone(value),
                            charge,
                            immutable: job.immutable,
                            used: self.clock,
                        },
                    );
                }
            }
        }
        let subscribers = lock(&shared.state)
            .reads
            .subscribers
            .remove(&job.key)
            .unwrap_or_default();
        for (id, subscriber, _) in subscribers {
            let _ = subscriber.try_send(result.clone());
            emit(shared, RepositoryEvent::ReadCompleted { id, cache_hit });
        }
        true
    }
}

/// Answers reload tickets with the snapshot their refresh just published. Reads queued ahead
/// of a ticket would otherwise delay the caller that is waiting to report a mutation.
pub(super) fn complete_reload(shared: &Shared) {
    let (subscribers, snapshot) = {
        let mut state = lock(&shared.state);
        // A request the worker has not taken yet is answered by the refresh it starts.
        if state.refresh_pending || !state.reads.subscribers.contains_key(RELOAD_KEY) {
            return;
        }
        state
            .reads
            .interactive
            .retain(|read| read.key != RELOAD_KEY);
        (
            state
                .reads
                .subscribers
                .remove(RELOAD_KEY)
                .unwrap_or_default(),
            Arc::clone(&state.snapshot),
        )
    };
    let value: Value = Arc::new((*snapshot).clone());
    for (id, subscriber, _) in subscribers {
        let _ = subscriber.try_send(Ok(Arc::clone(&value)));
        emit(
            shared,
            RepositoryEvent::ReadCompleted {
                id,
                cache_hit: false,
            },
        );
    }
}

pub(super) fn has_pending(state: &State) -> bool {
    !state.reads.interactive.is_empty() || !state.reads.background.is_empty()
}
/// Whether an interactive read other than a reload ticket is waiting.
pub(super) fn has_interactive(state: &State) -> bool {
    state
        .reads
        .interactive
        .iter()
        .any(|read| read.key != RELOAD_KEY)
}
pub(super) fn cancel_pending(shared: &Shared) {
    let mut state = lock(&shared.state);
    for subscribers in std::mem::take(&mut state.reads.subscribers).into_values() {
        for (_, subscriber, _) in subscribers {
            let _ = subscriber.try_send(Err(closed()));
        }
    }
    state.reads.interactive.clear();
    state.reads.background.clear();
}

struct ReadCancellation<'a> {
    shared: &'a Shared,
    key: &'a [u8],
    canceled: AtomicBool,
}
impl Cancellation for ReadCancellation<'_> {
    fn is_cancelled(&self) -> bool {
        if self.canceled.load(Ordering::Acquire) {
            return true;
        }
        let canceled = self.shared.cancel.load(Ordering::Acquire)
            || lock(&self.shared.state)
                .reads
                .subscribers
                .get(self.key)
                .is_none_or(|subscribers| {
                    subscribers
                        .iter()
                        .all(|(_, _, canceled)| canceled.load(Ordering::Acquire))
                });
        if canceled {
            self.canceled.store(true, Ordering::Release);
        }
        canceled
    }
}
struct CancellableExecutor<'a> {
    inner: &'a mut dyn CommandExecutor,
    cancellation: &'a dyn Cancellation,
}
struct CombinedCancellation<'a>(&'a dyn Cancellation, Option<&'a dyn Cancellation>);
impl Cancellation for CombinedCancellation<'_> {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled() || self.1.is_some_and(Cancellation::is_cancelled)
    }
}
impl CommandExecutor for CancellableExecutor<'_> {
    fn execute(
        &mut self,
        command: crate::process::GitProcessCommand,
    ) -> crate::process::PrivateActivityResult {
        self.inner
            .execute_observing(command, None, Some(self.cancellation))
    }
    fn execute_observing(
        &mut self,
        command: crate::process::GitProcessCommand,
        observer: crate::process::PrivateStdoutObserver<'_>,
        cancellation: Option<&dyn Cancellation>,
    ) -> crate::process::PrivateActivityResult {
        self.inner.execute_observing(
            command,
            observer,
            Some(&CombinedCancellation(self.cancellation, cancellation)),
        )
    }
}

fn take_read(queue: &mut VecDeque<PendingRead>, admission: ReadAdmission) -> Option<PendingRead> {
    if admission == ReadAdmission::All {
        queue.pop_front()
    } else {
        let index = queue.iter().position(|read| admission.admits(read))?;
        queue.remove(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(key: &'static [u8]) -> PendingRead {
        PendingRead {
            key: key.to_vec(),
            immutable: false,
            scoped: false,
            cacheable: true,
            job: Box::new(|_, _, _| panic!("queue-order test must not execute jobs")),
        }
    }

    #[test]
    fn interactive_reads_overtake_queued_background_work_without_reordering_peers() {
        let mut queue = ReadQueue::default();
        queue.background.push_back(pending(b"background-1"));
        queue.background.push_back(pending(b"background-2"));
        queue.interactive.push_back(pending(b"interactive-1"));
        queue.interactive.push_back(pending(b"interactive-2"));

        let mut keys = Vec::new();
        while let Some(job) = queue
            .interactive
            .pop_front()
            .or_else(|| queue.background.pop_front())
        {
            keys.push(job.key);
        }
        assert_eq!(
            keys,
            vec![
                b"interactive-1".to_vec(),
                b"interactive-2".to_vec(),
                b"background-1".to_vec(),
                b"background-2".to_vec(),
            ]
        );
    }
}
