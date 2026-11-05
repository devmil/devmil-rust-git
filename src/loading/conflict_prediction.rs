//! Advisory merge and rebase simulation using isolated temporary Git object storage.
use super::CommandExecutor;
use crate::activity::{ActivityId, ActivityStatus};
use crate::cancellation::Cancellation;
use crate::process::{GitProcessCommand, ProcessArgument, SemanticTimeout};
use crate::types::{ObjectId, RefId, RepositoryRelativePath, WorktreeId};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fmt, fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
pub const MAX_PREDICTED_CONFLICT_PATHS: usize = 256;
pub const MAX_PREDICTED_REBASE_COMMITS: usize = 256;
const MAX_PREDICTION_DETAIL_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictPredictionKind {
    Merge,
    Rebase,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConflictPredictionTarget {
    Ref(RefId),
    Commit(ObjectId),
}

impl ConflictPredictionTarget {
    #[must_use]
    pub fn argument(&self) -> String {
        match self {
            Self::Ref(reference) => String::from_utf8_lossy(reference.as_bytes()).into_owned(),
            Self::Commit(commit) => commit.to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ConflictPredictionRequest {
    pub repository: WorktreeId,
    pub kind: ConflictPredictionKind,
    pub target: ConflictPredictionTarget,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PredictionCompleteness {
    Complete,
    Incomplete { reason: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictPrediction {
    pub kind: ConflictPredictionKind,
    pub target: ConflictPredictionTarget,
    pub conflicting_paths: Vec<RepositoryRelativePath>,
    pub analyzed_commits: usize,
    pub total_commits: usize,
    pub completeness: PredictionCompleteness,
    pub activities: Vec<ActivityId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictPredictionErrorKind {
    InvalidTarget,
    Unsupported,
    Git,
    MalformedOutput,
    Canceled,
    ResourceLimit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictPredictionError {
    kind: ConflictPredictionErrorKind,
    detail: String,
}

impl ConflictPredictionError {
    #[must_use]
    pub fn new(kind: ConflictPredictionErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: bound_detail(detail.into()),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> ConflictPredictionErrorKind {
        self.kind
    }

    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for ConflictPredictionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for ConflictPredictionError {}

fn bound_detail(mut detail: String) -> String {
    if detail.len() <= MAX_PREDICTION_DETAIL_BYTES {
        return detail;
    }
    let mut boundary = MAX_PREDICTION_DETAIL_BYTES;
    while !detail.is_char_boundary(boundary) {
        boundary -= 1;
    }
    detail.truncate(boundary);
    detail
}

#[derive(Clone, Debug)]
pub struct ConflictPredictionLoader {
    executable: OsString,
}
impl ConflictPredictionLoader {
    pub fn new(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
        }
    }
    /// Simulates the requested operation without changing repository refs, index or worktree.
    /// # Errors
    /// Returns cancellation, unsupported Git, malformed output, resource or process errors.
    pub fn predict(
        &self,
        request: &ConflictPredictionRequest,
        cancellation: &dyn Cancellation,
        executor: &mut dyn CommandExecutor,
    ) -> Result<ConflictPrediction, ConflictPredictionError> {
        Session {
            executable: self.executable.clone(),
            executor: std::cell::RefCell::new(executor),
        }
        .predict(request, cancellation)
    }
}
const OUTPUT_LIMIT: usize = 2 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

struct Session<'a> {
    executable: OsString,
    executor: std::cell::RefCell<&'a mut dyn CommandExecutor>,
}
impl Session<'_> {
    fn run(
        &self,
        worktree: &Path,
        arguments: impl IntoIterator<Item = OsString>,
        isolation: Option<&ObjectIsolation>,
        stdin: Option<Vec<u8>>,
        cancellation: &dyn Cancellation,
    ) -> Result<CommandResult, ConflictPredictionError> {
        if cancellation.is_cancelled() {
            return Err(canceled());
        }
        let mut command = GitProcessCommand::git(&self.executable)
            .current_dir(worktree)
            .args(arguments.into_iter().map(ProcessArgument::visible))
            .output_limit(OUTPUT_LIMIT)
            .semantic_timeout(
                SemanticTimeout::new(Duration::from_secs(30), "conflict prediction")
                    .expect("constant timeout is valid"),
            )
            .env("LC_ALL", "C")
            .expect("constant environment name is valid");
        if let Some(isolation) = isolation {
            command = command
                .env("GIT_OBJECT_DIRECTORY", &isolation.objects)
                .and_then(|command| {
                    command.env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &isolation.alternate)
                })
                .and_then(|command| command.env("GIT_AUTHOR_NAME", "Git prediction"))
                .and_then(|command| command.env("GIT_AUTHOR_EMAIL", "prediction@example.invalid"))
                .and_then(|command| command.env("GIT_COMMITTER_NAME", "Git prediction"))
                .and_then(|command| {
                    command.env("GIT_COMMITTER_EMAIL", "prediction@example.invalid")
                })
                .map_err(|_| {
                    ConflictPredictionError::new(
                        ConflictPredictionErrorKind::Git,
                        "conflict prediction environment could not be constructed",
                    )
                })?;
        }
        if let Some(stdin) = stdin {
            command = command.stdin_bytes(stdin);
        }
        let mut result = self.executor.borrow_mut().execute(command);
        let id = result.activity.id;
        if result.stdout.truncated || result.stderr.truncated {
            return Err(ConflictPredictionError::new(
                ConflictPredictionErrorKind::ResourceLimit,
                "conflict prediction output exceeded its bound",
            ));
        }
        match result.activity.status {
            ActivityStatus::Canceled { .. } | ActivityStatus::TimedOut { .. } => Err(canceled()),
            ActivityStatus::LaunchError { .. } => Err(ConflictPredictionError::new(
                ConflictPredictionErrorKind::Unsupported,
                "installed Git could not start conflict prediction",
            )),
            status => Ok(CommandResult {
                id,
                status,
                stdout: result.stdout.take_bytes(),
                stderr: result.stderr.take_bytes(),
            }),
        }
    }

    fn resolve_commit(
        &self,
        worktree: &Path,
        value: &str,
        activities: &mut Vec<ActivityId>,
        cancellation: &dyn Cancellation,
    ) -> Result<ObjectId, ConflictPredictionError> {
        let result = self.run(
            worktree,
            [
                OsString::from("rev-parse"),
                OsString::from("--verify"),
                OsString::from(format!("{value}^{{commit}}")),
            ],
            None,
            None,
            cancellation,
        )?;
        activities.push(result.id);
        if !succeeded(&result.status) {
            return Err(ConflictPredictionError::new(
                ConflictPredictionErrorKind::InvalidTarget,
                "prediction target is not a commit",
            ));
        }
        parse_oid(&result.stdout)
    }

    fn predict_merge(
        &self,
        request: &ConflictPredictionRequest,
        isolation: &ObjectIsolation,
        activities: &mut Vec<ActivityId>,
        cancellation: &dyn Cancellation,
    ) -> Result<ConflictPrediction, ConflictPredictionError> {
        let worktree = request.repository.worktree_root().as_path();
        let target = self.resolve_commit(
            worktree,
            &request.target.argument(),
            activities,
            cancellation,
        )?;
        let result = self.run(
            worktree,
            [
                OsString::from("merge-tree"),
                OsString::from("--write-tree"),
                OsString::from("--name-only"),
                OsString::from("--messages"),
                OsString::from("-z"),
                OsString::from("HEAD"),
                OsString::from(target.as_str()),
            ],
            Some(isolation),
            None,
            cancellation,
        )?;
        activities.push(result.id);
        let mut conflicts = parse_merge_tree_paths(&result)?;
        let completeness = if conflicts.len() > MAX_PREDICTED_CONFLICT_PATHS {
            conflicts.truncate(MAX_PREDICTED_CONFLICT_PATHS);
            PredictionCompleteness::Incomplete {
                reason: format!(
                    "conflict path output was limited to {MAX_PREDICTED_CONFLICT_PATHS} files"
                ),
            }
        } else {
            PredictionCompleteness::Complete
        };
        Ok(ConflictPrediction {
            kind: request.kind,
            target: request.target.clone(),
            conflicting_paths: conflicts,
            analyzed_commits: 1,
            total_commits: 1,
            completeness,
            activities: activities.clone(),
        })
    }

    fn predict_rebase(
        &self,
        request: &ConflictPredictionRequest,
        isolation: &ObjectIsolation,
        activities: &mut Vec<ActivityId>,
        cancellation: &dyn Cancellation,
    ) -> Result<ConflictPrediction, ConflictPredictionError> {
        let worktree = request.repository.worktree_root().as_path();
        let onto = self.resolve_commit(
            worktree,
            &request.target.argument(),
            activities,
            cancellation,
        )?;
        let range = format!("{}..HEAD", onto.as_str());
        let commits_result = self.run(
            worktree,
            [
                OsString::from("rev-list"),
                OsString::from("--reverse"),
                OsString::from("--topo-order"),
                OsString::from(range),
            ],
            None,
            None,
            cancellation,
        )?;
        activities.push(commits_result.id);
        if !succeeded(&commits_result.status) {
            return git_failure("Git could not enumerate commits for rebase prediction");
        }
        let commits = parse_oid_lines(&commits_result.stdout)?;
        if commits.len() > MAX_PREDICTED_REBASE_COMMITS {
            return Err(ConflictPredictionError::new(
                ConflictPredictionErrorKind::ResourceLimit,
                format!("rebase prediction is limited to {MAX_PREDICTED_REBASE_COMMITS} commits"),
            ));
        }
        let mut current_tree =
            self.resolve_tree(worktree, onto.as_str(), isolation, activities, cancellation)?;
        let mut conflicts = BTreeSet::new();
        let mut analyzed = 0;
        let mut completeness = PredictionCompleteness::Complete;
        for commit in &commits {
            if cancellation.is_cancelled() {
                return Err(canceled());
            }
            let parent = self.first_parent(worktree, commit, activities, cancellation)?;
            let virtual_commit = self.virtual_commit(
                worktree,
                &current_tree,
                &parent,
                isolation,
                activities,
                cancellation,
            )?;
            let result = self.run(
                worktree,
                [
                    OsString::from("merge-tree"),
                    OsString::from("--write-tree"),
                    OsString::from("--name-only"),
                    OsString::from("--messages"),
                    OsString::from("-z"),
                    OsString::from(virtual_commit.as_str()),
                    OsString::from(commit.as_str()),
                ],
                Some(isolation),
                None,
                cancellation,
            )?;
            activities.push(result.id);
            let step_conflicts = parse_merge_tree_paths(&result)?;
            analyzed += 1;
            conflicts.extend(step_conflicts);
            if !conflicts.is_empty() {
                if analyzed < commits.len() {
                    completeness = PredictionCompleteness::Incomplete {
                        reason: "prediction stopped after the first conflicting rebase step".into(),
                    };
                }
                break;
            }
            current_tree = parse_first_oid(&result.stdout)?;
        }
        let mut conflicting_paths = conflicts.into_iter().collect::<Vec<_>>();
        if conflicting_paths.len() > MAX_PREDICTED_CONFLICT_PATHS {
            conflicting_paths.truncate(MAX_PREDICTED_CONFLICT_PATHS);
            completeness = PredictionCompleteness::Incomplete {
                reason: format!(
                    "conflict path output was limited to {MAX_PREDICTED_CONFLICT_PATHS} files"
                ),
            };
        }
        Ok(ConflictPrediction {
            kind: request.kind,
            target: request.target.clone(),
            conflicting_paths,
            analyzed_commits: analyzed,
            total_commits: commits.len(),
            completeness,
            activities: activities.clone(),
        })
    }

    fn resolve_tree(
        &self,
        worktree: &Path,
        commit: &str,
        isolation: &ObjectIsolation,
        activities: &mut Vec<ActivityId>,
        cancellation: &dyn Cancellation,
    ) -> Result<ObjectId, ConflictPredictionError> {
        let result = self.run(
            worktree,
            [
                OsString::from("rev-parse"),
                OsString::from(format!("{commit}^{{tree}}")),
            ],
            Some(isolation),
            None,
            cancellation,
        )?;
        activities.push(result.id);
        if !succeeded(&result.status) {
            return git_failure("Git could not resolve a tree for prediction");
        }
        parse_oid(&result.stdout)
    }

    fn first_parent(
        &self,
        worktree: &Path,
        commit: &ObjectId,
        activities: &mut Vec<ActivityId>,
        cancellation: &dyn Cancellation,
    ) -> Result<ObjectId, ConflictPredictionError> {
        let result = self.run(
            worktree,
            [
                OsString::from("rev-parse"),
                OsString::from(format!("{}^", commit.as_str())),
            ],
            None,
            None,
            cancellation,
        )?;
        activities.push(result.id);
        if !succeeded(&result.status) {
            return Err(ConflictPredictionError::new(
                ConflictPredictionErrorKind::Unsupported,
                "root commits cannot be replayed by advisory rebase prediction",
            ));
        }
        parse_oid(&result.stdout)
    }

    fn virtual_commit(
        &self,
        worktree: &Path,
        tree: &ObjectId,
        parent: &ObjectId,
        isolation: &ObjectIsolation,
        activities: &mut Vec<ActivityId>,
        cancellation: &dyn Cancellation,
    ) -> Result<ObjectId, ConflictPredictionError> {
        let result = self.run(
            worktree,
            [
                OsString::from("commit-tree"),
                OsString::from(tree.as_str()),
                OsString::from("-p"),
                OsString::from(parent.as_str()),
            ],
            Some(isolation),
            Some(b"Git conflict prediction\n".to_vec()),
            cancellation,
        )?;
        activities.push(result.id);
        if !succeeded(&result.status) {
            return git_failure("Git could not construct an isolated prediction commit");
        }
        parse_oid(&result.stdout)
    }
}

impl Session<'_> {
    fn predict(
        &self,
        request: &ConflictPredictionRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<ConflictPrediction, ConflictPredictionError> {
        if cancellation.is_cancelled() {
            return Err(canceled());
        }
        let isolation = ObjectIsolation::new(&request.repository)?;
        let mut activities = Vec::new();
        match request.kind {
            ConflictPredictionKind::Merge => {
                self.predict_merge(request, &isolation, &mut activities, cancellation)
            }
            ConflictPredictionKind::Rebase => {
                self.predict_rebase(request, &isolation, &mut activities, cancellation)
            }
        }
    }
}

struct CommandResult {
    id: ActivityId,
    status: ActivityStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

struct ObjectIsolation {
    root: PathBuf,
    objects: PathBuf,
    alternate: PathBuf,
}

impl ObjectIsolation {
    fn new(repository: &crate::types::WorktreeId) -> Result<Self, ConflictPredictionError> {
        for _ in 0..16 {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "devmil-conflict-prediction-{}-{sequence}",
                std::process::id()
            ));
            match create_private_directory(&root) {
                Ok(()) => {
                    let objects = root.join("objects");
                    create_private_directory(&objects).map_err(io_failure)?;
                    ensure_plain_directory(&root)?;
                    ensure_plain_directory(&objects)?;
                    return Ok(Self {
                        root,
                        objects,
                        alternate: repository
                            .repository()
                            .common_git_dir()
                            .as_path()
                            .join("objects"),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(io_failure(error)),
            }
        }
        Err(ConflictPredictionError::new(
            ConflictPredictionErrorKind::ResourceLimit,
            "could not allocate isolated prediction storage",
        ))
    }
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    // The platform temporary-directory ACL is the available privacy boundary. Creation remains
    // atomic and rejects an existing file, directory, or symlink.
    fs::create_dir(path)
}

fn ensure_plain_directory(path: &Path) -> Result<(), ConflictPredictionError> {
    let metadata = fs::symlink_metadata(path).map_err(io_failure)?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(ConflictPredictionError::new(
            ConflictPredictionErrorKind::ResourceLimit,
            "prediction storage is not a private directory",
        ))
    }
}

impl Drop for ObjectIsolation {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn parse_merge_tree_paths(
    result: &CommandResult,
) -> Result<Vec<RepositoryRelativePath>, ConflictPredictionError> {
    if !matches!(
        result.status,
        ActivityStatus::Succeeded { .. } | ActivityStatus::Failed { exit_code: Some(1) }
    ) {
        if contains(&result.stderr, b"unknown option")
            || contains(&result.stderr, b"unknown switch")
        {
            return Err(ConflictPredictionError::new(
                ConflictPredictionErrorKind::Unsupported,
                "installed Git does not support non-mutating merge-tree prediction",
            ));
        }
        return git_failure("Git could not calculate conflict prediction");
    }
    crate::history::merge_tree::parse_conflict_paths(&result.stdout, MAX_PREDICTED_CONFLICT_PATHS)
        .map_err(|error| {
            ConflictPredictionError::new(
                ConflictPredictionErrorKind::MalformedOutput,
                error.to_string(),
            )
        })
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|candidate| candidate == needle)
}

fn parse_oid(bytes: &[u8]) -> Result<ObjectId, ConflictPredictionError> {
    parse_first_oid(bytes)
}

fn parse_first_oid(bytes: &[u8]) -> Result<ObjectId, ConflictPredictionError> {
    crate::history::merge_tree::parse_first_oid(bytes).map_err(|error| {
        ConflictPredictionError::new(
            ConflictPredictionErrorKind::MalformedOutput,
            error.to_string(),
        )
    })
}

fn parse_oid_lines(bytes: &[u8]) -> Result<Vec<ObjectId>, ConflictPredictionError> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            std::str::from_utf8(line)
                .map_err(|_| malformed_oid())
                .and_then(|line| ObjectId::parse(line.trim()).map_err(|_| malformed_oid()))
        })
        .collect()
}

fn malformed_oid() -> ConflictPredictionError {
    ConflictPredictionError::new(
        ConflictPredictionErrorKind::MalformedOutput,
        "Git returned a malformed object identity during conflict prediction",
    )
}

fn succeeded(status: &ActivityStatus) -> bool {
    matches!(status, ActivityStatus::Succeeded { .. })
}

fn canceled() -> ConflictPredictionError {
    ConflictPredictionError::new(
        ConflictPredictionErrorKind::Canceled,
        "conflict prediction was canceled",
    )
}

fn git_failure<T>(detail: &str) -> Result<T, ConflictPredictionError> {
    Err(ConflictPredictionError::new(
        ConflictPredictionErrorKind::Git,
        detail,
    ))
}

#[allow(clippy::needless_pass_by_value)] // `map_err` transfers the concrete I/O error here.
fn io_failure(error: std::io::Error) -> ConflictPredictionError {
    ConflictPredictionError::new(
        ConflictPredictionErrorKind::ResourceLimit,
        format!("prediction storage failed: {error}"),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn error_details_are_utf8_bounded() {
        let error = super::ConflictPredictionError::new(
            super::ConflictPredictionErrorKind::Git,
            "é".repeat(super::MAX_PREDICTION_DETAIL_BYTES),
        );
        assert!(error.detail().len() <= super::MAX_PREDICTION_DETAIL_BYTES);
        assert!(error.detail().is_char_boundary(error.detail().len()));
    }
    use super::ObjectIsolation;
    use crate::types::{RepositoryId, RepositoryPath, WorktreeId};

    #[test]
    fn isolated_object_storage_is_removed_on_drop() {
        let temporary = std::env::temp_dir().join(format!(
            "devmil-prediction-storage-test-{}",
            super::TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&temporary).unwrap();
        let worktree_path = RepositoryPath::new(temporary.clone()).unwrap();
        let git_path = RepositoryPath::new(temporary.join(".git")).unwrap();
        std::fs::create_dir(git_path.as_path()).unwrap();
        let worktree =
            WorktreeId::new(RepositoryId::new(git_path.clone()), worktree_path, git_path);

        let isolation = ObjectIsolation::new(&worktree).unwrap();
        let root = isolation.root.clone();
        assert!(root.is_dir());
        drop(isolation);
        assert!(!root.exists());
        std::fs::remove_dir_all(&temporary).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn isolated_object_storage_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = std::env::temp_dir().join(format!(
            "devmil-prediction-permission-test-{}",
            super::TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&temporary).unwrap();
        let worktree_path = RepositoryPath::new(temporary.clone()).unwrap();
        let git_path = RepositoryPath::new(temporary.join(".git")).unwrap();
        std::fs::create_dir(git_path.as_path()).unwrap();
        let worktree =
            WorktreeId::new(RepositoryId::new(git_path.clone()), worktree_path, git_path);

        let isolation = ObjectIsolation::new(&worktree).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&isolation.root)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::symlink_metadata(&isolation.objects)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        drop(isolation);
        std::fs::remove_dir_all(&temporary).unwrap();
    }
}
