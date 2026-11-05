//! Frozen-tip graph loading with consistent boundaries and batched metadata reads.
use super::CommandExecutor;
use crate::activity::{ActivityId, ActivityRecord, ActivityStatus};
use crate::cancellation::Cancellation;
use crate::history::MAX_REV_LIST_RECORD_BYTES;
use crate::history::{
    batch_metadata_requests, parse_object_id, parse_rev_list_record, GraphParseError,
    RevListParser, RevListRecord,
};
use crate::process::{GitProcessCommand, ProcessArgument};
use crate::refs::STANDARD_REFERENCE_PATTERNS;
use crate::types::{ObjectId, RefId, WorktreeId};
use crate::version::GitVersion;
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;
pub const MAX_METADATA_OBJECT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_METADATA_AUTHOR_BYTES: usize = 512;
pub const MAX_METADATA_SUBJECT_BYTES: usize = 1_024;
const TOPOLOGY_OUTPUT_LIMIT: usize = 64 * 1024 * 1024;
const SMALL_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
const METADATA_PREFLIGHT_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
const METADATA_BATCH_CONTENT_BYTES: usize = 16 * 1024 * 1024;
const METADATA_OUTPUT_LIMIT: usize = 20 * 1024 * 1024;
const BOUNDARY_FILE_LIMIT: usize = 4 * 1024 * 1024;
const BOUNDARY_READ_CHUNK_BYTES: usize = 16 * 1024;
const MAX_BOUNDARY_STATE_ATTEMPTS: usize = 2;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum GraphHistoryOrder {
    Topological,
    CommitterDate,
}

/// The tips a history traversal starts from.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum GraphTraversalScope {
    /// `HEAD` only.
    Head,
    /// `HEAD` and every reference, like `git log --all`.
    AllReferences,
    /// `HEAD` and the references in
    /// [`STANDARD_REFERENCE_PATTERNS`](crate::refs::STANDARD_REFERENCE_PATTERNS): branches,
    /// remote-tracking branches, tags and the stash. Commits reachable only from other
    /// references, such as notes or tool checkpoints, are not part of the history.
    StandardReferences,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IngestedCommit {
    pub id: ObjectId,
    pub parents: Vec<ObjectId>,
    pub refs: Vec<RefId>,
    pub metadata: Option<GraphCommitMetadata>,
}

