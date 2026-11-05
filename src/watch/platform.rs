//! Native-primary platform watching and an explicitly selected polling fallback.
//!
//! The native adapter uses notify's recommended OS backend. Polling remains deliberately bounded
//! and is sufficient as a fallback because overflow always requests a complete refresh.

use super::{
    PlatformWatcherPort, RepositoryWatchPlan, RepositoryWatcherPort, TemporaryPathRules,
    WatchBatch, WatchChange, WatchChangeKind, WatchError, WatchErrorKind, WatchScope, WatchTarget,
};
use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher, WatcherKind};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, PoisonError, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

const NATIVE_CHANNEL_CAPACITY: usize = 4096;
const NATIVE_BATCH_LIMIT: usize = 4096;
const NATIVE_MAX_WATCH_ROOTS: usize = 50_000;
const FALLBACK_POLL_INTERVAL: Duration = Duration::from_millis(500);
const FALLBACK_MAX_EVENTS: usize = 4096;
const FALLBACK_MAX_ENTRIES: usize = 50_000;

#[derive(Clone, Copy, Debug)]
pub struct NativeWatcherFactory {
    channel_capacity: usize,
    batch_limit: usize,
}

impl Default for NativeWatcherFactory {
    fn default() -> Self {
        Self::native_primary()
    }
}

impl NativeWatcherFactory {
    pub const fn native_primary() -> Self {
        Self {
            channel_capacity: NATIVE_CHANNEL_CAPACITY,
            batch_limit: NATIVE_BATCH_LIMIT,
        }
    }

    /// Configures bounded callback and publication capacities.
    ///
    /// # Errors
    ///
    /// Both capacities must be nonzero.
    pub fn with_bounds(channel_capacity: usize, batch_limit: usize) -> Result<Self, WatchError> {
        if channel_capacity == 0 || batch_limit == 0 {
            return Err(WatchError::new(
                WatchErrorKind::Capacity,
                "native watcher capacities must be nonzero",
            ));
        }
        Ok(Self {
            channel_capacity,
            batch_limit,
        })
    }

    fn backend_kind() -> WatcherKind {
        <RecommendedWatcher as Watcher>::kind()
    }

    pub fn native_backend_available() -> bool {
        validate_native_backend(Self::backend_kind()).is_ok()
    }
}

impl PlatformWatcherPort for NativeWatcherFactory {
    fn covers_recursively(&self) -> bool {
        native_recursive()
    }

    fn watch(
        &self,
        plan: &RepositoryWatchPlan,
    ) -> Result<Box<dyn RepositoryWatcherPort>, WatchError> {
        validate_native_backend(Self::backend_kind())?;
        Ok(Box::new(NativeRepositoryWatcher::new(
            plan.targets.clone(),
            plan.excluded_worktree_roots.clone(),
            plan.temporary_paths.clone(),
            self.channel_capacity,
            self.batch_limit,
        )?))
    }
}

fn validate_native_backend(kind: WatcherKind) -> Result<(), WatchError> {
    match kind {
        WatcherKind::PollWatcher | WatcherKind::NullWatcher => Err(WatchError::new(
            WatchErrorKind::Unsupported,
            "notify recommended a polling or null watcher where a native backend is required",
        )),
        WatcherKind::Inotify
        | WatcherKind::Fsevent
        | WatcherKind::Kqueue
        | WatcherKind::ReadDirectoryChangesWatcher => Ok(()),
        _ => Err(WatchError::new(
            WatchErrorKind::Unsupported,
            "notify recommended an unknown watcher backend",
        )),
    }
}

type NativeMessage = notify::Result<Event>;

fn native_recursive() -> bool {
    matches!(
        <RecommendedWatcher as Watcher>::kind(),
        WatcherKind::Fsevent | WatcherKind::ReadDirectoryChangesWatcher
    )
}

pub struct NativeRepositoryWatcher {
    watcher: Option<RecommendedWatcher>,
    receiver: Receiver<NativeMessage>,
    overflow: Arc<AtomicBool>,
    targets: Vec<WatchTarget>,
    // Shared with the backend callback so exclusions can change without a registration gap.
    excluded_worktree_roots: Arc<RwLock<Vec<PathBuf>>>,
    watched: BTreeMap<PathBuf, RecursiveMode>,
    batch_limit: usize,
    temporary_paths: TemporaryPathRules,
}

