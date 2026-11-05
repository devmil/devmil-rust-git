use super::status_cache::StatusCache;
use super::{
    lock, publish, reads, ActivityId, Arc, DiscoveredRepository, Duration, Ordering,
    RepositoryError, RepositoryErrorKind, RepositoryPhase, RepositorySnapshot, Shared,
    NEXT_ACTIVITY,
};
use crate::loading::refs::{ReferenceLoader, RefsRequest};
use crate::loading::status::StatusOptions;
use crate::process::{ActivityGitRunner, GitProcessCommand, PrivateActivityResult};
use crate::repository::{DiscoveryRequest, RepositoryDiscovery, RepositoryKind};
use crate::types::RepositoryPath;
use crate::watch::platform::{NativeWatcherFactory, PollingWatcherFactory};
use crate::watch::{
    PlatformWatcherPort, RefreshCoordinator, RepositoryWatchPlan, RepositoryWatcherPort,
};
use std::time::Instant;

#[allow(clippy::too_many_lines)] // Owns the repository lifecycle and publishes each transition atomically.
pub(super) fn run(path: &std::path::Path, shared: &Arc<Shared>) {
    let mut watcher = WatchRegistration::default();
    let mut opening_watcher: Option<
        std::thread::JoinHandle<(WatchRegistration, Option<RepositoryError>)>,
    > = None;
    let mut discovering: Option<std::thread::JoinHandle<Option<RepositoryWatchPlan>>> = None;
    let mut rediscover = false;
    let mut refresh: Option<RefreshCoordinator> = None;
    let mut refreshing: Option<RefreshWork> = None;
    let mut status_cache = StatusCache::default();
    let mut pending_full = false;
    let mut pending_refresh = false;
    let mut pending_requested = false;
    // A scan that closes a watch-registration gap must not let reads overtake it.
    let mut pending_gap = false;
    let mut block_reads_until_refresh = false;
    let started = Instant::now();
    let mut cache = reads::ReadCache::default();
    loop {
        if shared.cancel.load(Ordering::Acquire) {
            break;
        }
        let Some(activity) = super::mutation::enter_worker(shared) else {
            break;
        };
        let (load, requested) = {
            let mut state = lock(&shared.state);
            (
                std::mem::take(&mut state.load_pending),
                std::mem::take(&mut state.refresh_pending),
            )
        };
        block_reads_until_refresh |= requested;
        let mut initialized = false;
        if load {
            let mut snapshot = (*lock(&shared.state).snapshot).clone();
            if snapshot.repository.is_none() || snapshot.phase == RepositoryPhase::Unloaded {
                snapshot.phase = RepositoryPhase::Loading;
                publish(shared, snapshot.clone());
                match snapshot
                    .repository
                    .clone()
                    .map_or_else(|| discover(path, shared), Ok)
                {
                    Ok(repository) => {
                        snapshot.repository = Some(repository.clone());
                        if shared.options.watch {
                            refresh = Some(
                                RefreshCoordinator::new(
                                    repository.worktree.clone(),
                                    millis(shared.options.debounce),
                                )
                                .with_temporary_paths(shared.options.temporary_paths.clone()),
                            );
                            let early = watcher.register_before_scan(&repository, shared);
                            let shared = Arc::clone(shared);
                            if early {
                                // Ignored paths only filter this watch's events, so they can
                                // arrive after the first scan without leaving a gap.
                                discovering = Some(discover_exclusions(repository.clone(), shared));
                            } else {
                                opening_watcher = Some(std::thread::spawn(move || {
                                    let mut registration = WatchRegistration::default();
                                    let mut snapshot = (*lock(&shared.state).snapshot).clone();
                                    registration.register(&repository, &shared, &mut snapshot);
                                    (registration, snapshot.watch_error)
                                }));
                            }
                        }
                        // Status and refs can be published before a large worktree's watcher
                        // finishes discovering exclusions and registering directories.
                        publish(shared, snapshot.clone());
                        refreshing = Some(start_refresh(
                            shared,
                            snapshot,
                            true,
                            std::mem::take(&mut status_cache),
                        ));
                        initialized = true;
                    }
                    Err(error) => {
                        snapshot.phase = RepositoryPhase::Failed;
                        snapshot.error = Some(error);
                        publish(shared, snapshot);
                    }
                }
            }
        }
        if refreshing.as_ref().is_some_and(RefreshWork::is_finished) {
            if let Ok((mut snapshot, retained_cache, refreshed_full)) =
                refreshing.take().expect("finished refresh exists").join()
            {
                status_cache = retained_cache;
                snapshot
                    .watch_error
                    .clone_from(&lock(&shared.state).snapshot.watch_error);
                // Reads completed during the scan must not survive its new repository state.
                cache.invalidate(refreshed_full);
                if refreshed_full {
                    super::graph_pages::invalidate_indices(shared);
                }
                if !shared.cancel.load(Ordering::Acquire) {
                    publish(shared, snapshot);
                }
                // A refresh that started after every request answers them all. One the watcher
                // queued behind it must not keep their readers waiting for a second scan.
                if !pending_requested && (!requested || initialized) {
                    block_reads_until_refresh = false;
                    reads::complete_reload(shared);
                }
            } else {
                let mut snapshot = (*lock(&shared.state).snapshot).clone();
                snapshot.phase = RepositoryPhase::Failed;
                snapshot.error = Some(RepositoryError::new(
                    RepositoryErrorKind::Read,
                    "repository refresh worker failed",
                ));
                block_reads_until_refresh = false;
                publish(shared, snapshot);
                reads::complete_reload(shared);
            }
        }
        // The first load already scans for a request that arrived with it.
        let explicit = requested && !initialized;
        let mut full = explicit;
        let mut due = explicit;
        let mut reload_watch = explicit;
        let mut gap = false;
        if opening_watcher
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            if let Ok((registered, watch_error)) = opening_watcher
                .take()
                .expect("finished watcher exists")
                .join()
            {
                watcher = registered;
                let mut snapshot = (*lock(&shared.state).snapshot).clone();
                if record_watch_error(
                    &mut snapshot,
                    watch_error.as_ref().map(|error| error.message.as_str()),
                ) {
                    publish(shared, snapshot);
                }
                // The rescan closes the interval between the first status and native watch.
                due = true;
                gap = true;
                full = true;
            } else {
                let mut snapshot = (*lock(&shared.state).snapshot).clone();
                if record_watch_error(&mut snapshot, Some("watch registration failed")) {
                    publish(shared, snapshot);
                }
                watcher.retry();
            }
        }
        if discovering
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            // A failed discovery keeps the exclusions the watch already filters with.
            if let Ok(Some(plan)) = discovering
                .take()
                .expect("finished exclusion discovery exists")
                .join()
            {
                if !watcher.apply_exclusions(plan.clone()) {
                    let mut snapshot = (*lock(&shared.state).snapshot).clone();
                    let old_error = snapshot.watch_error.clone();
                    watcher.register_plan(plan, shared, &mut snapshot);
                    if old_error != snapshot.watch_error {
                        publish(shared, snapshot);
                    }
                    // Replacing the registration reopens the gap the early watch closed.
                    due = true;
                    gap = true;
                    full = true;
                }
            }
        }
        if let Some(active) = watcher.active.as_mut() {
            match active.wait(Duration::ZERO) {
                Ok(batch) => {
                    // A refresh requested in this pass rescans everything the lost events
                    // covered, as after a mutation that parked this worker.
                    if batch.overflow && !explicit {
                        let revision = lock(&shared.state).snapshot.revision;
                        super::emit(shared, super::RepositoryEvent::WatchOverflow { revision });
                    }
                    if watcher.retry_at.is_none() {
                        watcher.failed_attempts = 0;
                        let mut snapshot = (*lock(&shared.state).snapshot).clone();
                        if record_watch_error(&mut snapshot, None) {
                            publish(shared, snapshot);
                        }
                    }
                    if let Some(refresh) = refresh.as_mut() {
                        refresh.ingest(batch, millis(started.elapsed()));
                        if let Some(intent) = refresh.take_due(millis(started.elapsed())) {
                            due = true;
                            let snapshot = Arc::clone(&lock(&shared.state).snapshot);
                            reload_watch |= intent.full_rescan
                                || snapshot.repository.as_ref().is_some_and(|repository| {
                                    ignore_rules_changed(repository, &intent.paths)
                                });
                            full |= intent.full_rescan
                                || snapshot.repository.as_ref().is_some_and(|repository| {
                                    intent.paths.iter().any(|path| {
                                        // Worktree attributes also control fixed-commit diff/search
                                        // interpretation, so those cached reads must be discarded.
                                        path.file_name()
                                            .is_some_and(|name| name == ".gitattributes")
                                            || path.starts_with(
                                                repository.worktree.git_dir().as_path(),
                                            )
                                            || path.starts_with(
                                                repository
                                                    .worktree
                                                    .repository()
                                                    .common_git_dir()
                                                    .as_path(),
                                            )
                                    })
                                });
                        }
                    }
                }
                Err(error) => {
                    watcher.active = None;
                    watcher.polling_required = shared.options.polling_fallback;
                    watcher.retry();
                    let mut snapshot = (*lock(&shared.state).snapshot).clone();
                    if record_watch_error(&mut snapshot, Some(error.message())) {
                        publish(shared, snapshot);
                    }
                }
            }
        }
        if shared.options.watch
            && opening_watcher.is_none()
            && !shared.cancel.load(Ordering::Acquire)
            && (reload_watch || watcher.retry_due())
        {
            let mut snapshot = (*lock(&shared.state).snapshot).clone();
            if let Some(repository) = snapshot.repository.clone() {
                if watcher.retry_due() {
                    // Recover with the exclusions last known. Discovering them walks the
                    // worktree, which would stall every queued read on this thread.
                    let old_error = snapshot.watch_error.clone();
                    let plan = watcher.plan.clone().unwrap_or_else(|| {
                        watch_plan(&repository, Vec::new(), &shared.options.temporary_paths)
                    });
                    watcher.register_plan(plan, shared, &mut snapshot);
                    if watcher.retry_at.is_none() {
                        due = true;
                        gap = true;
                        full = true;
                    }
                    if old_error != snapshot.watch_error {
                        publish(shared, snapshot);
                    }
                }
                rediscover = true;
            }
        }
        if rediscover && discovering.is_none() && !shared.cancel.load(Ordering::Acquire) {
            let repository = lock(&shared.state).snapshot.repository.clone();
            if let Some(repository) = repository {
                rediscover = false;
                discovering = Some(discover_exclusions(repository, Arc::clone(shared)));
            }
        }
        pending_refresh |= due;
        pending_requested |= explicit;
        pending_gap |= gap;
        pending_full |= full;
        // Watcher events can make the next scan due as soon as the last one ends. Answer the
        // interactive reads that waited for that scan before starting another, so a busy
        // worktree cannot starve them. A requested refresh or a registration-gap rescan
        // keeps reads behind it.
        if pending_refresh
            && refreshing.is_none()
            && !pending_requested
            && !pending_gap
            && !block_reads_until_refresh
            && !shared.cancel.load(Ordering::Acquire)
        {
            let mut answered = 0;
            while answered < READS_BETWEEN_SCANS
                && reads::has_interactive(&lock(&shared.state))
                && cache.process_one(
                    shared,
                    &mut WorkerExecutor(shared),
                    reads::ReadAdmission::All,
                )
            {
                answered += 1;
            }
        }
        if pending_refresh && refreshing.is_none() && !shared.cancel.load(Ordering::Acquire) {
            let full = std::mem::take(&mut pending_full);
            pending_refresh = false;
            pending_requested = false;
            pending_gap = false;
            cache.invalidate(full);
            if full {
                super::graph_pages::invalidate_indices(shared);
            }
            let snapshot = (*lock(&shared.state).snapshot).clone();
            if snapshot.repository.is_some() {
                refreshing = Some(start_refresh(
                    shared,
                    snapshot,
                    full,
                    std::mem::take(&mut status_cache),
                ));
            }
        }
        let admission = if refreshing.is_none() {
            Some(reads::ReadAdmission::All)
        } else {
            match shared.options.refresh_read_policy {
                super::RefreshReadPolicy::AfterSnapshot => None,
                super::RefreshReadPolicy::ConcurrentImmutable => {
                    Some(reads::ReadAdmission::Immutable)
                }
                super::RefreshReadPolicy::ConcurrentScoped => Some(reads::ReadAdmission::Scoped),
            }
        };
        let processed = !block_reads_until_refresh
            && admission.is_some_and(|admission| {
                cache.process_one(shared, &mut WorkerExecutor(shared), admission)
            });
        drop(activity);
        let busy = refreshing.is_some()
            || pending_refresh
            || opening_watcher.is_some()
            || discovering.is_some()
            || refresh
                .as_ref()
                .is_some_and(RefreshCoordinator::has_pending);
        let mut state = lock(&shared.state);
        if state.worker_busy != busy {
            state.worker_busy = busy;
            shared.changed.notify_all();
        }
        if !state.load_pending
            && !state.refresh_pending
            && (!reads::has_pending(&state) || !processed)
            && !shared.cancel.load(Ordering::Acquire)
        {
            drop(
                shared
                    .changed
                    .wait_timeout(state, Duration::from_millis(25))
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
        }
    }
    if let Some(refreshing) = refreshing {
        let _ = refreshing.join();
    }
    if let Some(opening) = opening_watcher {
        let _ = opening.join();
    }
    if let Some(discovering) = discovering {
        let _ = discovering.join();
    }
    drop(watcher);
    reads::cancel_pending(shared);
    let mut snapshot = (*lock(&shared.state).snapshot).clone();
    snapshot.phase = RepositoryPhase::Closed;
    publish(shared, snapshot);
}