impl IngestedCommit {
    pub fn topology(id: ObjectId, parents: Vec<ObjectId>) -> Self {
        Self {
            id,
            parents,
            refs: Vec::new(),
            metadata: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphMetadataRecord {
    pub id: ObjectId,
    pub metadata: GraphCommitMetadata,
    pub status: GraphMetadataStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphMetadataStatus {
    Available,
    Unavailable(GraphMetadataUnavailableKind),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphMetadataUnavailableKind {
    ObjectTooLarge,
    AuthorTooLarge,
    SubjectTooLarge,
    MalformedCommit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphQueryFailureKind {
    InvalidRequest,
    InvalidPageToken,
    MissingGit,
    UnsupportedGit,
    RepositoryUnavailable,
    MalformedOutput,
    ProcessFailed,
    OutputTooLarge,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphQueryFailure {
    kind: GraphQueryFailureKind,
    message: String,
}

impl GraphQueryFailure {
    pub const MAX_MESSAGE_BYTES: usize = 4096;

    pub fn new(kind: GraphQueryFailureKind, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > Self::MAX_MESSAGE_BYTES {
            let mut end = Self::MAX_MESSAGE_BYTES;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
        }
        Self { kind, message }
    }

    pub fn public(kind: GraphQueryFailureKind, message: &'static str) -> Self {
        Self::new(kind, message)
    }

    pub const fn kind(&self) -> GraphQueryFailureKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Metadata read independently from the topology stream.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GraphCommitMetadata {
    pub subject: Vec<u8>,
    pub author: Vec<u8>,
    /// Lowercase SHA-256 Gravatar identifier. The author email never leaves the Git adapter.
    pub avatar_hash: String,
    pub author_time: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GraphCondition {
    ShallowBoundary { commit: ObjectId, parent: ObjectId },
    MissingObject { commit: ObjectId, object: ObjectId },
    ReplaceRef { commit: ObjectId },
    Graft { commit: ObjectId },
}

/// Cache key for one frozen-tip history traversal.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GraphRequest {
    pub worktree: WorktreeId,
    pub order: GraphHistoryOrder,
    pub scope: GraphTraversalScope,
}
/// Owned immutable-data result. Revisions and window publication belong to the caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedTopology {
    pub rows: Vec<IngestedCommit>,
    pub conditions: Vec<GraphCondition>,
    pub frozen_tips: Vec<ObjectId>,
    pub activities: Vec<ActivityId>,
}

/// Frozen topology storage shared by repository caches and UI paging adapters.
/// Cloning this handle retains the same allocation; windows clone only their rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrozenTopology(std::sync::Arc<LoadedTopology>);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphWindowConditions {
    pub conditions: Vec<GraphCondition>,
    pub total_count: usize,
    pub missing_object_count: usize,
    pub truncated: bool,
}

impl FrozenTopology {
    /// Validates identity uniqueness without copying topology records.
    /// # Errors
    /// Rejects duplicate commit IDs in malformed topology data.
    pub fn new(topology: std::sync::Arc<LoadedTopology>) -> Result<Self, GraphQueryFailure> {
        let mut identities = std::collections::HashSet::with_capacity(topology.rows.len());
        if topology.rows.iter().any(|row| !identities.insert(&row.id)) {
            return Err(GraphQueryFailure::public(
                GraphQueryFailureKind::MalformedOutput,
                "Git returned a duplicate history object",
            ));
        }
        Ok(Self(topology))
    }
    pub fn loaded(&self) -> &std::sync::Arc<LoadedTopology> {
        &self.0
    }
    pub fn rows(&self) -> &[IngestedCommit] {
        &self.0.rows
    }
    pub fn len(&self) -> usize {
        self.0.rows.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.rows.is_empty()
    }
    pub fn position_after(&self, id: &ObjectId) -> Option<usize> {
        self.0
            .rows
            .iter()
            .position(|row| &row.id == id)
            .map(|position| position + 1)
    }
    pub fn same_topology_as(&self, other: &Self) -> bool {
        self.0.conditions == other.0.conditions
            && self.len() == other.len()
            && self
                .rows()
                .iter()
                .zip(other.rows())
                .all(|(left, right)| left.id == right.id && left.parents == right.parents)
    }
    pub fn window_rows(&self, offset: usize, limit: usize) -> Vec<IngestedCommit> {
        let start = offset.min(self.len());
        let end = start.saturating_add(limit).min(self.len());
        self.0.rows[start..end].to_vec()
    }
    pub fn window_conditions(
        &self,
        offset: usize,
        limit: usize,
        condition_limit: usize,
    ) -> GraphWindowConditions {
        let start = offset.min(self.len());
        let end = start.saturating_add(limit).min(self.len());
        let identities = self.0.rows[start..end]
            .iter()
            .map(|row| &row.id)
            .collect::<std::collections::HashSet<_>>();
        let mut conditions = Vec::with_capacity(condition_limit.min(self.0.conditions.len()));
        let mut total_count = 0usize;
        let mut missing_object_count = 0usize;
        for condition in &self.0.conditions {
            let commit = match condition {
                GraphCondition::ShallowBoundary { commit, .. }
                | GraphCondition::MissingObject { commit, .. }
                | GraphCondition::ReplaceRef { commit }
                | GraphCondition::Graft { commit } => commit,
            };
            if !identities.contains(commit) {
                continue;
            }
            total_count = total_count.saturating_add(1);
            if matches!(condition, GraphCondition::MissingObject { .. }) {
                missing_object_count = missing_object_count.saturating_add(1);
            }
            if conditions.len() < condition_limit {
                conditions.push(condition.clone());
            }
        }
        GraphWindowConditions {
            truncated: conditions.len() < total_count,
            conditions,
            total_count,
            missing_object_count,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedMetadata {
    pub records: Vec<GraphMetadataRecord>,
    pub missing: Vec<ObjectId>,
    pub activities: Vec<ActivityId>,
}
#[derive(Clone, Debug)]
pub struct GraphLoader {
    executable: OsString,
    known_version: Option<GitVersion>,
    topology_limit: Option<usize>,
}
impl Default for GraphLoader {
    fn default() -> Self {
        Self::new("git")
    }
}
impl GraphLoader {
    pub fn new(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
            known_version: None,
            topology_limit: None,
        }
    }
    #[must_use]
    pub const fn with_known_version(mut self, version: GitVersion) -> Self {
        self.known_version = Some(version);
        self
    }
    #[must_use]
    pub const fn with_topology_limit(mut self, limit: usize) -> Self {
        self.topology_limit = Some(limit);
        self
    }
    /// Loads topology against frozen tips and verifies shallow/graft/replacement consistency.
    ///
    /// # Errors
    /// Returns cancellation or bounded Git, parsing, and boundary-consistency failures.
    pub fn load(
        &self,
        query: &GraphRequest,
        cancellation: &dyn Cancellation,
        executor: &mut dyn CommandExecutor,
    ) -> Result<LoadedTopology, GraphLoadError> {
        check_cancellation(cancellation)?;
        let session = GraphSession {
            executable: self.executable.clone(),
            known_version: self.known_version,
            topology_limit: self.topology_limit,
            executor: RefCell::new(executor),
        };
        let mut activities = Vec::new();
        let mut loaded = session.query_inner(query, cancellation, &mut activities)?;
        let mut identities = std::collections::HashSet::with_capacity(loaded.rows.len());
        if loaded.rows.iter().any(|row| !identities.insert(&row.id)) {
            return Err(GraphLoadError::Failed(GraphQueryFailure::public(
                GraphQueryFailureKind::MalformedOutput,
                "Git returned a duplicate history object",
            )));
        }
        loaded.activities = activities;
        Ok(loaded)
    }
    /// Loads a bounded set of metadata with one size preflight and size-aware content batches.
    ///
    /// # Errors
    /// Rejects empty, duplicate, or over-512 requests, cancellation, and failed or malformed Git output.
    pub fn metadata(
        &self,
        worktree: &WorktreeId,
        object_ids: &[ObjectId],
        cancellation: &dyn Cancellation,
        executor: &mut dyn CommandExecutor,
    ) -> Result<LoadedMetadata, GraphLoadError> {
        check_cancellation(cancellation)?;
        let mut unique = std::collections::HashSet::new();
        if object_ids.is_empty()
            || object_ids.len() > 512
            || object_ids.iter().any(|id| !unique.insert(id))
        {
            return Err(GraphLoadError::Failed(GraphQueryFailure::public(
                GraphQueryFailureKind::InvalidRequest,
                "invalid graph metadata batch",
            )));
        }
        let request = MetadataRequest {
            worktree,
            object_ids,
        };
        let session = GraphSession {
            executable: self.executable.clone(),
            known_version: self.known_version,
            topology_limit: self.topology_limit,
            executor: RefCell::new(executor),
        };
        let mut activities = Vec::new();
        let (records, missing) =
            session.metadata_inner_result(&request, cancellation, &mut activities)?;
        Ok(LoadedMetadata {
            records,
            missing,
            activities,
        })
    }
}
struct GraphSession<'a> {
    executable: OsString,
    known_version: Option<GitVersion>,
    topology_limit: Option<usize>,
    executor: RefCell<&'a mut dyn CommandExecutor>,
}
struct MetadataRequest<'a> {
    worktree: &'a WorktreeId,
    object_ids: &'a [ObjectId],
}
impl MetadataRequest<'_> {
    const fn object_ids(&self) -> &[ObjectId] {
        self.object_ids
    }
}
static GRAPH_PERF_TRACE_ENABLED: OnceLock<bool> = OnceLock::new();

struct GraphPerfSpan {
    phase: &'static str,
    started: Option<Instant>,
}

impl GraphPerfSpan {
    fn new(phase: &'static str) -> Self {
        let enabled = *GRAPH_PERF_TRACE_ENABLED.get_or_init(|| {
            std::env::var_os("DEVMIL_GIT_PERF_TRACE").as_deref() == Some(OsStr::new("1"))
        });
        if enabled {
            eprintln!("DEVMIL_GIT_PERF phase={phase}.start elapsed_us=0");
        }
        Self {
            phase,
            started: enabled.then(Instant::now),
        }
    }
}

impl Drop for GraphPerfSpan {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            eprintln!(
                "DEVMIL_GIT_PERF phase={}.end elapsed_us={}",
                self.phase,
                started.elapsed().as_micros()
            );
        }
    }
}

impl GraphSession<'_> {
    fn run(
        &self,
        directory: Option<&Path>,
        arguments: impl IntoIterator<Item = ProcessArgument>,
        output_limit: usize,
        _cancellation: &dyn Cancellation,
    ) -> PrivateGraphRun {
        let mut command = GitProcessCommand::git(self.executable.clone())
            .args(arguments)
            .output_limit(output_limit)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .expect("constant environment name is valid")
            .env("LC_ALL", "C")
            .expect("constant environment name is valid");
        if let Some(directory) = directory {
            command = command.current_dir(directory);
        }
        let mut result = self.executor.borrow_mut().execute(command);
        PrivateGraphRun {
            record: result.activity,
            stdout: result.stdout.take_bytes(),
            stdout_truncated: result.stdout.truncated,
        }
    }

    fn run_with_stdin(
        &self,
        directory: &Path,
        arguments: impl IntoIterator<Item = ProcessArgument>,
        stdin: Vec<u8>,
        output_limit: usize,
        _cancellation: &dyn Cancellation,
    ) -> PrivateGraphRun {
        let command = GitProcessCommand::git(self.executable.clone())
            .args(arguments)
            .current_dir(directory)
            .stdin_bytes(stdin)
            .output_limit(output_limit)
            // Batched commit metadata contains subjects and authors. Keep parser bytes private;
            // safe version, topology, ref, and object-ID plumbing remains visible in activity.
            .redact_output()
            .env("GIT_OPTIONAL_LOCKS", "0")
            .expect("constant environment name is valid")
            .env("LC_ALL", "C")
            .expect("constant environment name is valid");
        let mut result = self.executor.borrow_mut().execute(command);
        PrivateGraphRun {
            record: result.activity,
            stdout: result.stdout.take_bytes(),
            stdout_truncated: result.stdout.truncated,
        }
    }

    fn run_without_shallow_boundaries(
        &self,
        directory: &Path,
        arguments: impl IntoIterator<Item = ProcessArgument>,
        output_limit: usize,
        _cancellation: &dyn Cancellation,
    ) -> PrivateGraphRun {
        let command = GitProcessCommand::git(self.executable.clone())
            .args(arguments)
            .current_dir(directory)
            .output_limit(output_limit)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .expect("constant environment name is valid")
            .env("LC_ALL", "C")
            .expect("constant environment name is valid")
            .env(
                "GIT_SHALLOW_FILE",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .expect("constant environment name is valid");
        let mut result = self.executor.borrow_mut().execute(command);
        PrivateGraphRun {
            record: result.activity,
            stdout: result.stdout.take_bytes(),
            stdout_truncated: result.stdout.truncated,
        }
    }

    fn read_version(
        &self,
        cancellation: &dyn Cancellation,
        activities: &mut Vec<ActivityId>,
    ) -> Result<GitVersion, GraphLoadError> {
        let _perf = GraphPerfSpan::new("git.graph.version");
        let run = self.run(
            None,
            [ProcessArgument::visible(OsStr::new("--version"))],
            SMALL_OUTPUT_LIMIT,
            cancellation,
        );
        activities.push(run.record.id);
        match run.record.status {
            ActivityStatus::Canceled { .. } => Err(GraphLoadError::Canceled),
            ActivityStatus::LaunchError { .. } => {
                Err(GraphLoadError::Failed(GraphQueryFailure::public(
                    GraphQueryFailureKind::MissingGit,
                    "Git could not be started",
                )))
            }
            ActivityStatus::Succeeded { .. } if !run.stdout_truncated => {
                let version = GitVersion::parse(&run.stdout).ok_or_else(|| {
                    GraphLoadError::Failed(GraphQueryFailure::public(
                        GraphQueryFailureKind::MalformedOutput,
                        "Git returned an invalid version",
                    ))
                })?;
                if version >= GitVersion::new(2, 39, 0) {
                    Ok(version)
                } else {
                    Err(GraphLoadError::Failed(GraphQueryFailure::public(
                        GraphQueryFailureKind::UnsupportedGit,
                        "Git 2.39.0 or newer is required",
                    )))
                }
            }
            ActivityStatus::Succeeded { .. } => Err(output_too_large()),
            _ => Err(GraphLoadError::Failed(GraphQueryFailure::public(
                GraphQueryFailureKind::ProcessFailed,
                "Git version detection failed",
            ))),
        }
    }

    fn repository_is_available(
        &self,
        directory: &Path,
        cancellation: &dyn Cancellation,
        activities: &mut Vec<ActivityId>,
    ) -> Result<(), GraphLoadError> {
        let _perf = GraphPerfSpan::new("git.graph.repository_probe");
        let run = self.run(
            Some(directory),
            [
                ProcessArgument::visible("rev-parse"),
                ProcessArgument::visible("--git-dir"),
            ],
            SMALL_OUTPUT_LIMIT,
            cancellation,
        );
        activities.push(run.record.id);
        successful(&run).map_err(|step| match step {
            GraphLoadError::Failed(_) => GraphLoadError::Failed(GraphQueryFailure::public(
                GraphQueryFailureKind::RepositoryUnavailable,
                "Repository history is unavailable",
            )),
            GraphLoadError::Canceled => GraphLoadError::Canceled,
        })
    }

    fn frozen_tips(
        &self,
        query: &GraphRequest,
        cancellation: &dyn Cancellation,
        activities: &mut Vec<ActivityId>,
    ) -> Result<FrozenTips, GraphLoadError> {
        let _perf = GraphPerfSpan::new("git.graph.freeze_tips");
        let directory = query.worktree.worktree_root().as_path();
        let mut tips = Vec::new();
        let mut named_tips = Vec::new();
        let head = self.run(
            Some(directory),
            [
                ProcessArgument::visible("rev-parse"),
                ProcessArgument::visible("--verify"),
                ProcessArgument::visible("HEAD"),
            ],
            SMALL_OUTPUT_LIMIT,
            cancellation,
        );
        activities.push(head.record.id);
        match head.record.status {
            ActivityStatus::Succeeded { .. } if !head.stdout_truncated => {
                tips.push(parse_single_object_id(&head.stdout).map_err(GraphLoadError::Failed)?);
            }
            ActivityStatus::Canceled { .. } => return Err(GraphLoadError::Canceled),
            ActivityStatus::Succeeded { .. } => return Err(output_too_large()),
            ActivityStatus::LaunchError { .. } => {
                return Err(GraphLoadError::Failed(GraphQueryFailure::public(
                    GraphQueryFailureKind::MissingGit,
                    "Git could not be started",
                )));
            }
            // `rev-parse --verify HEAD` exits nonzero for an unborn branch. The repository
            // availability probe above keeps this distinct from an invalid repository.
            _ => {}
        }

        let patterns: Option<&[&str]> = match query.scope {
            GraphTraversalScope::Head => None,
            GraphTraversalScope::AllReferences => Some(&[]),
            GraphTraversalScope::StandardReferences => Some(&STANDARD_REFERENCE_PATTERNS),
        };
        if let Some(patterns) = patterns {
            let mut arguments = vec![
                ProcessArgument::visible("for-each-ref"),
                // An annotated tag names a tag object. Its peeled commit is the
                // row the reference belongs to.
                ProcessArgument::visible("--format=%(refname)%00%(objectname)%00%(*objectname)"),
            ];
            // Without patterns Git lists every reference.
            arguments.extend(patterns.iter().copied().map(ProcessArgument::visible));
            let refs = self.run(Some(directory), arguments, SMALL_OUTPUT_LIMIT, cancellation);
            activities.push(refs.record.id);
            successful(&refs)?;
            for (object, reference) in parse_frozen_reference_tips(&refs.stdout)? {
                tips.push(object.clone());
                named_tips.push((object, reference));
            }
        }
        let mut seen = std::collections::HashSet::with_capacity(tips.len());
        tips.retain(|tip| seen.insert(tip.clone()));
        Ok(FrozenTips {
            objects: tips,
            named: named_tips,
        })
    }

    fn topology(
        &self,
        query: &GraphRequest,
        tips: &[ObjectId],
        capabilities: GraphCapabilities,
        cancellation: &dyn Cancellation,
        activities: &mut Vec<ActivityId>,
    ) -> Result<(Vec<IngestedCommit>, Vec<GraphCondition>), GraphLoadError> {
        let _perf = GraphPerfSpan::new("git.graph.topology_parse");
        let mut arguments = vec![
            ProcessArgument::visible("rev-list"),
            ProcessArgument::visible("--parents"),
            ProcessArgument::visible(match query.order {
                GraphHistoryOrder::Topological => "--topo-order",
                GraphHistoryOrder::CommitterDate => "--date-order",
            }),
        ];
        if capabilities.missing_print {
            arguments.push(ProcessArgument::visible("--missing=print"));
        }
        if let Some(limit) = self.topology_limit {
            arguments.push(ProcessArgument::visible(format!("--max-count={limit}")));
        }
        arguments.extend(
            tips.iter()
                .map(|tip| ProcessArgument::visible(tip.as_str())),
        );
        let run = self.run(
            Some(query.worktree.worktree_root().as_path()),
            arguments,
            TOPOLOGY_OUTPUT_LIMIT,
            cancellation,
        );
        activities.push(run.record.id);
        successful(&run)?;
        let parsed = parse_topology_output(&run.stdout).map_err(malformed_output)?;
        let mut conditions = Vec::new();
        for record in &parsed.records {
            for parent in &record.parents {
                if parsed.missing.contains(parent) {
                    conditions.push(GraphCondition::MissingObject {
                        commit: record.id.clone(),
                        object: parent.clone(),
                    });
                }
            }
        }
        Ok((
            parsed
                .records
                .into_iter()
                .map(|record| IngestedCommit::topology(record.id, record.parents))
                .collect(),
            conditions,
        ))
    }

    fn capture_boundary_state(
        &self,
        query: &GraphRequest,
        cancellation: &dyn Cancellation,
        activities: &mut Vec<ActivityId>,
    ) -> Result<GraphBoundaryState, GraphLoadError> {
        let _perf = GraphPerfSpan::new("git.graph.boundary_state");
        let common_git_dir = query.worktree.repository().common_git_dir().as_path();
        let shallow = read_object_id_file(&common_git_dir.join("shallow"), cancellation)?;
        let grafts = read_graft_file(&common_git_dir.join("info/grafts"), cancellation)?;
        let run = self.run(
            Some(query.worktree.worktree_root().as_path()),
            [
                ProcessArgument::visible("for-each-ref"),
                ProcessArgument::visible("--format=%(refname)%00%(objectname)%00"),
                ProcessArgument::visible("refs/replace"),
            ],
            SMALL_OUTPUT_LIMIT,
            cancellation,
        );
        activities.push(run.record.id);
        successful(&run)?;
        check_cancellation(cancellation)?;
        let replacements = parse_replace_refs(&run.stdout).map_err(malformed_output)?;
        Ok(GraphBoundaryState {
            shallow,
            grafts,
            replacements,
        })
    }

    fn apply_shallow_boundaries(
        &self,
        query: &GraphRequest,
        rows: &mut [IngestedCommit],
        conditions: &mut Vec<GraphCondition>,
        recovery: BoundaryRecovery<'_>,
        cancellation: &dyn Cancellation,
        activities: &mut Vec<ActivityId>,
    ) -> Result<(), GraphLoadError> {
        if recovery.shallow.is_empty() {
            return Ok(());
        }
        let present = rows
            .iter()
            .filter(|row| recovery.shallow.contains(&row.id))
            .map(|row| row.id.clone())
            .collect::<Vec<_>>();
        if present.is_empty() {
            return Ok(());
        }
        let mut arguments = vec![
            ProcessArgument::visible("rev-list"),
            ProcessArgument::visible("--parents"),
        ];
        if recovery.capabilities.no_walk {
            arguments.push(ProcessArgument::visible("--no-walk"));
        }
        arguments.extend(
            present
                .iter()
                .map(|object| ProcessArgument::visible(object.as_str())),
        );
        let run = self.run_without_shallow_boundaries(
            query.worktree.worktree_root().as_path(),
            arguments,
            TOPOLOGY_OUTPUT_LIMIT,
            cancellation,
        );
        activities.push(run.record.id);
        successful(&run)?;
        check_cancellation(cancellation)?;
        let mut parser = RevListParser::new();
        let mut raw = parser.push(&run.stdout).map_err(malformed_output)?;
        raw.extend(parser.finish().map_err(malformed_output)?);
        if raw.len() != present.len() {
            return Err(malformed_output(GraphParseError::UnexpectedRecordCount {
                expected: present.len(),
                actual: raw.len(),
            }));
        }
        let mut raw_by_id = raw
            .into_iter()
            .map(|record| (record.id.clone(), record))
            .collect::<std::collections::HashMap<_, _>>();
        for row in rows {
            let Some(raw) = raw_by_id.remove(&row.id) else {
                continue;
            };
            row.parents = raw.parents;
            conditions.retain(|condition| {
                !matches!(condition, GraphCondition::MissingObject { commit, .. } if commit == &row.id)
            });
            conditions.extend(
                row.parents
                    .iter()
                    .map(|parent| GraphCondition::ShallowBoundary {
                        commit: row.id.clone(),
                        parent: parent.clone(),
                    }),
            );
        }
        if raw_by_id.is_empty() {
            Ok(())
        } else {
            Err(malformed_output(GraphParseError::UnexpectedRecordCount {
                expected: present.len(),
                actual: present.len() + raw_by_id.len(),
            }))
        }
    }

    fn apply_replacement_conditions(
        rows: &[IngestedCommit],
        conditions: &mut Vec<GraphCondition>,
        replacements: &[ReplaceRefState],
        cancellation: &dyn Cancellation,
    ) -> Result<(), GraphLoadError> {
        check_cancellation(cancellation)?;
        let visible = rows
            .iter()
            .map(|row| &row.id)
            .collect::<std::collections::HashSet<_>>();
        conditions.extend(
            replacements
                .iter()
                .filter(|replacement| visible.contains(&replacement.commit))
                .map(|replacement| GraphCondition::ReplaceRef {
                    commit: replacement.commit.clone(),
                }),
        );
        check_cancellation(cancellation)?;
        Ok(())
    }

    fn apply_graft_conditions(
        rows: &[IngestedCommit],
        conditions: &mut Vec<GraphCondition>,
        grafts: &[GraftRecord],
        cancellation: &dyn Cancellation,
    ) -> Result<(), GraphLoadError> {
        let visible = rows
            .iter()
            .map(|row| &row.id)
            .collect::<std::collections::HashSet<_>>();
        conditions.extend(
            grafts
                .iter()
                .filter(|graft| visible.contains(&graft.commit))
                .map(|graft| GraphCondition::Graft {
                    commit: graft.commit.clone(),
                }),
        );
        check_cancellation(cancellation)?;
        Ok(())
    }
}

fn parse_frozen_reference_tips(bytes: &[u8]) -> Result<Vec<(ObjectId, RefId)>, GraphLoadError> {
    let mut result = Vec::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let fields = line
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        // The optional third field is the commit an annotated tag peels to.
        if fields.len() != 2 && fields.len() != 3 {
            return Err(GraphLoadError::Failed(GraphQueryFailure::public(
                GraphQueryFailureKind::MalformedOutput,
                "Git returned a malformed frozen reference",
            )));
        }
        let reference = RefId::from_bytes(fields[0].to_vec()).map_err(|_| {
            GraphLoadError::Failed(GraphQueryFailure::public(
                GraphQueryFailureKind::MalformedOutput,
                "Git returned an invalid frozen reference name",
            ))
        })?;
        let object = parse_object_id(fields[fields.len() - 1]).map_err(|_| {
            GraphLoadError::Failed(GraphQueryFailure::public(
                GraphQueryFailureKind::MalformedOutput,
                "Git returned an invalid reference target",
            ))
        })?;
        result.push((object, reference));
    }
    Ok(result)
}

impl GraphSession<'_> {
    fn metadata_inner_result(
        &self,
        request: &MetadataRequest<'_>,
        cancellation: &dyn Cancellation,
        activities: &mut Vec<ActivityId>,
    ) -> Result<(Vec<GraphMetadataRecord>, Vec<ObjectId>), GraphLoadError> {
        let worktree = request.worktree.worktree_root().as_path();
        let preflight = {
            let _perf = GraphPerfSpan::new("git.graph.metadata_preflight");
            self.run_with_stdin(
                worktree,
                [
                    ProcessArgument::visible("cat-file"),
                    ProcessArgument::visible("--batch-check"),
                ],
                batch_metadata_requests(request.object_ids()),
                METADATA_PREFLIGHT_OUTPUT_LIMIT,
                cancellation,
            )
        };
        activities.push(preflight.record.id);
        successful(&preflight)?;
        let preflight = parse_cat_file_preflight(&preflight.stdout, request.object_ids())
            .map_err(GraphLoadError::Failed)?;

        let mut records_by_id = std::collections::HashMap::with_capacity(preflight.len());
        let mut missing = Vec::new();
        let mut batches = Vec::<Vec<ObjectId>>::new();
        let mut batch = Vec::new();
        let mut batch_bytes = 0_usize;
        for entry in preflight {
            check_cancellation(cancellation)?;
            match entry {
                MetadataPreflight::Missing(id) => missing.push(id),
                MetadataPreflight::Oversized(id) => {
                    records_by_id.insert(id.clone(), oversized_metadata_record(id));
                }
                MetadataPreflight::Readable { id, size } => {
                    if !batch.is_empty()
                        && batch_bytes.saturating_add(size) > METADATA_BATCH_CONTENT_BYTES
                    {
                        batches.push(std::mem::take(&mut batch));
                        batch_bytes = 0;
                    }
                    batch_bytes = batch_bytes.saturating_add(size);
                    batch.push(id);
                }
            }
        }
        if !batch.is_empty() {
            batches.push(batch);
        }

        for batch in batches {
            check_cancellation(cancellation)?;
            let run = {
                let _perf = GraphPerfSpan::new("git.graph.metadata_content_batch");
                self.run_with_stdin(
                    worktree,
                    [
                        ProcessArgument::visible("cat-file"),
                        ProcessArgument::visible("--batch"),
                    ],
                    batch_metadata_requests(&batch),
                    METADATA_OUTPUT_LIMIT,
                    cancellation,
                )
            };
            activities.push(run.record.id);
            successful(&run)?;
            let (records, unexpectedly_missing) =
                parse_cat_file_metadata(&run.stdout, &batch).map_err(GraphLoadError::Failed)?;
            missing.extend(unexpectedly_missing);
            records_by_id.extend(
                records
                    .into_iter()
                    .map(|record| (record.id.clone(), record)),
            );
        }

        let records = request
            .object_ids()
            .iter()
            .filter_map(|id| records_by_id.remove(id))
            .collect();
        let missing_set = missing
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        let missing = request
            .object_ids()
            .iter()
            .filter(|id| missing_set.contains(*id))
            .cloned()
            .collect();
        Ok((records, missing))
    }

    fn query_inner(
        &self,
        query: &GraphRequest,
        cancellation: &dyn Cancellation,
        activities: &mut Vec<ActivityId>,
    ) -> Result<LoadedTopology, GraphLoadError> {
        let result = (|| {
            let version = match self.known_version {
                Some(version) => version,
                None => self.read_version(cancellation, activities)?,
            };
            let capabilities = GraphCapabilities::for_version(version);
            self.repository_is_available(
                query.worktree.worktree_root().as_path(),
                cancellation,
                activities,
            )?;
            for attempt in 0..MAX_BOUNDARY_STATE_ATTEMPTS {
                check_cancellation(cancellation)?;
                let boundary_before =
                    self.capture_boundary_state(query, cancellation, activities)?;
                let frozen = self.frozen_tips(query, cancellation, activities)?;
                let tips = frozen.objects;
                check_cancellation(cancellation)?;

                let (mut rows, mut conditions) = if tips.is_empty() {
                    (Vec::new(), Vec::new())
                } else {
                    self.topology(query, &tips, capabilities, cancellation, activities)?
                };
                let positions = rows
                    .iter()
                    .enumerate()
                    .map(|(position, row)| (row.id.clone(), position))
                    .collect::<std::collections::HashMap<_, _>>();
                for (target, reference) in &frozen.named {
                    if let Some(position) = positions.get(target) {
                        rows[*position].refs.push(reference.clone());
                    }
                }
                for row in &mut rows {
                    row.refs.sort();
                    row.refs.dedup();
                }
                check_cancellation(cancellation)?;

                if !rows.is_empty() {
                    self.apply_shallow_boundaries(
                        query,
                        &mut rows,
                        &mut conditions,
                        BoundaryRecovery {
                            shallow: &boundary_before.shallow,
                            capabilities,
                        },
                        cancellation,
                        activities,
                    )?;
                    Self::apply_replacement_conditions(
                        &rows,
                        &mut conditions,
                        &boundary_before.replacements,
                        cancellation,
                    )?;
                    Self::apply_graft_conditions(
                        &rows,
                        &mut conditions,
                        &boundary_before.grafts,
                        cancellation,
                    )?;
                }

                let boundary_after =
                    self.capture_boundary_state(query, cancellation, activities)?;
                check_cancellation(cancellation)?;
                if boundary_before != boundary_after {
                    if attempt + 1 < MAX_BOUNDARY_STATE_ATTEMPTS {
                        continue;
                    }
                    return Err(boundary_state_changed());
                }
                if rows.is_empty() {
                    return Ok(LoadedTopology {
                        rows,
                        conditions,
                        frozen_tips: tips,
                        activities: Vec::new(),
                    });
                }

                conditions.sort_by(graph_condition_order);
                conditions.dedup();
                check_cancellation(cancellation)?;
                return Ok(LoadedTopology {
                    rows,
                    conditions,
                    frozen_tips: tips,
                    activities: Vec::new(),
                });
            }
            Err(boundary_state_changed())
        })();
        result
    }
}

struct PrivateGraphRun {
    record: ActivityRecord,
    stdout: Vec<u8>,
    stdout_truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GraphBoundaryState {
    shallow: std::collections::HashSet<ObjectId>,
    grafts: Vec<GraftRecord>,
    replacements: Vec<ReplaceRefState>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FrozenTips {
    objects: Vec<ObjectId>,
    named: Vec<(ObjectId, RefId)>,
}

#[derive(Clone, Copy)]
struct BoundaryRecovery<'a> {
    shallow: &'a std::collections::HashSet<ObjectId>,
    capabilities: GraphCapabilities,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GraftRecord {
    commit: ObjectId,
    parents: Vec<ObjectId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReplaceRefState {
    commit: ObjectId,
    replacement: ObjectId,
}

#[derive(Clone, Copy, Debug)]
struct GraphCapabilities {
    missing_print: bool,
    no_walk: bool,
}

impl GraphCapabilities {
    const fn for_version(version: GitVersion) -> Self {
        let supported = version.major > 2 || (version.major == 2 && version.minor >= 39);
        Self {
            missing_print: supported,
            no_walk: supported,
        }
    }
}

struct ParsedTopology {
    records: Vec<RevListRecord>,
    missing: std::collections::HashSet<ObjectId>,
}

fn parse_topology_output(bytes: &[u8]) -> Result<ParsedTopology, GraphParseError> {
    let mut records = Vec::new();
    let mut missing = std::collections::HashSet::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if let Some(object) = line.strip_prefix(b"?") {
            missing.insert(parse_object_id(object)?);
        } else {
            if line.len() > MAX_REV_LIST_RECORD_BYTES {
                return Err(GraphParseError::RecordTooLong {
                    maximum: MAX_REV_LIST_RECORD_BYTES,
                });
            }
            records.push(parse_rev_list_record(line)?);
        }
    }
    let referenced = records
        .iter()
        .flat_map(|record| record.parents.iter())
        .collect::<std::collections::HashSet<_>>();
    if missing.iter().any(|object| !referenced.contains(object)) {
        return Err(GraphParseError::UnreferencedMissingObject);
    }
    records.retain(|record| !missing.contains(&record.id));
    Ok(ParsedTopology { records, missing })
}

fn read_object_id_file(
    path: &Path,
    cancellation: &dyn Cancellation,
) -> Result<std::collections::HashSet<ObjectId>, GraphLoadError> {
    let Some(bytes) = read_bounded_file(path, cancellation)? else {
        return Ok(std::collections::HashSet::new());
    };
    let mut objects = std::collections::HashSet::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        check_cancellation(cancellation)?;
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        objects.insert(parse_object_id(line).map_err(malformed_output)?);
    }
    Ok(objects)
}

fn read_graft_file(
    path: &Path,
    cancellation: &dyn Cancellation,
) -> Result<Vec<GraftRecord>, GraphLoadError> {
    let Some(bytes) = read_bounded_file(path, cancellation)? else {
        return Ok(Vec::new());
    };
    let mut grafts = Vec::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        check_cancellation(cancellation)?;
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let line = line.split(|byte| *byte == b'#').next().unwrap_or_default();
        let mut fields = line
            .split(u8::is_ascii_whitespace)
            .filter(|field| !field.is_empty());
        let Some(commit) = fields.next() else {
            continue;
        };
        let commit = parse_object_id(commit).map_err(malformed_output)?;
        let parents = fields
            .map(|parent| parse_object_id(parent).map_err(malformed_output))
            .collect::<Result<Vec<_>, _>>()?;
        grafts.push(GraftRecord { commit, parents });
    }
    Ok(grafts)
}

fn read_bounded_file(
    path: &Path,
    cancellation: &dyn Cancellation,
) -> Result<Option<Vec<u8>>, GraphLoadError> {
    check_cancellation(cancellation)?;
    let mut file = match open_boundary_file(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(boundary_file_unavailable()),
    };
    check_cancellation(cancellation)?;
    let metadata = file.metadata().map_err(|_| boundary_file_unavailable())?;
    if !metadata.file_type().is_file() {
        return Err(boundary_file_unavailable());
    }

    let capacity = usize::try_from(metadata.len())
        .unwrap_or(BOUNDARY_FILE_LIMIT + 1)
        .min(BOUNDARY_FILE_LIMIT + 1);
    read_bounded_contents(&mut file, capacity, cancellation).map(Some)
}

fn read_bounded_contents(
    reader: &mut impl Read,
    capacity: usize,
    cancellation: &dyn Cancellation,
) -> Result<Vec<u8>, GraphLoadError> {
    let mut bytes = Vec::with_capacity(capacity);
    let mut chunk = [0_u8; BOUNDARY_READ_CHUNK_BYTES];
    loop {
        check_cancellation(cancellation)?;
        let remaining = BOUNDARY_FILE_LIMIT + 1 - bytes.len();
        let read_limit = chunk.len().min(remaining);
        let read = reader
            .read(&mut chunk[..read_limit])
            .map_err(|_| boundary_file_unavailable())?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > BOUNDARY_FILE_LIMIT {
            return Err(output_too_large());
        }
    }
    check_cancellation(cancellation)?;
    Ok(bytes)
}

#[cfg(unix)]
fn open_boundary_file(path: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};

    rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(io::Error::from)
}

#[cfg(not(unix))]
fn open_boundary_file(path: &Path) -> io::Result<File> {
    File::open(path)
}

fn check_cancellation(cancellation: &dyn Cancellation) -> Result<(), GraphLoadError> {
    if cancellation.is_cancelled() {
        Err(GraphLoadError::Canceled)
    } else {
        Ok(())
    }
}

fn boundary_file_unavailable() -> GraphLoadError {
    GraphLoadError::Failed(GraphQueryFailure::public(
        GraphQueryFailureKind::RepositoryUnavailable,
        "Repository history boundary metadata is unavailable",
    ))
}

fn boundary_state_changed() -> GraphLoadError {
    GraphLoadError::Failed(GraphQueryFailure::public(
        GraphQueryFailureKind::RepositoryUnavailable,
        "Repository history boundaries changed while reading",
    ))
}

fn parse_replace_refs(bytes: &[u8]) -> Result<Vec<ReplaceRefState>, GraphParseError> {
    let fields = bytes
        .split(|byte| *byte == 0)
        .map(|field| {
            let field = field
                .strip_prefix(b"\r\n")
                .or_else(|| field.strip_prefix(b"\n"))
                .unwrap_or(field);
            let field = field.strip_suffix(b"\r").unwrap_or(field);
            field.strip_suffix(b"\n").unwrap_or(field)
        })
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    if fields.len() % 2 != 0 {
        return Err(GraphParseError::UnexpectedFieldCount {
            expected_multiple: 2,
            actual: fields.len(),
        });
    }
    fields
        .chunks_exact(2)
        .map(|fields| {
            let object = fields[0]
                .strip_prefix(b"refs/replace/")
                .ok_or_else(|| GraphParseError::InvalidRef(fields[0].to_vec()))?;
            Ok(ReplaceRefState {
                commit: parse_object_id(object)?,
                replacement: parse_object_id(fields[1])?,
            })
        })
        .collect()
}