impl NativeRepositoryWatcher {
    fn new(
        targets: Vec<WatchTarget>,
        excluded_worktree_roots: Vec<PathBuf>,
        temporary_paths: TemporaryPathRules,
        channel_capacity: usize,
        batch_limit: usize,
    ) -> Result<Self, WatchError> {
        let (sender, receiver) = mpsc::sync_channel(channel_capacity);
        let overflow = Arc::new(AtomicBool::new(false));
        let callback_overflow = Arc::clone(&overflow);
        let callback_targets = targets.clone();
        let excluded_worktree_roots = Arc::new(RwLock::new(excluded_worktree_roots));
        let callback_exclusions = Arc::clone(&excluded_worktree_roots);
        let callback_temporary = temporary_paths.clone();
        let watcher = notify::recommended_watcher(move |message| {
            send_native_message(
                &sender,
                &callback_overflow,
                &callback_targets,
                &callback_exclusions
                    .read()
                    .unwrap_or_else(PoisonError::into_inner),
                &callback_temporary,
                message,
            );
        })
        .map_err(|error| native_initialization_error("create native watcher", &error))?;
        let mut native = Self {
            watcher: Some(watcher),
            receiver,
            overflow,
            targets,
            excluded_worktree_roots,
            watched: BTreeMap::new(),
            batch_limit,
            temporary_paths,
        };
        native.ensure_watches()?;
        Ok(native)
    }

    fn ensure_watches(&mut self) -> Result<(), WatchError> {
        let desired = desired_watch_roots(
            &self.targets,
            &self
                .excluded_worktree_roots
                .read()
                .unwrap_or_else(PoisonError::into_inner),
            native_recursive(),
        )?;
        let watcher = self
            .watcher
            .as_mut()
            .ok_or_else(|| WatchError::new(WatchErrorKind::Closed, "native watcher is closed"))?;
        let vanished: Vec<_> = self
            .watched
            .iter()
            .filter(|(path, _)| !path.is_dir())
            .map(|(path, _)| path.clone())
            .collect();
        for path in vanished {
            let _ = watcher.unwatch(&path);
            self.watched.remove(&path);
        }
        for (path, mode) in desired {
            if self.watched.get(&path) == Some(&mode) {
                continue;
            }
            if self.watched.contains_key(&path) {
                let _ = watcher.unwatch(&path);
            }
            watcher
                .watch(&path, mode)
                .map_err(|error| native_initialization_error("register native watch", &error))?;
            self.watched.insert(path, mode);
        }
        Ok(())
    }

    fn absorb(&self, message: NativeMessage, batch: &mut WatchBatch) {
        match message {
            Ok(event) => {
                batch.overflow |= event.need_rescan();
                if event.kind.is_access() {
                    return;
                }
                let kind = map_event_kind(event.kind);
                for path in event.paths {
                    if !self.path_is_relevant(&path) {
                        continue;
                    }
                    if batch.changes.len() < self.batch_limit {
                        batch.changes.push(WatchChange { path, kind });
                    } else {
                        batch.overflow = true;
                    }
                }
            }
            Err(error) => {
                batch.overflow = true;
                for path in error.paths {
                    if self.path_is_relevant(&path) && batch.changes.len() < self.batch_limit {
                        batch.changes.push(WatchChange {
                            path,
                            kind: WatchChangeKind::Modified,
                        });
                    }
                }
            }
        }
    }

    fn path_is_relevant(&self, path: &Path) -> bool {
        path_is_relevant_to_targets(
            path,
            &self.targets,
            &self
                .excluded_worktree_roots
                .read()
                .unwrap_or_else(PoisonError::into_inner),
            &self.temporary_paths,
        )
    }
}

impl RepositoryWatcherPort for NativeRepositoryWatcher {
    fn update_exclusions(&mut self, excluded_worktree_roots: &[PathBuf]) -> bool {
        // Explicit-watch backends skip ignored directories while registering, so only a
        // recursive registration is independent of the exclusions.
        if !native_recursive() {
            return false;
        }
        excluded_worktree_roots.clone_into(
            &mut self
                .excluded_worktree_roots
                .write()
                .unwrap_or_else(PoisonError::into_inner),
        );
        true
    }