/// Interactive reads answered between two watcher-triggered scans.
const READS_BETWEEN_SCANS: usize = 8;

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn execute(shared: &Shared, command: GitProcessCommand) -> PrivateActivityResult {
    execute_observing(shared, command, None, None)
}

/// Runs a command with the repository's activity routing outside the worker thread.
pub(super) fn execute_with_cancellation(
    shared: &Shared,
    command: GitProcessCommand,
    cancellation: Option<&dyn crate::cancellation::Cancellation>,
) -> PrivateActivityResult {
    execute_observing(shared, command, None, cancellation)
}

struct WorkerExecutor<'a>(&'a Shared);
impl crate::loading::CommandExecutor for WorkerExecutor<'_> {
    fn execute(&mut self, command: GitProcessCommand) -> PrivateActivityResult {
        execute(self.0, command)
    }
    fn execute_observing(
        &mut self,
        command: GitProcessCommand,
        observer: crate::process::PrivateStdoutObserver<'_>,
        cancellation: Option<&dyn crate::cancellation::Cancellation>,
    ) -> PrivateActivityResult {
        execute_observing(self.0, command, observer, cancellation)
    }
}

struct CombinedCancellation<'a>(
    &'a Shared,
    Option<&'a dyn crate::cancellation::Cancellation>,
);
impl crate::cancellation::Cancellation for CombinedCancellation<'_> {
    fn is_cancelled(&self) -> bool {
        self.0.cancel.load(Ordering::Acquire)
            || self
                .1
                .is_some_and(crate::cancellation::Cancellation::is_cancelled)
    }
}