fn graph_condition_order(left: &GraphCondition, right: &GraphCondition) -> std::cmp::Ordering {
    graph_condition_key(left).cmp(&graph_condition_key(right))
}

fn graph_condition_key(condition: &GraphCondition) -> (u8, &str, &str) {
    match condition {
        GraphCondition::ShallowBoundary { commit, parent } => (0, commit.as_str(), parent.as_str()),
        GraphCondition::MissingObject { commit, object } => (1, commit.as_str(), object.as_str()),
        GraphCondition::ReplaceRef { commit } => (2, commit.as_str(), ""),
        GraphCondition::Graft { commit } => (3, commit.as_str(), ""),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GraphLoadError {
    Canceled,
    Failed(GraphQueryFailure),
}

impl std::fmt::Display for GraphLoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Canceled => formatter.write_str("graph loading canceled"),
            Self::Failed(failure) => formatter.write_str(failure.message()),
        }
    }
}

impl std::error::Error for GraphLoadError {}

fn successful(run: &PrivateGraphRun) -> Result<(), GraphLoadError> {
    match run.record.status {
        ActivityStatus::Succeeded { .. } if !run.stdout_truncated => Ok(()),
        ActivityStatus::Canceled { .. } => Err(GraphLoadError::Canceled),
        ActivityStatus::LaunchError { .. } => {
            Err(GraphLoadError::Failed(GraphQueryFailure::public(
                GraphQueryFailureKind::MissingGit,
                "Git could not be started",
            )))
        }
        ActivityStatus::Succeeded { .. } => Err(output_too_large()),
        _ => Err(GraphLoadError::Failed(GraphQueryFailure::public(
            GraphQueryFailureKind::ProcessFailed,
            "Git history command failed",
        ))),
    }
}