    fn wait(&mut self, timeout: Duration) -> Result<WatchBatch, WatchError> {
        let mut batch = WatchBatch {
            changes: Vec::new(),
            overflow: self.overflow.swap(false, Ordering::AcqRel),
        };
        if !batch.overflow {
            match self.receiver.recv_timeout(timeout) {
                Ok(message) => self.absorb(message, &mut batch),
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(batch),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(WatchError::new(
                        WatchErrorKind::Closed,
                        "native watcher callback channel closed",
                    ));
                }
            }
        }
        loop {
            match self.receiver.try_recv() {
                Ok(message) => self.absorb(message, &mut batch),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    batch.overflow = true;
                    break;
                }
            }
        }
        batch.overflow |= self.overflow.swap(false, Ordering::AcqRel);
        let watch_tree_changed = batch.changes.iter().any(|change| {
            (matches!(
                change.kind,
                WatchChangeKind::Created | WatchChangeKind::Renamed
            ) && change.path.is_dir())
                || (matches!(
                    change.kind,
                    WatchChangeKind::Removed | WatchChangeKind::Renamed
                ) && self.watched.contains_key(&change.path))
        });
        if watch_tree_changed {
            self.ensure_watches()?;
        }
        Ok(batch)
    }
}

impl Drop for NativeRepositoryWatcher {
    fn drop(&mut self) {
        // Dropping the backend first stops its callback thread and disconnects its sender before
        // the receiver and filter state are released.
        drop(self.watcher.take());
    }
}

fn send_native_message(
    sender: &SyncSender<NativeMessage>,
    overflow: &AtomicBool,
    targets: &[WatchTarget],
    excluded_worktree_roots: &[PathBuf],
    temporary_paths: &TemporaryPathRules,
    message: NativeMessage,
) {
    match &message {
        Ok(event) if event.kind.is_access() => return,
        Ok(event)
            if !event.paths.is_empty()
                && !event.paths.iter().any(|path| {
                    path_is_relevant_to_targets(
                        path,
                        targets,
                        excluded_worktree_roots,
                        temporary_paths,
                    )
                }) =>
        {
            return;
        }
        Err(error)
            if !error.paths.is_empty()
                && !error.paths.iter().any(|path| {
                    path_is_relevant_to_targets(
                        path,
                        targets,
                        excluded_worktree_roots,
                        temporary_paths,
                    )
                }) =>
        {
            return;
        }
        Ok(_) | Err(_) => {}
    }
    match sender.try_send(message) {
        Err(mpsc::TrySendError::Full(_)) => overflow.store(true, Ordering::Release),
        Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => {}
    }
}

fn path_is_relevant_to_targets(
    path: &Path,
    targets: &[WatchTarget],
    excluded_worktree_roots: &[PathBuf],
    temporary_paths: &TemporaryPathRules,
) -> bool {
    if temporary_paths.matches(path) {
        return false;
    }
    let inside_common_metadata = targets.iter().any(|target| {
        target.scope == WatchScope::CommonGitDirectory
            && !target.recursive
            && path.starts_with(&target.path)
    });
    targets.iter().any(|target| {
        if target.scope == WatchScope::Worktree && inside_common_metadata {
            return false;
        }
        if target.scope == WatchScope::Worktree
            && excluded_worktree_roots
                .iter()
                .any(|excluded| path == excluded || path.starts_with(excluded))
        {
            return false;
        }
        if target.recursive {
            path == target.path || path.starts_with(&target.path)
        } else {
            path == target.path
        }
    })
}

fn desired_watch_roots(
    targets: &[WatchTarget],
    excluded_worktree_roots: &[PathBuf],
    native_recursive: bool,
) -> Result<BTreeMap<PathBuf, RecursiveMode>, WatchError> {
    let mut roots = BTreeMap::new();
    let metadata_roots = targets
        .iter()
        .filter_map(|target| match target.scope {
            WatchScope::CommonGitDirectory => Some(target.path.clone()),
            WatchScope::Index => target.path.parent().map(Path::to_path_buf),
            _ => None,
        })
        .collect::<Vec<_>>();
    for target in targets {
        if target.recursive && target.path.is_dir() {
            if native_recursive {
                insert_watch_mode(&mut roots, target.path.clone(), RecursiveMode::Recursive);
            } else {
                add_directory_watch_roots(
                    &target.path,
                    target.scope == WatchScope::Worktree,
                    excluded_worktree_roots,
                    &metadata_roots,
                    &mut roots,
                )?;
            }
            if let Some(parent) = target.path.parent() {
                insert_watch_mode(
                    &mut roots,
                    parent.to_path_buf(),
                    RecursiveMode::NonRecursive,
                );
            }
        } else {
            let root = nearest_existing_directory(&target.path)?;
            insert_watch_mode(&mut roots, root, RecursiveMode::NonRecursive);
        }
    }
    if native_recursive {
        // A recursive registration already covers every descendant target. Keeping nested roots
        // needlessly restarts the FSEvents stream once per registration on macOS.
        let recursive = roots
            .iter()
            .filter_map(|(path, mode)| (*mode == RecursiveMode::Recursive).then_some(path.clone()))
            .collect::<Vec<_>>();
        roots.retain(|path, _| {
            !recursive
                .iter()
                .any(|parent| path != parent && path.starts_with(parent))
        });
    }
    Ok(roots)
}