fn execute_observing(
    shared: &Shared,
    command: GitProcessCommand,
    observer: crate::process::PrivateStdoutObserver<'_>,
    cancellation: Option<&dyn crate::cancellation::Cancellation>,
) -> PrivateActivityResult {
    let id = shared.options.activity_id.as_ref().map_or_else(
        || {
            ActivityId::new(NEXT_ACTIVITY.fetch_add(1, Ordering::Relaxed))
                .expect("activity counter exhausted")
        },
        |allocate| allocate(),
    );
    let mut sink = |event| {
        if let Some(sink) = &shared.options.activity_sink {
            sink(event);
        }
    };
    let canceled = CombinedCancellation(shared, cancellation);
    if let Some(observer) = observer {
        ActivityGitRunner::run_private_observing_stdout(
            command,
            id,
            Some(&canceled),
            &mut sink,
            observer,
        )
    } else {
        ActivityGitRunner::run_private(command, id, Some(&canceled), &mut sink)
    }
}

fn discover(
    path: &std::path::Path,
    shared: &Shared,
) -> Result<DiscoveredRepository, RepositoryError> {
    let path = RepositoryPath::new(path).map_err(|_| {
        RepositoryError::new(RepositoryErrorKind::Discovery, "invalid repository path")
    })?;
    let request = DiscoveryRequest {
        path,
        discover_parents: shared.options.discover_parents,
    };
    RepositoryDiscovery::new(shared.options.git_executable.clone())
        .discover(&request, &mut |command| execute(shared, command).activity)
        .map_err(|error| RepositoryError::new(RepositoryErrorKind::Discovery, error.message()))
}