fn output_too_large() -> GraphLoadError {
    GraphLoadError::Failed(GraphQueryFailure::public(
        GraphQueryFailureKind::OutputTooLarge,
        "Git history output exceeded its safety bound",
    ))
}

fn malformed_output(_error: GraphParseError) -> GraphLoadError {
    GraphLoadError::Failed(GraphQueryFailure::public(
        GraphQueryFailureKind::MalformedOutput,
        "Git returned malformed history output",
    ))
}

fn parse_single_object_id(bytes: &[u8]) -> Result<ObjectId, GraphQueryFailure> {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    parse_object_id(bytes).map_err(|_| {
        GraphQueryFailure::public(
            GraphQueryFailureKind::MalformedOutput,
            "Git returned an invalid object identity",
        )
    })
}

fn parse_cat_file_metadata(
    bytes: &[u8],
    expected: &[ObjectId],
) -> Result<(Vec<GraphMetadataRecord>, Vec<ObjectId>), GraphQueryFailure> {
    let frames =
        crate::history::cat_file::parse_batch(bytes, expected).map_err(|_| metadata_malformed())?;
    let mut records = Vec::with_capacity(expected.len());
    let mut missing = Vec::new();
    for frame in frames {
        let expected_id = &frame.id;
        let Some(object) = frame.contents else {
            missing.push(frame.id);
            continue;
        };
        let size = object.len();
        let (metadata, status) = if size > MAX_METADATA_OBJECT_BYTES {
            (
                metadata_fallback(b"Summary unavailable: commit object exceeds the browsing limit"),
                GraphMetadataStatus::Unavailable(GraphMetadataUnavailableKind::ObjectTooLarge),
            )
        } else {
            match parse_commit_summary(object) {
                Ok(metadata) => (metadata, GraphMetadataStatus::Available),
                Err(kind) => (
                    metadata_fallback(match kind {
                        GraphMetadataUnavailableKind::AuthorTooLarge => {
                            b"Summary unavailable: author exceeds the browsing limit"
                        }
                        GraphMetadataUnavailableKind::SubjectTooLarge => {
                            b"Summary unavailable: subject exceeds the browsing limit"
                        }
                        GraphMetadataUnavailableKind::MalformedCommit => {
                            b"Summary unavailable: commit metadata is malformed"
                        }
                        GraphMetadataUnavailableKind::ObjectTooLarge => {
                            b"Summary unavailable: commit object exceeds the browsing limit"
                        }
                    }),
                    GraphMetadataStatus::Unavailable(kind),
                ),
            }
        };
        records.push(GraphMetadataRecord {
            id: expected_id.clone(),
            metadata,
            status,
        });
    }
    Ok((records, missing))
}