fn add_directory_watch_roots(
    root: &Path,
    worktree_scope: bool,
    excluded_worktree_roots: &[PathBuf],
    metadata_roots: &[PathBuf],
    roots: &mut BTreeMap<PathBuf, RecursiveMode>,
) -> Result<(), WatchError> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        if worktree_scope
            && (excluded_worktree_roots
                .iter()
                .any(|excluded| path == excluded.as_path() || path.starts_with(excluded))
                || metadata_roots
                    .iter()
                    .any(|metadata| path == metadata.as_path() || path.starts_with(metadata)))
        {
            continue;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error(&path, &error)),
        };
        if !metadata.file_type().is_dir() {
            continue;
        }
        if roots.len() == NATIVE_MAX_WATCH_ROOTS && !roots.contains_key(&path) {
            return Err(WatchError::new(
                WatchErrorKind::Capacity,
                "repository contains too many non-ignored directories to watch",
            ));
        }
        insert_watch_mode(roots, path.clone(), RecursiveMode::NonRecursive);
        let children = fs::read_dir(&path).map_err(|error| io_error(&path, &error))?;
        for child in children {
            let child = child.map_err(|error| io_error(&path, &error))?;
            if child
                .file_type()
                .map_err(|error| io_error(&child.path(), &error))?
                .is_dir()
            {
                pending.push(child.path());
            }
        }
    }
    Ok(())
}

fn insert_watch_mode(
    roots: &mut BTreeMap<PathBuf, RecursiveMode>,
    path: PathBuf,
    mode: RecursiveMode,
) {
    roots
        .entry(path)
        .and_modify(|existing| {
            if mode == RecursiveMode::Recursive {
                *existing = RecursiveMode::Recursive;
            }
        })
        .or_insert(mode);
}

fn nearest_existing_directory(path: &Path) -> Result<PathBuf, WatchError> {
    let mut candidate = if path.is_dir() {
        Some(path)
    } else {
        path.parent()
    };
    while let Some(current) = candidate {
        if current.is_dir() {
            return Ok(current.to_path_buf());
        }
        candidate = current.parent();
    }
    Err(WatchError::new(
        WatchErrorKind::Io,
        format!("no existing parent directory for {}", path.display()),
    ))
}

fn map_event_kind(kind: EventKind) -> WatchChangeKind {
    match kind {
        EventKind::Create(_) => WatchChangeKind::Created,
        EventKind::Remove(_) => WatchChangeKind::Removed,
        EventKind::Modify(ModifyKind::Name(_)) => WatchChangeKind::Renamed,
        EventKind::Modify(_) | EventKind::Any | EventKind::Other | EventKind::Access(_) => {
            WatchChangeKind::Modified
        }
    }
}

fn native_initialization_error(context: &str, error: &notify::Error) -> WatchError {
    let kind = match error.kind {
        notify::ErrorKind::MaxFilesWatch => WatchErrorKind::Capacity,
        _ => WatchErrorKind::Io,
    };
    WatchError::new(kind, format!("{context}: {error}"))
}

#[derive(Clone, Copy, Debug)]
pub struct PollingWatcherFactory {
    poll_interval: Duration,
    max_events: usize,
    max_entries: usize,
}

impl PollingWatcherFactory {
    /// Constructs the conservative, explicitly selected fallback. Production composition must not
    /// silently treat this as the native-primary backend.
    pub const fn bounded_fallback() -> Self {
        Self {
            poll_interval: FALLBACK_POLL_INTERVAL,
            max_events: FALLBACK_MAX_EVENTS,
            max_entries: FALLBACK_MAX_ENTRIES,
        }
    }

    /// Configures deterministic bounds, primarily for tests and constrained platforms.
    ///
    /// # Errors
    ///
    /// All bounds must be nonzero.
    pub fn with_bounds(
        poll_interval: Duration,
        max_events: usize,
        max_entries: usize,
    ) -> Result<Self, WatchError> {
        if poll_interval.is_zero() || max_events == 0 || max_entries == 0 {
            return Err(WatchError::new(
                WatchErrorKind::Capacity,
                "watcher bounds must be nonzero",
            ));
        }
        Ok(Self {
            poll_interval,
            max_events,
            max_entries,
        })
    }