#[expect(
    clippy::too_many_lines,
    reason = "publishes status, refs and HEAD together or retains the previous coherent snapshot"
)]
fn refresh_snapshot(
    shared: &Shared,
    mut snapshot: RepositorySnapshot,
    full: bool,
    status_cache: &mut StatusCache,
) -> RepositorySnapshot {
    let Some(repository) = snapshot.repository.clone() else {
        return snapshot;
    };
    let repository = &repository;
    // The first load has nothing coherent on screen yet, so its references are published as
    // soon as they are read; a worktree scan can take far longer than the reference query.
    let preview_refs = full
        && snapshot.phase == RepositoryPhase::Loading
        && repository.kind != RepositoryKind::Bare;
    let options = StatusOptions {
        bare: repository.kind == RepositoryKind::Bare,
        ..StatusOptions::default()
    };
    let ((status, status_activities), refs) = std::thread::scope(|scope| {
        let status_work = scope.spawn(|| {
            let mut status_activities = Vec::new();
            let mut executor = |command| {
                let result = execute(shared, command);
                status_activities.push(result.activity.clone());
                result
            };
            let status = status_cache.load(
                &shared.options.git_executable,
                repository,
                options,
                shared.options.index_refresh == super::IndexRefreshPolicy::InstallOnce,
                &mut executor,
            );
            (status, status_activities)
        });
        let refs = full.then(|| {
            let result = query_refs(shared, repository);
            if preview_refs && result.failure.is_none() && !shared.cancel.load(Ordering::Acquire) {
                let mut preview = snapshot.clone();
                preview.refs = Arc::new(result.updates.clone());
                preview.refs_activities = Arc::new(result.activities.clone());
                publish(shared, preview);
            }
            result
        });
        let status = status_work
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        (status, refs)
    });
    snapshot.status_activities = Arc::new(status_activities);
    snapshot.status_failure = status.as_ref().err().copied();
    let old_status = snapshot.status.clone();
    let old_refs = Arc::clone(&snapshot.refs);
    let mut bare_head = None;
    match status {
        Ok(status) => {
            snapshot.status = status.map(Arc::new);
            snapshot.error = None;
        }
        Err(error) => {
            snapshot.error = Some(RepositoryError::new(
                RepositoryErrorKind::Read,
                format!("status read failed: {error:?}"),
            ));
        }
    }
    if let Some(mut result) = refs {
        snapshot.refs_failure = result.failure;
        if let Some(failure) = result.failure {
            snapshot.error = Some(RepositoryError::new(
                RepositoryErrorKind::Read,
                format!("reference read failed: {failure:?}"),
            ));
        } else {
            if repository.kind == RepositoryKind::Bare {
                match current_bare_head(
                    shared,
                    &repository.worktree,
                    &result.updates,
                    &mut result.activities,
                ) {
                    Ok(head) => bare_head = Some(head),
                    Err(error) => {
                        use crate::loading::refs::RefsFailureKind;
                        snapshot.refs_failure = Some(match error.kind {
                            RepositoryErrorKind::Canceled => RefsFailureKind::Canceled,
                            RepositoryErrorKind::InvalidOutput => RefsFailureKind::InvalidOutput,
                            RepositoryErrorKind::MissingGit => RefsFailureKind::MissingGit,
                            _ => RefsFailureKind::GitFailed,
                        });
                        snapshot.error = Some(error);
                    }
                }
            }
            snapshot.refs = Arc::new(result.updates);
        }
        snapshot.refs_activities = Arc::new(result.activities);
    }
    if shared.cancel.load(Ordering::Acquire) {
        return snapshot;
    }
    snapshot.phase = if snapshot.error.is_some() {
        snapshot.status = old_status;
        snapshot.refs = old_refs;
        RepositoryPhase::Failed
    } else {
        if let (Some(repository), Some(head)) = (snapshot.repository.as_mut(), bare_head) {
            repository.head = head;
        }
        if let (Some(repository), Some(status)) =
            (snapshot.repository.as_mut(), snapshot.status.as_ref())
        {
            if let Some(head) = current_head(&status.branch) {
                repository.head = head;
            }
        }
        RepositoryPhase::Ready
    };
    snapshot
}

fn query_refs(
    shared: &Shared,
    repository: &DiscoveredRepository,
) -> crate::loading::refs::RefsResult {
    let mut executor = |command| execute(shared, command);
    let request = RefsRequest {
        worktree: repository.worktree.clone(),
        include_secondary: true,
        head_symbolic_hint: match &repository.head {
            crate::types::HeadState::Unborn { branch } => Some(branch.clone()),
            crate::types::HeadState::Attached { .. } | crate::types::HeadState::Detached { .. } => {
                None
            }
        },
    };
    let loader = ReferenceLoader::new(shared.options.git_executable.clone());
    if repository.kind == RepositoryKind::Bare {
        loader.query_bare(&request, &mut executor)
    } else {
        loader.query(&request, &mut executor)
    }
}

type RefreshResult = (RepositorySnapshot, StatusCache, bool);

enum RefreshWork {
    Inline(Box<RefreshResult>),
    Background(std::thread::JoinHandle<RefreshResult>),
}

impl RefreshWork {
    fn is_finished(&self) -> bool {
        match self {
            Self::Inline(_) => true,
            Self::Background(worker) => worker.is_finished(),
        }
    }

    fn join(self) -> std::thread::Result<RefreshResult> {
        match self {
            Self::Inline(result) => Ok(*result),
            Self::Background(worker) => worker.join(),
        }
    }
}

fn start_refresh(
    shared: &Arc<Shared>,
    snapshot: RepositorySnapshot,
    full: bool,
    mut status_cache: StatusCache,
) -> RefreshWork {
    let invalidate_full = full && snapshot.phase != RepositoryPhase::Loading;
    if shared.options.refresh_read_policy == super::RefreshReadPolicy::AfterSnapshot {
        let snapshot = refresh_snapshot(shared, snapshot, full, &mut status_cache);
        return RefreshWork::Inline(Box::new((snapshot, status_cache, invalidate_full)));
    }
    let activity = super::mutation::fork_worker(shared);
    let shared = Arc::clone(shared);
    RefreshWork::Background(std::thread::spawn(move || {
        let snapshot = refresh_snapshot(&shared, snapshot, full, &mut status_cache);
        drop(activity);
        shared.changed.notify_all();
        (snapshot, status_cache, invalidate_full)
    }))
}