#[derive(Debug, Eq, PartialEq)]
enum MetadataPreflight {
    Missing(ObjectId),
    Oversized(ObjectId),
    Readable { id: ObjectId, size: usize },
}

fn parse_cat_file_preflight(
    bytes: &[u8],
    expected: &[ObjectId],
) -> Result<Vec<MetadataPreflight>, GraphQueryFailure> {
    let sizes = crate::history::cat_file::parse_batch_check(bytes, expected)
        .map_err(|_| metadata_malformed())?;
    Ok(expected
        .iter()
        .zip(sizes)
        .map(|(id, size)| match size {
            None => MetadataPreflight::Missing(id.clone()),
            Some(size) if size > MAX_METADATA_OBJECT_BYTES => {
                MetadataPreflight::Oversized(id.clone())
            }
            Some(size) => MetadataPreflight::Readable {
                id: id.clone(),
                size,
            },
        })
        .collect())
}

fn oversized_metadata_record(id: ObjectId) -> GraphMetadataRecord {
    GraphMetadataRecord {
        id,
        metadata: metadata_fallback(
            b"Summary unavailable: commit object exceeds the browsing limit",
        ),
        status: GraphMetadataStatus::Unavailable(GraphMetadataUnavailableKind::ObjectTooLarge),
    }
}