    pub const fn policy(&self) -> PollingPolicy {
        PollingPolicy {
            poll_interval: self.poll_interval,
            max_events: self.max_events,
            max_entries: self.max_entries,
        }
    }

    /// Measures one bounded snapshot without starting a watcher.
    ///
    /// # Errors
    ///
    /// Returns a typed filesystem error if a watched path cannot be inspected.
    pub fn measure_snapshot(
        &self,
        plan: &RepositoryWatchPlan,
    ) -> Result<PollingScanMeasurement, WatchError> {
        let started = Instant::now();
        let snapshot = snapshot(
            &plan.targets,
            &plan.excluded_worktree_roots,
            &plan.temporary_paths,
            self.max_entries,
        )?;
        Ok(PollingScanMeasurement {
            entries: snapshot.entries.len(),
            elapsed: started.elapsed(),
            overflow: snapshot.overflow,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PollingPolicy {
    pub poll_interval: Duration,
    pub max_events: usize,
    pub max_entries: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PollingScanMeasurement {
    pub entries: usize,
    pub elapsed: Duration,
    pub overflow: bool,
}

impl PlatformWatcherPort for PollingWatcherFactory {
    fn watch(
        &self,
        plan: &RepositoryWatchPlan,
    ) -> Result<Box<dyn RepositoryWatcherPort>, WatchError> {
        Ok(Box::new(PollingRepositoryWatcher::with_temporary_paths(
            plan.targets.clone(),
            plan.excluded_worktree_roots.clone(),
            plan.temporary_paths.clone(),
            PollingPolicy {
                poll_interval: self.poll_interval,
                max_events: self.max_events,
                max_entries: self.max_entries,
            },
        )?))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Fingerprint {
    kind: EntryKind,
    len: u64,
    modified: Option<SystemTime>,
}

struct Snapshot {
    entries: BTreeMap<PathBuf, Fingerprint>,
    overflow: bool,
}

pub struct PollingRepositoryWatcher {
    targets: Vec<WatchTarget>,
    excluded_worktree_roots: Vec<PathBuf>,
    baseline: BTreeMap<PathBuf, Fingerprint>,
    poll_interval: Duration,
    max_events: usize,
    max_entries: usize,
    temporary_paths: TemporaryPathRules,
}

impl PollingRepositoryWatcher {
    /// Uses the default [`TemporaryPathRules`].
    ///
    /// # Errors
    ///
    /// Returns filesystem and capacity errors encountered while establishing the baseline.
    pub fn new(
        targets: Vec<WatchTarget>,
        excluded_worktree_roots: Vec<PathBuf>,
        poll_interval: Duration,
        max_events: usize,
        max_entries: usize,
    ) -> Result<Self, WatchError> {
        Self::with_temporary_paths(
            targets,
            excluded_worktree_roots,
            TemporaryPathRules::default(),
            PollingPolicy {
                poll_interval,
                max_events,
                max_entries,
            },
        )
    }

    /// # Errors
    ///
    /// Returns filesystem and capacity errors encountered while establishing the baseline.
    pub fn with_temporary_paths(
        targets: Vec<WatchTarget>,
        excluded_worktree_roots: Vec<PathBuf>,
        temporary_paths: TemporaryPathRules,
        policy: PollingPolicy,
    ) -> Result<Self, WatchError> {
        let PollingPolicy {
            poll_interval,
            max_events,
            max_entries,
        } = policy;
        if poll_interval.is_zero() || max_events == 0 || max_entries == 0 {
            return Err(WatchError::new(
                WatchErrorKind::Capacity,
                "watcher bounds must be nonzero",
            ));
        }
        let initial = snapshot(
            &targets,
            &excluded_worktree_roots,
            &temporary_paths,
            max_entries,
        )?;
        Ok(Self {
            targets,
            excluded_worktree_roots,
            baseline: initial.entries,
            poll_interval,
            max_events,
            max_entries,
            temporary_paths,
        })
    }

    fn scan(&mut self) -> Result<WatchBatch, WatchError> {
        let current = snapshot(
            &self.targets,
            &self.excluded_worktree_roots,
            &self.temporary_paths,
            self.max_entries,
        )?;
        let mut paths = BTreeSet::new();
        paths.extend(self.baseline.keys().cloned());
        paths.extend(current.entries.keys().cloned());
        let mut changes = Vec::new();
        let mut overflow = current.overflow;
        for path in paths {
            let kind = match (self.baseline.get(&path), current.entries.get(&path)) {
                (None, Some(_)) => Some(WatchChangeKind::Created),
                (Some(_), None) => Some(WatchChangeKind::Removed),
                (Some(before), Some(after)) if before != after => Some(WatchChangeKind::Modified),
                _ => None,
            };
            if let Some(kind) = kind {
                if changes.len() < self.max_events {
                    changes.push(WatchChange { path, kind });
                } else {
                    overflow = true;
                }
            }
        }
        self.baseline = current.entries;
        Ok(WatchBatch { changes, overflow })
    }
}

impl RepositoryWatcherPort for PollingRepositoryWatcher {
    fn wait(&mut self, timeout: Duration) -> Result<WatchBatch, WatchError> {
        let deadline = Instant::now() + timeout;
        loop {
            let batch = self.scan()?;
            if batch.overflow || !batch.changes.is_empty() || Instant::now() >= deadline {
                return Ok(batch);
            }
            thread::sleep(
                self.poll_interval
                    .min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
}

fn snapshot(
    targets: &[WatchTarget],
    excluded_worktree_roots: &[PathBuf],
    temporary_paths: &TemporaryPathRules,
    max_entries: usize,
) -> Result<Snapshot, WatchError> {
    let mut entries = BTreeMap::new();
    let mut overflow = false;
    for target in targets {
        scan_path(
            &target.path,
            target.recursive,
            target.scope == WatchScope::Worktree,
            excluded_worktree_roots,
            temporary_paths,
            &mut entries,
            max_entries,
            &mut overflow,
        )?;
    }
    Ok(Snapshot { entries, overflow })
}

#[allow(clippy::too_many_arguments)]
fn scan_path(
    path: &Path,
    recursive: bool,
    worktree_scope: bool,
    excluded_worktree_roots: &[PathBuf],
    temporary_paths: &TemporaryPathRules,
    entries: &mut BTreeMap<PathBuf, Fingerprint>,
    max_entries: usize,
    overflow: &mut bool,
) -> Result<(), WatchError> {
    if temporary_paths.matches(path)
        || (worktree_scope
            && excluded_worktree_roots
                .iter()
                .any(|excluded| path == excluded || path.starts_with(excluded)))
    {
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error(path, &error)),
    };
    if entries.len() >= max_entries && !entries.contains_key(path) {
        *overflow = true;
        return Ok(());
    }
    let file_type = metadata.file_type();
    let kind = if file_type.is_file() {
        EntryKind::File
    } else if file_type.is_dir() {
        EntryKind::Directory
    } else if file_type.is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::Other
    };
    entries.insert(
        path.to_path_buf(),
        Fingerprint {
            kind,
            len: if file_type.is_dir() {
                0
            } else {
                metadata.len()
            },
            modified: if file_type.is_dir() {
                None
            } else {
                metadata.modified().ok()
            },
        },
    );
    if recursive && file_type.is_dir() && !*overflow {
        let children = fs::read_dir(path).map_err(|error| io_error(path, &error))?;
        for child in children {
            let child = child.map_err(|error| io_error(path, &error))?;
            scan_path(
                &child.path(),
                true,
                worktree_scope,
                excluded_worktree_roots,
                temporary_paths,
                entries,
                max_entries,
                overflow,
            )?;
            if *overflow {
                break;
            }
        }
    }
    Ok(())
}

fn io_error(path: &Path, error: &io::Error) -> WatchError {
    WatchError::new(
        WatchErrorKind::Io,
        format!("could not watch {}: {error}", path.display()),
    )
}

#[cfg(test)]
mod p02_native_watch_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn polling_and_null_backends_are_rejected_as_native_primary() {
        assert_eq!(
            validate_native_backend(WatcherKind::PollWatcher)
                .unwrap_err()
                .kind,
            WatchErrorKind::Unsupported
        );
        assert_eq!(
            validate_native_backend(WatcherKind::NullWatcher)
                .unwrap_err()
                .kind,
            WatchErrorKind::Unsupported
        );
    }

    #[test]
    fn disconnected_native_callback_drops_messages_without_panicking() {
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        let overflow = AtomicBool::new(false);
        send_native_message(
            &sender,
            &overflow,
            &[],
            &[],
            &TemporaryPathRules::default(),
            Ok(Event::new(EventKind::Other)),
        );
        assert!(!overflow.load(Ordering::Acquire));
    }

    #[test]
    fn callback_discards_access_and_git_object_events_before_the_bounded_queue() {
        let targets = vec![
            WatchTarget {
                path: PathBuf::from("/repo"),
                recursive: true,
                scope: WatchScope::Worktree,
            },
            WatchTarget {
                path: PathBuf::from("/repo/.git"),
                recursive: false,
                scope: WatchScope::CommonGitDirectory,
            },
            WatchTarget {
                path: PathBuf::from("/repo/.git/refs"),
                recursive: true,
                scope: WatchScope::Refs,
            },
        ];
        assert!(path_is_relevant_to_targets(
            Path::new("/repo/src/main.rs"),
            &targets,
            &[],
            &TemporaryPathRules::default(),
        ));
        assert!(path_is_relevant_to_targets(
            Path::new("/repo/.git/refs/heads/main"),
            &targets,
            &[],
            &TemporaryPathRules::default(),
        ));
        assert!(!path_is_relevant_to_targets(
            Path::new("/repo/.git/objects/aa/object"),
            &targets,
            &[],
            &TemporaryPathRules::default(),
        ));
        assert!(!path_is_relevant_to_targets(
            Path::new("/repo/target/debug/binary"),
            &targets,
            &[PathBuf::from("/repo/target")],
            &TemporaryPathRules::default(),
        ));
        assert!(path_is_relevant_to_targets(
            Path::new("/repo/.git/refs/heads/main"),
            &targets,
            &[PathBuf::from("/repo/target")],
            &TemporaryPathRules::default(),
        ));

        let (sender, receiver) = mpsc::sync_channel(1);
        let overflow = AtomicBool::new(false);
        let access = Event::new(EventKind::Access(notify::event::AccessKind::Any))
            .add_path(PathBuf::from("/repo/src/main.rs"));
        send_native_message(
            &sender,
            &overflow,
            &targets,
            &[],
            &TemporaryPathRules::default(),
            Ok(access),
        );
        let object = Event::new(EventKind::Create(notify::event::CreateKind::File))
            .add_path(PathBuf::from("/repo/.git/objects/aa/object"));
        send_native_message(
            &sender,
            &overflow,
            &targets,
            &[],
            &TemporaryPathRules::default(),
            Ok(object),
        );
        let source = Event::new(EventKind::Modify(ModifyKind::Any))
            .add_path(PathBuf::from("/repo/src/main.rs"));
        send_native_message(
            &sender,
            &overflow,
            &targets,
            &[],
            &TemporaryPathRules::default(),
            Ok(source),
        );

        assert!(receiver.try_recv().is_ok());
        assert!(!overflow.load(Ordering::Acquire));
    }

    #[test]
    fn callback_discards_ignored_burst_before_the_bounded_queue() {
        let targets = vec![WatchTarget {
            path: PathBuf::from("/repo"),
            recursive: true,
            scope: WatchScope::Worktree,
        }];
        let excluded_worktree_roots = vec![PathBuf::from("/repo/target")];
        let (sender, receiver) = mpsc::sync_channel(1);
        let overflow = AtomicBool::new(false);

        for number in 0..500 {
            let ignored = Event::new(EventKind::Create(notify::event::CreateKind::File))
                .add_path(PathBuf::from(format!("/repo/target/ignored-{number}")));
            send_native_message(
                &sender,
                &overflow,
                &targets,
                &excluded_worktree_roots,
                &TemporaryPathRules::default(),
                Ok(ignored),
            );
        }
        let source = Event::new(EventKind::Modify(ModifyKind::Any))
            .add_path(PathBuf::from("/repo/src/main.rs"));
        send_native_message(
            &sender,
            &overflow,
            &targets,
            &excluded_worktree_roots,
            &TemporaryPathRules::default(),
            Ok(source),
        );

        let queued = receiver.try_recv().unwrap().unwrap();
        assert_eq!(queued.paths, vec![PathBuf::from("/repo/src/main.rs")]);
        assert!(receiver.try_recv().is_err());
        assert!(!overflow.load(Ordering::Acquire));
    }

    #[test]
    fn watch_roots_use_native_recursion_or_explicit_filtered_directories() {
        let root = std::env::temp_dir().join(format!(
            "devmil-git-native-watch-roots-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::create_dir_all(root.join("target/debug/deps")).unwrap();
        fs::create_dir_all(root.join(".git/objects/aa")).unwrap();
        fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        let targets = vec![
            WatchTarget {
                path: root.clone(),
                recursive: true,
                scope: WatchScope::Worktree,
            },
            WatchTarget {
                path: root.join(".git/index"),
                recursive: false,
                scope: WatchScope::Index,
            },
            WatchTarget {
                path: root.join(".git"),
                recursive: false,
                scope: WatchScope::CommonGitDirectory,
            },
            WatchTarget {
                path: root.join(".git/refs"),
                recursive: true,
                scope: WatchScope::Refs,
            },
        ];

        let roots = desired_watch_roots(&targets, &[root.join("target")], false).unwrap();
        assert!(roots.contains_key(&root));
        assert!(roots.contains_key(&root.join("src/nested")));
        assert!(roots.contains_key(&root.join(".git/refs/heads")));
        assert!(!roots.contains_key(&root.join("target")));
        assert!(!roots.contains_key(&root.join("target/debug/deps")));
        assert!(!roots.contains_key(&root.join(".git/objects")));
        assert!(roots
            .values()
            .all(|mode| *mode == RecursiveMode::NonRecursive));

        let native_roots = desired_watch_roots(&targets, &[root.join("target")], true).unwrap();
        assert_eq!(native_roots.get(&root), Some(&RecursiveMode::Recursive));
        assert_eq!(
            native_roots.len(),
            2,
            "only the recursive worktree and its replacement-detection parent need registration"
        );
        assert!(!native_roots.contains_key(&root.join(".git/refs")));
        assert!(!native_roots.contains_key(&root.join("src")));
        assert!(!native_roots.contains_key(&root.join("target")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_recursive_coverage_preserves_external_metadata_roots() {
        let root = std::env::temp_dir().join(format!(
            "devmil-watch-linked-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        fs::create_dir_all(root.join("worktree/src")).unwrap();
        fs::create_dir_all(root.join("external/refs/heads")).unwrap();
        let targets = vec![
            WatchTarget {
                path: root.join("worktree"),
                recursive: true,
                scope: WatchScope::Worktree,
            },
            WatchTarget {
                path: root.join("worktree/src"),
                recursive: true,
                scope: WatchScope::Worktree,
            },
            WatchTarget {
                path: root.join("external/refs"),
                recursive: true,
                scope: WatchScope::Refs,
            },
            WatchTarget {
                path: root.join("external/index"),
                recursive: false,
                scope: WatchScope::Index,
            },
        ];
        let roots = desired_watch_roots(&targets, &[], true).unwrap();
        assert_eq!(roots.len(), 4);
        assert_eq!(
            roots.get(&root.join("worktree")),
            Some(&RecursiveMode::Recursive)
        );
        assert!(!roots.contains_key(&root.join("worktree/src")));
        assert_eq!(
            roots.get(&root.join("external/refs")),
            Some(&RecursiveMode::Recursive)
        );
        assert_eq!(
            roots.get(&root.join("external")),
            Some(&RecursiveMode::NonRecursive)
        );
        assert_eq!(roots.get(&root), Some(&RecursiveMode::NonRecursive));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pruned_native_registration_still_observes_index_and_reference_changes() {
        let directory = std::env::temp_dir().join(format!(
            "devmil-watch-metadata-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        fs::create_dir_all(directory.join(".git/refs/heads")).unwrap();
        let root = fs::canonicalize(&directory).unwrap();
        let index = root.join(".git/index");
        let reference = root.join(".git/refs/heads/main");
        let targets = vec![
            WatchTarget {
                path: root.clone(),
                recursive: true,
                scope: WatchScope::Worktree,
            },
            WatchTarget {
                path: index.clone(),
                recursive: false,
                scope: WatchScope::Index,
            },
            WatchTarget {
                path: root.join(".git/refs"),
                recursive: true,
                scope: WatchScope::Refs,
            },
        ];
        let mut watcher = NativeRepositoryWatcher::new(
            targets,
            Vec::new(),
            TemporaryPathRules::default(),
            64,
            64,
        )
        .unwrap();
        fs::write(&index, b"index").unwrap();
        fs::write(&reference, b"ref").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut observed = std::collections::BTreeSet::new();
        while std::time::Instant::now() < deadline
            && !(observed.contains(&index) && observed.contains(&reference))
        {
            observed.extend(
                watcher
                    .wait(Duration::from_millis(100))
                    .unwrap()
                    .changes
                    .into_iter()
                    .map(|change| change.path),
            );
        }
        assert!(
            observed.contains(&index),
            "index event was lost after recursive root pruning"
        );
        assert!(
            observed.contains(&reference),
            "reference event was lost after recursive root pruning"
        );
        drop(watcher);
        fs::remove_dir_all(directory).unwrap();
    }
}