fn current_bare_head(
    shared: &Shared,
    worktree: &crate::types::WorktreeId,
    updates: &[crate::loading::refs::RefsSectionData],
    activities: &mut Vec<crate::activity::ActivityRecord>,
) -> Result<crate::types::HeadState, RepositoryError> {
    use crate::loading::refs::RefsSectionData;
    use crate::types::{HeadState, ObjectId};
    if let Some(branch) = updates.iter().find_map(|update| match update {
        RefsSectionData::Branches { head_symbolic, .. } => head_symbolic.clone(),
        _ => None,
    }) {
        let target = updates
            .iter()
            .flat_map(|update| match update {
                RefsSectionData::Branches { rows, .. } | RefsSectionData::Tags(rows) => {
                    rows.as_slice()
                }
                RefsSectionData::Remotes { references, .. } => references.as_slice(),
                _ => &[],
            })
            .find(|reference| reference.id == branch)
            .map(|reference| reference.target.clone());
        return Ok(match target {
            Some(target) => HeadState::Attached { branch, target },
            None => HeadState::Unborn { branch },
        });
    }
    let result = execute(
        shared,
        GitProcessCommand::git(shared.options.git_executable.clone())
            .current_dir(worktree.worktree_root().as_path())
            .args(["rev-parse", "--verify", "HEAD"].map(crate::process::ProcessArgument::visible))
            .output_limit(128)
            .redact_output(),
    );
    let id = result.activity.id;
    let status = result.activity.status.clone();
    activities.push(result.activity);
    let failure = |kind, message| RepositoryError::new(kind, message).with_activity(Some(id));
    if !matches!(status, crate::activity::ActivityStatus::Succeeded { .. }) {
        let kind = if matches!(status, crate::activity::ActivityStatus::Canceled { .. }) {
            RepositoryErrorKind::Canceled
        } else {
            RepositoryErrorKind::Read
        };
        return Err(failure(kind, "Git could not resolve detached bare HEAD"));
    }
    let target = (!result.stdout.truncated)
        .then(|| {
            std::str::from_utf8(&result.stdout.bytes)
                .ok()
                .and_then(|value| ObjectId::parse(value.trim()).ok())
        })
        .flatten()
        .ok_or_else(|| {
            failure(
                RepositoryErrorKind::InvalidOutput,
                "Git returned invalid detached bare HEAD",
            )
        })?;
    Ok(HeadState::Detached { target })
}

fn current_head(branch: &crate::status::BranchStatus) -> Option<crate::types::HeadState> {
    use crate::status::BranchHead;
    use crate::types::{HeadState, RefId};
    let reference = |name: &crate::status::StatusRefName| {
        let mut bytes = b"refs/heads/".to_vec();
        bytes.extend_from_slice(name.as_bytes());
        RefId::from_bytes(bytes).ok()
    };
    match &branch.head {
        BranchHead::Attached(name) => Some(HeadState::Attached {
            branch: reference(name)?,
            target: branch.oid.clone()?,
        }),
        BranchHead::Detached => Some(HeadState::Detached {
            target: branch.oid.clone()?,
        }),
        BranchHead::Unborn { branch } => Some(HeadState::Unborn {
            branch: reference(branch)?,
        }),
    }
}