fn parse_commit_summary(
    object: &[u8],
) -> Result<GraphCommitMetadata, GraphMetadataUnavailableKind> {
    use crate::history::summary::SummaryError;
    let summary = crate::history::summary::parse_commit_summary(
        object,
        MAX_METADATA_AUTHOR_BYTES,
        MAX_METADATA_SUBJECT_BYTES,
    )
    .map_err(|error| match error {
        SummaryError::MalformedCommit => GraphMetadataUnavailableKind::MalformedCommit,
        SummaryError::AuthorTooLarge => GraphMetadataUnavailableKind::AuthorTooLarge,
        SummaryError::SubjectTooLarge => GraphMetadataUnavailableKind::SubjectTooLarge,
    })?;
    let normalized_email = String::from_utf8_lossy(&summary.author_email)
        .trim()
        .to_lowercase();
    Ok(GraphCommitMetadata {
        subject: summary.subject,
        author: summary.author,
        author_time: summary.author_time,
        avatar_hash: format!("{:x}", Sha256::digest(normalized_email.as_bytes())),
    })
}

fn metadata_fallback(subject: &[u8]) -> GraphCommitMetadata {
    GraphCommitMetadata {
        subject: subject.to_vec(),
        author: b"Unavailable".to_vec(),
        avatar_hash: String::new(),
        author_time: 0,
    }
}

