//! Platform file-watching ports and deterministic refresh coalescing.

use crate::types::{RepositoryRelativePath, WorktreeId};
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MAX_COALESCED_PATHS: usize = 1024;
pub const MAX_WATCH_EXCLUSIONS: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WatchChangeKind {
    Created,
    Modified,
    Renamed,
    Removed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WatchChange {
    pub path: PathBuf,
    pub kind: WatchChangeKind,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WatchBatch {
    pub changes: Vec<WatchChange>,
    pub overflow: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WatchScope {
    Worktree,
    Index,
    Head,
    Refs,
    PackedRefs,
    Config,
    Sequencer,
    CommonGitDirectory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WatchTarget {
    pub path: PathBuf,
    pub recursive: bool,
    pub scope: WatchScope,
}

/// File names an application writes temporarily inside a repository and that must not trigger a
/// refresh. A path matches when any of its components starts with a prefix or ends with a suffix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemporaryPathRules {
    pub prefixes: Vec<String>,
    pub suffixes: Vec<String>,
}

impl Default for TemporaryPathRules {
    /// The historical rules: `.qspline-*`, `qspline-tmp-*` and `*.qspline-tmp`.
    fn default() -> Self {
        Self {
            prefixes: vec![".qspline-".into(), "qspline-tmp-".into()],
            suffixes: vec![".qspline-tmp".into()],
        }
    }
}

impl TemporaryPathRules {
    /// Rules that ignore no path.
    #[must_use]
    pub fn none() -> Self {
        Self {
            prefixes: Vec::new(),
            suffixes: Vec::new(),
        }
    }

    #[must_use]
    pub fn matches(&self, path: &Path) -> bool {
        if self.prefixes.is_empty() && self.suffixes.is_empty() {
            return false;
        }
        path.components().any(|component| {
            let name = component.as_os_str().to_string_lossy();
            self.prefixes
                .iter()
                .any(|prefix| name.starts_with(prefix.as_str()))
                || self
                    .suffixes
                    .iter()
                    .any(|suffix| name.ends_with(suffix.as_str()))
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryWatchPlan {
    pub worktree: WorktreeId,
    pub targets: Vec<WatchTarget>,
    /// Absolute worktree paths Git classifies as ignored. Metadata targets are never excluded.
    pub excluded_worktree_roots: Vec<PathBuf>,
    /// Application temporary files the watcher drops before reporting a change.
    pub temporary_paths: TemporaryPathRules,
}

impl RepositoryWatchPlan {
    pub fn for_worktree(worktree: WorktreeId) -> Self {
        let root = worktree.worktree_root().as_path();
        let git = worktree.git_dir().as_path();
        let common = worktree.repository().common_git_dir().as_path();
        let mut targets = vec![target(root, true, WatchScope::Worktree)];
        add_git_targets(&mut targets, git);
        add_common_targets(&mut targets, common);
        Self {
            worktree,
            targets,
            excluded_worktree_roots: Vec::new(),
            temporary_paths: TemporaryPathRules::default(),
        }
    }

    #[must_use]
    pub fn with_temporary_paths(mut self, rules: TemporaryPathRules) -> Self {
        self.temporary_paths = rules;
        self
    }

    #[must_use]
    pub fn excluding_worktree_paths(
        mut self,
        paths: impl IntoIterator<Item = RepositoryRelativePath>,
    ) -> Self {
        let root = self.worktree.worktree_root().as_path();
        self.excluded_worktree_roots = paths
            .into_iter()
            .take(MAX_WATCH_EXCLUSIONS)
            .map(|path| root.join(path.as_path()))
            .collect();
        self.excluded_worktree_roots.sort();
        self.excluded_worktree_roots.dedup();
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WatchExclusionError {
    message: String,
}

impl WatchExclusionError {
    pub fn new(message: impl Into<String>) -> Self {
        let mut message = message.into();
        while message.len() > 256 {
            message.pop();
        }
        Self { message }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

fn add_git_targets(targets: &mut Vec<WatchTarget>, git: &Path) {
    for (relative, recursive, scope) in [
        ("index", false, WatchScope::Index),
        ("HEAD", false, WatchScope::Head),
        ("refs", true, WatchScope::Refs),
        ("packed-refs", false, WatchScope::PackedRefs),
        ("config", false, WatchScope::Config),
        ("info/exclude", false, WatchScope::Config),
        ("rebase-merge", true, WatchScope::Sequencer),
        ("rebase-apply", true, WatchScope::Sequencer),
        ("sequencer", true, WatchScope::Sequencer),
        ("MERGE_HEAD", false, WatchScope::Sequencer),
        ("CHERRY_PICK_HEAD", false, WatchScope::Sequencer),
        ("REVERT_HEAD", false, WatchScope::Sequencer),
        ("BISECT_LOG", false, WatchScope::Sequencer),
        ("BISECT_START", false, WatchScope::Sequencer),
    ] {
        targets.push(target(&git.join(relative), recursive, scope));
    }
}

fn add_common_targets(targets: &mut Vec<WatchTarget>, common: &Path) {
    // Do not recurse into `objects`: object creation alone does not change a visible snapshot and
    // a large repository can contain millions of object files.
    targets.push(target(common, false, WatchScope::CommonGitDirectory));
    targets.push(target(&common.join("HEAD"), false, WatchScope::Head));
    targets.push(target(
        &common.join("worktrees"),
        true,
        WatchScope::CommonGitDirectory,
    ));
    targets.push(target(&common.join("refs"), true, WatchScope::Refs));
    targets.push(target(
        &common.join("packed-refs"),
        false,
        WatchScope::PackedRefs,
    ));
    targets.push(target(&common.join("config"), false, WatchScope::Config));
    targets.push(target(
        &common.join("info/exclude"),
        false,
        WatchScope::Config,
    ));
}

fn target(path: &Path, recursive: bool, scope: WatchScope) -> WatchTarget {
    WatchTarget {
        path: path.to_path_buf(),
        recursive,
        scope,
    }
}

pub trait RepositoryWatcherPort: Send {
    /// Waits up to `timeout` for one bounded batch.
    ///
    /// # Errors
    ///
    /// Returns a typed platform or filesystem watcher failure.
    fn wait(&mut self, timeout: Duration) -> Result<WatchBatch, WatchError>;

    /// Replaces the ignored worktree roots without re-registering, so no change is missed.
    /// Returns `false` when the registration depends on them and the watcher must be replaced.
    fn update_exclusions(&mut self, _excluded_worktree_roots: &[PathBuf]) -> bool {
        false
    }
}

pub trait PlatformWatcherPort: Send + Sync {
    /// Whether one recursive registration covers the worktree without walking its descendants.
    /// Such a watch is cheap enough to start before the first scan and filter ignored paths later.
    fn covers_recursively(&self) -> bool {
        false
    }

    /// Creates a watcher for all targets in one repository plan.
    ///
    /// # Errors
    ///
    /// Returns a typed error if the platform watcher cannot initialize its baseline.
    fn watch(
        &self,
        plan: &RepositoryWatchPlan,
    ) -> Result<Box<dyn RepositoryWatcherPort>, WatchError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WatchErrorKind {
    Io,
    Capacity,
    Closed,
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WatchError {
    pub kind: WatchErrorKind,
    message: String,
}

impl WatchError {
    pub fn new(kind: WatchErrorKind, message: impl Into<String>) -> Self {
        let mut message = message.into();
        while message.len() > 512 {
            message.pop();
        }
        Self { kind, message }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for WatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for WatchError {}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RefreshReason {
    Filesystem,
    Overflow,
    Manual,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefreshIntent {
    pub reason: RefreshReason,
    pub full_rescan: bool,
    pub paths: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationWriteToken(u64);

impl OperationWriteToken {
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 {
            None
        } else {
            Some(Self(value))
        }
    }
}

#[derive(Clone, Debug)]
struct SuppressedOperation {
    token: OperationWriteToken,
    paths: BTreeSet<PathBuf>,
    deadline_ms: u64,
    acknowledged: bool,
}

/// Deterministic per-session refresh policy. Time is supplied by its caller as monotonic
/// milliseconds so fake-watcher tests never sleep.
pub struct RefreshCoordinator {
    worktree: WorktreeId,
    coalesce_ms: u64,
    last_event_ms: Option<u64>,
    pending_paths: BTreeSet<PathBuf>,
    pending_overflow: bool,
    suppressed: Vec<SuppressedOperation>,
    temporary_paths: TemporaryPathRules,
}

impl RefreshCoordinator {
    pub fn new(worktree: WorktreeId, coalesce_ms: u64) -> Self {
        Self {
            worktree,
            coalesce_ms,
            last_event_ms: None,
            pending_paths: BTreeSet::new(),
            pending_overflow: false,
            suppressed: Vec::new(),
            temporary_paths: TemporaryPathRules::default(),
        }
    }

    /// Replaces the ignored temporary-file rules (default: [`TemporaryPathRules::default`]).
    #[must_use]
    pub fn with_temporary_paths(mut self, rules: TemporaryPathRules) -> Self {
        self.temporary_paths = rules;
        self
    }

    pub fn worktree(&self) -> &WorktreeId {
        &self.worktree
    }

    /// Whether ingested changes or a lost-event overflow wait for their debounce deadline.
    pub fn has_pending(&self) -> bool {
        self.last_event_ms.is_some() || self.pending_overflow
    }

    /// Registers all exact paths touched by one application operation until a monotonic deadline.
    pub fn begin_own_write(
        &mut self,
        token: OperationWriteToken,
        paths: impl IntoIterator<Item = PathBuf>,
        deadline_ms: u64,
    ) {
        let paths = paths.into_iter().collect::<BTreeSet<_>>();
        if paths.is_empty() {
            return;
        }
        self.suppressed.retain(|item| item.token != token);
        self.suppressed.push(SuppressedOperation {
            token,
            paths,
            deadline_ms,
            acknowledged: false,
        });
    }

    /// Acknowledges operation completion and extends suppression through its debounce deadline.
    pub fn acknowledge_own_write(
        &mut self,
        token: OperationWriteToken,
        debounce_deadline_ms: u64,
    ) -> bool {
        let Some(operation) = self.suppressed.iter_mut().find(|item| item.token == token) else {
            return false;
        };
        operation.acknowledged = true;
        operation.deadline_ms = operation.deadline_ms.max(debounce_deadline_ms);
        true
    }

    pub fn own_write_acknowledged(&self, token: OperationWriteToken) -> bool {
        self.suppressed
            .iter()
            .any(|item| item.token == token && item.acknowledged)
    }

    pub fn ingest(&mut self, batch: WatchBatch, observed_ms: u64) {
        let mut relevant = false;
        for change in batch.changes {
            if self.temporary_paths.matches(&change.path)
                || self.suppressed.iter().any(|suppressed| {
                    observed_ms <= suppressed.deadline_ms && suppressed.paths.contains(&change.path)
                })
            {
                continue;
            }
            relevant = true;
            if self.pending_paths.len() < MAX_COALESCED_PATHS {
                self.pending_paths.insert(change.path);
            } else {
                self.pending_overflow = true;
            }
        }
        self.suppressed
            .retain(|item| observed_ms <= item.deadline_ms);
        if batch.overflow {
            relevant = true;
            self.pending_overflow = true;
        }
        if relevant {
            self.last_event_ms = Some(observed_ms);
        }
    }

    /// Emits one coalesced intent after the quiet window, or immediately for overflow.
    pub fn take_due(&mut self, now_ms: u64) -> Option<RefreshIntent> {
        let last_event = self.last_event_ms?;
        if !self.pending_overflow && now_ms.saturating_sub(last_event) < self.coalesce_ms {
            return None;
        }
        let reason = if self.pending_overflow {
            RefreshReason::Overflow
        } else {
            RefreshReason::Filesystem
        };
        Some(self.issue(reason, self.pending_overflow))
    }

    /// Creates an immediate complete refresh intent and folds any pending burst into it.
    pub fn manual_refresh(&mut self) -> RefreshIntent {
        self.issue(RefreshReason::Manual, true)
    }

    fn issue(&mut self, reason: RefreshReason, full_rescan: bool) -> RefreshIntent {
        let paths = std::mem::take(&mut self.pending_paths)
            .into_iter()
            .collect();
        self.last_event_ms = None;
        self.pending_overflow = false;
        RefreshIntent {
            reason,
            full_rescan,
            paths,
        }
    }
}

/// Whether `path` matches the default [`TemporaryPathRules`].
pub fn is_tool_temporary_path(path: &Path) -> bool {
    TemporaryPathRules::default().matches(path)
}

#[cfg(test)]
mod p02_watch_tests {
    use super::*;
    use crate::types::{RepositoryId, RepositoryPath};
    use std::collections::VecDeque;

    struct FakeWatcher(VecDeque<WatchBatch>);

    impl RepositoryWatcherPort for FakeWatcher {
        fn wait(&mut self, _timeout: Duration) -> Result<WatchBatch, WatchError> {
            self.0.pop_front().ok_or_else(|| {
                WatchError::new(WatchErrorKind::Closed, "fake watcher queue exhausted")
            })
        }
    }

    fn worktree() -> WorktreeId {
        WorktreeId::new(
            RepositoryId::new(RepositoryPath::new("/repo/.git").unwrap()),
            RepositoryPath::new("/repo").unwrap(),
            RepositoryPath::new("/repo/.git").unwrap(),
        )
    }

    #[test]
    fn temporary_path_rules_are_configurable_and_default_to_the_historical_names() {
        let defaults = TemporaryPathRules::default();
        assert!(defaults.matches(Path::new("/repo/.qspline-patch")));
        assert!(defaults.matches(Path::new("/repo/dir/x.qspline-tmp")));
        assert!(!defaults.matches(Path::new("/repo/.gitlama-patch-1")));
        assert!(is_tool_temporary_path(Path::new("/repo/qspline-tmp-1/x")));
        let custom = TemporaryPathRules {
            prefixes: vec![".gitlama-".into()],
            suffixes: Vec::new(),
        };
        let mut coordinator = RefreshCoordinator::new(worktree(), 0).with_temporary_paths(custom);
        coordinator.ingest(
            WatchBatch {
                changes: vec![change("/repo/.gitlama-patch-1")],
                overflow: false,
            },
            1,
        );
        assert!(coordinator.take_due(100).is_none());
        coordinator.ingest(
            WatchBatch {
                changes: vec![change("/repo/.qspline-x")],
                overflow: false,
            },
            1,
        );
        assert!(coordinator.take_due(100).is_some());
        assert!(!TemporaryPathRules::none().matches(Path::new("/repo/.qspline-x")));
    }

    fn change(path: &str) -> WatchChange {
        WatchChange {
            path: PathBuf::from(path),
            kind: WatchChangeKind::Modified,
        }
    }

    #[test]
    fn fake_bursts_coalesce_ignore_own_writes_and_overflow_immediately() {
        let worktree = worktree();
        let mut coordinator = RefreshCoordinator::new(worktree, 50);
        let token = OperationWriteToken::new(1).unwrap();
        coordinator.begin_own_write(token, [PathBuf::from("/repo/index")], 100);
        let mut watcher = FakeWatcher(VecDeque::from([
            WatchBatch {
                changes: vec![
                    change("/repo/index"),
                    change("/repo/.qspline-stage.tmp"),
                    change("/repo/a"),
                ],
                overflow: false,
            },
            WatchBatch {
                changes: vec![change("/repo/b"), change("/repo/a")],
                overflow: false,
            },
            WatchBatch {
                changes: Vec::new(),
                overflow: true,
            },
        ]));
        coordinator.ingest(watcher.wait(Duration::ZERO).unwrap(), 10);
        coordinator.ingest(watcher.wait(Duration::ZERO).unwrap(), 20);
        assert_eq!(coordinator.take_due(69), None);
        let ticket = coordinator.take_due(70).unwrap();
        assert_eq!(ticket.reason, RefreshReason::Filesystem);
        assert_eq!(
            ticket.paths,
            [PathBuf::from("/repo/a"), PathBuf::from("/repo/b")]
        );
        coordinator.ingest(watcher.wait(Duration::ZERO).unwrap(), 61);
        let overflow = coordinator.take_due(61).unwrap();
        assert_eq!(overflow.reason, RefreshReason::Overflow);
        assert!(overflow.full_rescan);
    }

    #[test]
    fn intents_never_allocate_repository_generations() {
        let mut coordinator = RefreshCoordinator::new(worktree(), 50);
        let first = coordinator.manual_refresh();
        let second = coordinator.manual_refresh();
        assert_eq!(first.reason, RefreshReason::Manual);
        assert_eq!(second.reason, RefreshReason::Manual);
    }

    #[test]
    fn own_write_suppression_survives_many_events_and_unrelated_interleaving() {
        let mut coordinator = RefreshCoordinator::new(worktree(), 10);
        let token = OperationWriteToken::new(7).unwrap();
        coordinator.begin_own_write(
            token,
            [
                PathBuf::from("/repo/index"),
                PathBuf::from("/repo/index.lock"),
            ],
            20,
        );
        for observed_ms in 1..=6 {
            coordinator.ingest(
                WatchBatch {
                    changes: vec![change("/repo/index"), change("/repo/index.lock")],
                    overflow: false,
                },
                observed_ms,
            );
        }
        coordinator.ingest(
            WatchBatch {
                changes: vec![change("/repo/unrelated")],
                overflow: false,
            },
            7,
        );
        assert!(coordinator.acknowledge_own_write(token, 30));
        assert!(coordinator.own_write_acknowledged(token));
        coordinator.ingest(
            WatchBatch {
                changes: vec![change("/repo/index")],
                overflow: false,
            },
            25,
        );
        let intent = coordinator.take_due(35).unwrap();
        assert_eq!(intent.paths, [PathBuf::from("/repo/unrelated")]);
        coordinator.ingest(
            WatchBatch {
                changes: vec![change("/repo/index")],
                overflow: false,
            },
            31,
        );
        assert_eq!(
            coordinator.take_due(41).unwrap().paths,
            [PathBuf::from("/repo/index")]
        );
    }

    #[test]
    fn linked_worktree_plan_contains_per_worktree_and_common_git_state() {
        let worktree = WorktreeId::new(
            RepositoryId::new(RepositoryPath::new("/common").unwrap()),
            RepositoryPath::new("/linked").unwrap(),
            RepositoryPath::new("/common/worktrees/linked").unwrap(),
        );
        let plan = RepositoryWatchPlan::for_worktree(worktree);
        assert!(plan
            .targets
            .iter()
            .any(|item| item.scope == WatchScope::Index));
        assert!(plan
            .targets
            .iter()
            .any(|item| item.scope == WatchScope::Sequencer));
        assert!(plan
            .targets
            .iter()
            .any(|item| item.scope == WatchScope::CommonGitDirectory));
    }
}

pub mod ignore;
pub mod platform;