#[derive(Default)]
struct WatchRegistration {
    active: Option<Box<dyn RepositoryWatcherPort>>,
    plan: Option<RepositoryWatchPlan>,
    retry_at: Option<Instant>,
    failed_attempts: u32,
    polling_required: bool,
}
impl WatchRegistration {
    fn retry(&mut self) {
        let delay = Duration::from_secs(1_u64 << self.failed_attempts.min(5));
        self.failed_attempts = self.failed_attempts.saturating_add(1);
        self.retry_at = Some(Instant::now() + delay);
    }
    fn retry_due(&self) -> bool {
        self.retry_at
            .is_some_and(|deadline| Instant::now() >= deadline)
    }
    /// Starts a recursive watch without exclusions before the first scan. Returns `false` when
    /// the backend would walk the worktree, which keeps registration off the opening path.
    fn register_before_scan(&mut self, repository: &DiscoveredRepository, shared: &Shared) -> bool {
        let native = NativeWatcherFactory::native_primary();
        let factory: &dyn PlatformWatcherPort =
            shared.options.watcher_factory.as_deref().unwrap_or(&native);
        if self.polling_required || !factory.covers_recursively() {
            return false;
        }
        let plan = watch_plan(repository, Vec::new(), &shared.options.temporary_paths);
        match factory.watch(&plan) {
            Ok(active) => {
                self.active = Some(active);
                self.plan = Some(plan);
                self.retry_at = None;
                true
            }
            Err(_) => false,
        }
    }
    /// Filters the active watch with `plan`'s exclusions. Returns `false` when it must be replaced.
    fn apply_exclusions(&mut self, plan: RepositoryWatchPlan) -> bool {
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        if self.plan.as_ref() == Some(&plan) {
            return true;
        }
        if !active.update_exclusions(&plan.excluded_worktree_roots) {
            return false;
        }
        self.plan = Some(plan);
        true
    }
    fn register(
        &mut self,
        repository: &DiscoveredRepository,
        shared: &Shared,
        snapshot: &mut RepositorySnapshot,
    ) {
        if shared.cancel.load(Ordering::Acquire) {
            return;
        }
        let exclusions = load_exclusions(repository, shared);
        if shared.cancel.load(Ordering::Acquire) {
            return;
        }
        // Without an answer from Git the exclusions last known still apply.
        let plan = match (exclusions, self.plan.clone()) {
            (Some(exclusions), _) => {
                watch_plan(repository, exclusions, &shared.options.temporary_paths)
            }
            (None, Some(plan)) => plan,
            (None, None) => watch_plan(repository, Vec::new(), &shared.options.temporary_paths),
        };
        self.register_plan(plan, shared, snapshot);
    }
    fn register_plan(
        &mut self,
        plan: RepositoryWatchPlan,
        shared: &Shared,
        snapshot: &mut RepositorySnapshot,
    ) {
        if shared.cancel.load(Ordering::Acquire) {
            return;
        }
        if self.active.is_some() && self.retry_at.is_none() && self.plan.as_ref() == Some(&plan) {
            return;
        }
        let native = if self.polling_required && shared.options.polling_fallback {
            PollingWatcherFactory::bounded_fallback().watch(&plan)
        } else {
            shared.options.watcher_factory.as_ref().map_or_else(
                || NativeWatcherFactory::native_primary().watch(&plan),
                |factory| factory.watch(&plan),
            )
        };
        let result = if native.is_err()
            && !self.polling_required
            && shared.options.polling_fallback
            && !shared.cancel.load(Ordering::Acquire)
        {
            PollingWatcherFactory::bounded_fallback().watch(&plan)
        } else {
            native
        };
        match result {
            Ok(active) => {
                self.active = Some(active);
                self.plan = Some(plan);
                self.retry_at = None;
            }
            Err(error) => {
                self.retry();
                record_watch_error(snapshot, Some(error.message()));
            }
        }
    }
}
/// Reads the ignored roots, or `None` when Git could not list them.
fn load_exclusions(
    repository: &DiscoveredRepository,
    shared: &Shared,
) -> Option<Vec<crate::types::RepositoryRelativePath>> {
    if repository.kind == RepositoryKind::Bare {
        return Some(Vec::new());
    }
    crate::watch::ignore::load(
        &shared.options.git_executable,
        &repository.worktree,
        &mut |command| execute(shared, command),
    )
    .ok()
}
/// Lists ignored roots off the worker thread, which keeps serving reads while Git walks the
/// worktree.
fn discover_exclusions(
    repository: DiscoveredRepository,
    shared: Arc<Shared>,
) -> std::thread::JoinHandle<Option<RepositoryWatchPlan>> {
    std::thread::spawn(move || {
        let plan = load_exclusions(&repository, &shared)
            .map(|exclusions| watch_plan(&repository, exclusions, &shared.options.temporary_paths));
        shared.changed.notify_all();
        plan
    })
}
fn record_watch_error(snapshot: &mut RepositorySnapshot, message: Option<&str>) -> bool {
    let error = message.map(|message| RepositoryError::new(RepositoryErrorKind::Watch, message));
    if snapshot.watch_error == error {
        return false;
    }
    snapshot.watch_error = error;
    true
}
fn watch_plan(
    repository: &DiscoveredRepository,
    exclusions: Vec<crate::types::RepositoryRelativePath>,
    temporary_paths: &crate::watch::TemporaryPathRules,
) -> RepositoryWatchPlan {
    let mut plan = RepositoryWatchPlan::for_worktree(repository.worktree.clone())
        .excluding_worktree_paths(exclusions)
        .with_temporary_paths(temporary_paths.clone());
    if repository.kind == RepositoryKind::Bare {
        plan.targets
            .retain(|target| target.scope != crate::watch::WatchScope::Worktree);
    }
    plan
}
fn ignore_rules_changed(repository: &DiscoveredRepository, paths: &[std::path::PathBuf]) -> bool {
    let git = repository.worktree.git_dir().as_path();
    let common = repository.worktree.repository().common_git_dir().as_path();
    paths.iter().any(|path| {
        path.file_name().is_some_and(|name| name == ".gitignore")
            || [git, common].iter().any(|root| {
                path == *root
                    || path == &root.join("info")
                    || ["info/exclude", "config", "config.worktree"]
                        .iter()
                        .any(|name| path == &root.join(name))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Repository, RepositoryOptions};
    use crate::types::{HeadState, RepositoryId, WorktreeId};
    use crate::version::GitVersion;
    use crate::watch::{WatchBatch, WatchError, WatchErrorKind, WatchScope};
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize},
        Condvar, Mutex,
    };

    struct CapturingFactory {
        plans: Mutex<Vec<RepositoryWatchPlan>>,
        waits: Arc<AtomicUsize>,
        fail: bool,
    }
    struct FakeWatcher {
        waits: Arc<AtomicUsize>,
        fail: bool,
    }
    struct RecoveringFactory {
        registrations: AtomicUsize,
        waits: Arc<AtomicUsize>,
    }
    struct BlockingFactory {
        entered: AtomicBool,
        gate: Arc<(Mutex<bool>, Condvar)>,
    }
    impl RepositoryWatcherPort for FakeWatcher {
        fn wait(&mut self, _: Duration) -> Result<WatchBatch, WatchError> {
            self.waits.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                Err(WatchError::new(WatchErrorKind::Io, "watch disconnected"))
            } else {
                Ok(WatchBatch::default())
            }
        }
    }
    impl PlatformWatcherPort for CapturingFactory {
        fn watch(
            &self,
            plan: &RepositoryWatchPlan,
        ) -> Result<Box<dyn RepositoryWatcherPort>, WatchError> {
            self.plans.lock().unwrap().push(plan.clone());
            Ok(Box::new(FakeWatcher {
                waits: Arc::clone(&self.waits),
                fail: self.fail,
            }))
        }
    }
    impl PlatformWatcherPort for RecoveringFactory {
        fn watch(
            &self,
            _: &RepositoryWatchPlan,
        ) -> Result<Box<dyn RepositoryWatcherPort>, WatchError> {
            let registration = self.registrations.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(FakeWatcher {
                waits: Arc::clone(&self.waits),
                fail: registration == 0,
            }))
        }
    }
    impl PlatformWatcherPort for BlockingFactory {
        fn watch(
            &self,
            _: &RepositoryWatchPlan,
        ) -> Result<Box<dyn RepositoryWatcherPort>, WatchError> {
            self.entered.store(true, Ordering::Release);
            let (lock, changed) = &*self.gate;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = changed.wait(released).unwrap();
            }
            Ok(Box::new(FakeWatcher {
                waits: Arc::new(AtomicUsize::new(0)),
                fail: false,
            }))
        }
    }
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let path = std::env::temp_dir().join(format!(
                "devmil-watch-lifecycle-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            let result = std::process::Command::new("git")
                .args(["init", "-b", "main"])
                .current_dir(&path)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            Self(path)
        }
        fn discovered(&self) -> DiscoveredRepository {
            let root = RepositoryPath::new(&self.0).unwrap();
            let git = RepositoryPath::new(self.0.join(".git")).unwrap();
            DiscoveredRepository {
                worktree: WorktreeId::new(RepositoryId::new(git.clone()), root, git),
                kind: RepositoryKind::Worktree,
                head: HeadState::Unborn {
                    branch: crate::types::RefId::from_bytes(b"refs/heads/main".to_vec()).unwrap(),
                },
                git_version: GitVersion::new(2, 40, 0),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn factory(fail: bool) -> Arc<CapturingFactory> {
        Arc::new(CapturingFactory {
            plans: Mutex::new(Vec::new()),
            waits: Arc::new(AtomicUsize::new(0)),
            fail,
        })
    }

    #[test]
    fn bare_head_refreshes_attached_detached_and_unborn_states() {
        let fixture = Fixture::new();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(&fixture.0)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["commit", "--allow-empty", "-m", "base"]);
        let first = git(&["rev-parse", "HEAD"]);
        git(&["switch", "-c", "feature"]);
        git(&["commit", "--allow-empty", "-m", "feature"]);
        let second = git(&["rev-parse", "HEAD"]);
        git(&["switch", "main"]);
        git(&["clone", "--bare", ".", "bare"]);
        let repository = Repository::new(
            fixture.0.join("bare"),
            RepositoryOptions {
                watch: false,
                ..RepositoryOptions::default()
            },
        )
        .unwrap();
        repository.load().unwrap();
        let expect = |expected: HeadState| {
            let previous = repository.snapshot().revision;
            repository.refresh().unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let snapshot = repository.snapshot();
                if snapshot.revision > previous
                    && snapshot.phase == RepositoryPhase::Ready
                    && snapshot
                        .repository
                        .as_ref()
                        .is_some_and(|repository| repository.head == expected)
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "bare HEAD did not refresh: {snapshot:?}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let branch = |name: &str| crate::types::RefId::parse(format!("refs/heads/{name}")).unwrap();
        expect(HeadState::Attached {
            branch: branch("main"),
            target: crate::types::ObjectId::parse(first).unwrap(),
        });
        git(&["-C", "bare", "symbolic-ref", "HEAD", "refs/heads/feature"]);
        expect(HeadState::Attached {
            branch: branch("feature"),
            target: crate::types::ObjectId::parse(&second).unwrap(),
        });
        git(&["-C", "bare", "update-ref", "--no-deref", "HEAD", &second]);
        expect(HeadState::Detached {
            target: crate::types::ObjectId::parse(second).unwrap(),
        });
        git(&["-C", "bare", "symbolic-ref", "HEAD", "refs/heads/missing"]);
        expect(HeadState::Unborn {
            branch: branch("missing"),
        });
        repository.close();
    }

    #[test]
    fn disconnected_watcher_is_dropped_and_failure_is_published_once() {
        let fixture = Fixture::new();
        let factory = factory(true);
        let repository = Repository::new(
            &fixture.0,
            RepositoryOptions {
                watcher_factory: Some(factory.clone()),
                polling_fallback: false,
                ..RepositoryOptions::default()
            },
        )
        .unwrap();
        repository.load().unwrap();
        // The first load and the rescan that follows registration publish on their own
        // schedule, later under load. A failure published again and again never goes quiet.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut revision = repository.snapshot().revision;
        let mut unchanged_since = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(10));
            let snapshot = repository.snapshot();
            if snapshot.revision != revision {
                revision = snapshot.revision;
                unchanged_since = Instant::now();
            } else if snapshot.watch_error.is_some()
                && snapshot.phase == RepositoryPhase::Ready
                && unchanged_since.elapsed() >= Duration::from_millis(300)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "snapshots kept changing after the watcher failed: {snapshot:?}"
            );
        }
        assert_eq!(factory.waits.load(Ordering::Relaxed), 1);
        repository.close();
    }

    #[test]
    fn opening_watcher_does_not_block_history_and_rescans_registration_gap() {
        use crate::loading::graph::{GraphHistoryOrder, GraphTraversalScope};
        let fixture = Fixture::new();
        let commit = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--allow-empty",
                "--quiet",
                "-m",
                "base",
            ])
            .current_dir(&fixture.0)
            .status()
            .unwrap();
        assert!(commit.success());
        let factory = Arc::new(BlockingFactory {
            entered: AtomicBool::new(false),
            gate: Arc::new((Mutex::new(false), Condvar::new())),
        });
        let repository = Repository::new(
            &fixture.0,
            RepositoryOptions {
                watcher_factory: Some(factory.clone()),
                polling_fallback: false,
                ..RepositoryOptions::default()
            },
        )
        .unwrap();
        repository.load().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while (repository.snapshot().phase != RepositoryPhase::Ready
            || !factory.entered.load(Ordering::Acquire))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(repository.snapshot().phase, RepositoryPhase::Ready);
        assert!(factory.entered.load(Ordering::Acquire));
        let initial_revision = repository.snapshot().revision;
        std::fs::write(fixture.0.join("arrived-during-watch-setup"), b"new").unwrap();
        let history = repository
            .history_prefix(
                GraphHistoryOrder::Topological,
                GraphTraversalScope::AllReferences,
                512,
            )
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .expect("history read must finish while watcher registration is blocked")
            .unwrap();
        assert_eq!(history.rows.len(), 1);
        {
            let (lock, changed) = &*factory.gate;
            *lock.lock().unwrap() = true;
            changed.notify_all();
        }
        while Instant::now() < deadline {
            let snapshot = repository.snapshot();
            if snapshot.revision > initial_revision
                && snapshot
                    .status
                    .as_ref()
                    .is_some_and(|status| !status.entries.is_empty())
            {
                repository.close();
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("registration gap did not converge after watcher startup");
    }

    #[test]
    fn disconnected_watcher_retries_and_clears_its_failure_after_recovery() {
        let fixture = Fixture::new();
        let factory = Arc::new(RecoveringFactory {
            registrations: AtomicUsize::new(0),
            waits: Arc::new(AtomicUsize::new(0)),
        });
        let repository = Repository::new(
            &fixture.0,
            RepositoryOptions {
                watcher_factory: Some(factory.clone()),
                polling_fallback: false,
                ..RepositoryOptions::default()
            },
        )
        .unwrap();
        repository.load().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let failure_revision = loop {
            let snapshot = repository.snapshot();
            if snapshot.watch_error.is_some() {
                break snapshot.revision;
            }
            assert!(Instant::now() < deadline, "watch failure was not published");
            std::thread::sleep(Duration::from_millis(10));
        };
        loop {
            let snapshot = repository.snapshot();
            if factory.registrations.load(Ordering::Relaxed) >= 2
                && snapshot.revision > failure_revision
                && snapshot.watch_error.is_none()
            {
                break;
            }
            assert!(Instant::now() < deadline, "watch retry did not recover");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(factory.waits.load(Ordering::Relaxed) >= 2);
        repository.close();
    }
    #[test]
    fn failed_ignore_discovery_keeps_the_exclusions_in_use() {
        let fixture = Fixture::new();
        let factory = factory(false);
        std::fs::create_dir_all(fixture.0.join("target")).unwrap();
        std::fs::write(fixture.0.join("target/file"), b"ignored").unwrap();
        std::fs::write(fixture.0.join(".gitignore"), b"/target/\n").unwrap();
        let options = |git_executable: &str| RepositoryOptions {
            git_executable: git_executable.into(),
            watcher_factory: Some(factory.clone()),
            polling_fallback: false,
            ..RepositoryOptions::default()
        };
        let repository = Repository::new(&fixture.0, options("git")).unwrap();
        let unreachable =
            Repository::new(&fixture.0, options("devmil-git-missing-executable")).unwrap();
        let discovered = fixture.discovered();
        let mut registration = WatchRegistration::default();
        let mut snapshot = RepositorySnapshot::default();
        registration.register(&discovered, &repository.shared, &mut snapshot);
        registration.register(&discovered, &unreachable.shared, &mut snapshot);
        assert_eq!(
            factory.plans.lock().unwrap().len(),
            1,
            "an unanswered discovery must not replace the watch with an unfiltered one"
        );
        assert_eq!(
            registration.plan.unwrap().excluded_worktree_roots,
            vec![fixture.0.join("target")]
        );
    }

    #[test]
    fn registration_reloads_ignore_exclusions_and_bare_plan_excludes_worktree() {
        let fixture = Fixture::new();
        let factory = factory(false);
        std::fs::create_dir_all(fixture.0.join("target")).unwrap();
        std::fs::write(fixture.0.join("target/file"), b"ignored").unwrap();
        std::fs::write(fixture.0.join(".gitignore"), b"/target/\n").unwrap();
        let repository = Repository::new(
            &fixture.0,
            RepositoryOptions {
                watcher_factory: Some(factory.clone()),
                polling_fallback: false,
                ..RepositoryOptions::default()
            },
        )
        .unwrap();
        let mut discovered = fixture.discovered();
        let mut registration = WatchRegistration::default();
        let mut snapshot = RepositorySnapshot::default();
        registration.register(&discovered, &repository.shared, &mut snapshot);
        assert_eq!(
            factory.plans.lock().unwrap()[0].excluded_worktree_roots,
            vec![fixture.0.join("target")]
        );
        registration.register(&discovered, &repository.shared, &mut snapshot);
        assert_eq!(
            factory.plans.lock().unwrap().len(),
            1,
            "unchanged ignore roots must retain their active watcher"
        );
        std::fs::write(fixture.0.join(".gitignore"), b"").unwrap();
        assert!(ignore_rules_changed(
            &discovered,
            &[fixture.0.join(".gitignore")]
        ));
        assert!(ignore_rules_changed(
            &discovered,
            &[fixture.0.join(".git/info/exclude")]
        ));
        assert!(ignore_rules_changed(
            &discovered,
            &[fixture.0.join(".git/config")]
        ));
        registration.register(&discovered, &repository.shared, &mut snapshot);
        assert!(factory.plans.lock().unwrap()[1]
            .excluded_worktree_roots
            .is_empty());
        discovered.kind = RepositoryKind::Bare;
        let plan = watch_plan(
            &discovered,
            Vec::new(),
            &crate::watch::TemporaryPathRules::default(),
        );
        assert!(plan
            .targets
            .iter()
            .all(|target| target.scope != WatchScope::Worktree));
        assert!(plan
            .targets
            .iter()
            .all(|target| !target.recursive || target.path != fixture.0.join(".git")));
    }
}