fn metadata_malformed() -> GraphQueryFailure {
    GraphQueryFailure::public(
        GraphQueryFailureKind::MalformedOutput,
        "Git returned malformed commit metadata",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    static NEXT_TEST: AtomicU64 = AtomicU64::new(1);
    struct CancellationSource(Arc<AtomicBool>);
    struct CancellationToken(Arc<AtomicBool>);
    impl CancellationSource {
        fn new() -> (Self, CancellationToken) {
            let flag = Arc::new(AtomicBool::new(false));
            (Self(flag.clone()), CancellationToken(flag))
        }
        fn cancel(&self) {
            self.0.store(true, Ordering::Release);
        }
    }
    impl Cancellation for CancellationToken {
        fn is_cancelled(&self) -> bool {
            self.0.load(Ordering::Acquire)
        }
    }

    const A: &str = "0000000000000000000000000000000000000001";
    const B: &str = "0000000000000000000000000000000000000002";

    #[test]
    fn frozen_reference_tips_preserve_exact_names_and_reject_partial_records() {
        let parsed = parse_frozen_reference_tips(
            format!("refs/heads/main\0{A}\0\nrefs/tags/v1\0{B}\0\n").as_bytes(),
        )
        .unwrap();
        assert_eq!(parsed[0].0.as_str(), A);
        assert_eq!(parsed[0].1.as_bytes(), b"refs/heads/main");
        assert_eq!(parsed[1].0.as_str(), B);
        assert_eq!(parsed[1].1.as_bytes(), b"refs/tags/v1");
        assert!(
            parse_frozen_reference_tips(format!("refs/heads/main\0{A}\0extra\n").as_bytes())
                .is_err()
        );
        // An annotated tag is attributed to the commit it peels to.
        let peeled =
            parse_frozen_reference_tips(format!("refs/tags/v2\0{A}\0{B}\n").as_bytes()).unwrap();
        assert_eq!(peeled[0].0.as_str(), B);
        assert_eq!(peeled[0].1.as_bytes(), b"refs/tags/v2");
    }

    #[cfg(unix)]
    #[test]
    fn boundary_reader_rejects_symlinks_and_devices() {
        use std::os::unix::fs::symlink;

        let directory = std::env::temp_dir().join(format!(
            "devmil-boundary-types-{}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let regular = directory.join("regular");
        std::fs::write(&regular, A).unwrap();
        let link = directory.join("link");
        symlink(&regular, &link).unwrap();
        let (_, cancellation) = CancellationSource::new();

        for path in [&link, Path::new("/dev/null")] {
            assert!(matches!(
                read_bounded_file(path, &cancellation),
                Err(GraphLoadError::Failed(failure))
                    if failure.kind() == GraphQueryFailureKind::RepositoryUnavailable
            ));
        }

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn boundary_reader_rejects_fifos() {
        use rustix::fs::{mkfifoat, Mode, CWD};

        let directory = std::env::temp_dir().join(format!(
            "devmil-boundary-fifo-{}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let fifo = directory.join("fifo");
        mkfifoat(CWD, &fifo, Mode::RUSR | Mode::WUSR).unwrap();
        let (_, cancellation) = CancellationSource::new();

        assert!(matches!(
            read_bounded_file(&fifo, &cancellation),
            Err(GraphLoadError::Failed(failure))
                if failure.kind() == GraphQueryFailureKind::RepositoryUnavailable
        ));

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn boundary_reader_observes_cancellation_before_opening() {
        let (source, cancellation) = CancellationSource::new();
        source.cancel();
        assert!(matches!(
            read_bounded_file(Path::new("boundary-must-not-be-opened"), &cancellation),
            Err(GraphLoadError::Canceled)
        ));
    }

    #[test]
    fn boundary_reader_polls_cancellation_between_chunks() {
        struct CancelAfterFirstChunk {
            source: CancellationSource,
            read: bool,
        }

        impl Read for CancelAfterFirstChunk {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if self.read {
                    return Ok(0);
                }
                self.read = true;
                buffer[0] = b'x';
                self.source.cancel();
                Ok(1)
            }
        }

        let (source, cancellation): (_, CancellationToken) = CancellationSource::new();
        let mut reader = CancelAfterFirstChunk {
            source,
            read: false,
        };
        assert!(matches!(
            read_bounded_contents(&mut reader, 1, &cancellation),
            Err(GraphLoadError::Canceled)
        ));
    }

    #[test]
    fn cat_file_length_framing_keeps_hostile_subject_bytes_inside_one_record() {
        let object = b"tree 0000000000000000000000000000000000000000\nauthor A <a@example.invalid> 1704067200 +0000\ncommitter A <a@example.invalid> 1704067200 +0000\n\nsubject\0bytes\nbody";
        let mut response = format!("{A} commit {}\n", object.len()).into_bytes();
        response.extend_from_slice(object);
        response.push(b'\n');
        let (records, missing) =
            parse_cat_file_metadata(&response, &[ObjectId::parse(A).unwrap()]).unwrap();
        assert!(missing.is_empty());
        assert_eq!(records[0].metadata.subject, b"subject\0bytes");
        assert_eq!(records[0].metadata.author, b"A");
        assert_eq!(
            records[0].metadata.avatar_hash,
            "2f761a0ba6fc6541787d69b3833f6979dadb036ef2c610f73e3ecb0ad843eb76"
        );
        assert_eq!(records[0].metadata.author_time, 1_704_067_200);
    }

    #[test]
    fn cat_file_metadata_rejects_truncated_frames_and_wrong_identities() {
        let expected = ObjectId::parse(A).unwrap();
        let object = b"tree 0000000000000000000000000000000000000000\n\nsubject";
        let mut truncated = format!("{A} commit {}\n", object.len()).into_bytes();
        truncated.extend_from_slice(&object[..object.len() - 1]);
        assert!(parse_cat_file_metadata(&truncated, std::slice::from_ref(&expected)).is_err());

        let mut wrong = format!("{B} commit {}\n", object.len()).into_bytes();
        wrong.extend_from_slice(object);
        wrong.push(b'\n');
        assert!(parse_cat_file_metadata(&wrong, &[expected]).is_err());
    }

    #[test]
    fn cat_file_preflight_rejects_truncated_and_wrong_identity_rows() {
        let expected = ObjectId::parse(A).unwrap();
        assert!(parse_cat_file_preflight(
            format!("{A} commit 42").as_bytes(),
            std::slice::from_ref(&expected),
        )
        .is_err());
        assert!(
            parse_cat_file_preflight(format!("{B} commit 42\n").as_bytes(), &[expected]).is_err()
        );
    }
}
