//! Cached, typed repository pages. Query identities contain every Git argument affecting a read.
use super::{ReadTicket, Repository, RepositoryError, RepositoryErrorKind};
use crate::diff::model::DiffPath;
use crate::loading::{blame, path_history, reflog, tree};
use crate::types::ObjectId;

#[derive(Clone, Debug)]
pub struct TreeQuery {
    pub commit: ObjectId,
    pub directory: Option<DiffPath>,
}
#[derive(Clone, Debug)]
pub struct PathHistoryQuery {
    pub revision: Option<ObjectId>,
    pub path: DiffPath,
    pub kind: path_history::PathHistoryKind,
    pub offset: usize,
    pub limit: usize,
    pub rename_hops_before: usize,
}
#[derive(Clone, Debug)]
pub struct RangeHistoryQuery {
    pub revision: ObjectId,
    pub path: DiffPath,
    pub first_line: u32,
    pub last_line: u32,
    pub limit: usize,
}
#[derive(Clone, Debug)]
pub struct BlameQuery {
    pub revision: blame::BlameRevision,
    pub path: DiffPath,
    pub options: blame::BlameOptions,
}
fn field(key: &mut Vec<u8>, value: &[u8]) {
    key.extend_from_slice(&(value.len() as u64).to_be_bytes());
    key.extend_from_slice(value);
}
fn number(key: &mut Vec<u8>, value: usize) {
    field(key, &value.to_be_bytes());
}

impl Repository {
    /// Loads an immutable directory snapshot. Paginate with its `page` method without more Git calls.
    /// # Errors
    /// Returns a queue or repository lifecycle error.
    pub fn tree(
        &self,
        query: TreeQuery,
    ) -> Result<ReadTicket<tree::CommitTreeSnapshot>, RepositoryError> {
        let mut key = b"tree-v1".to_vec();
        field(&mut key, query.commit.as_str().as_bytes());
        field(
            &mut key,
            query.directory.as_ref().map_or(&[], DiffPath::as_bytes),
        );
        let executable = self.shared.options.git_executable.clone();
        self.schedule_weighted(key, true, tree_weight, move |repository, executor, _| {
            tree::load_tree(
                &executable,
                repository.worktree.worktree_root(),
                &query.commit,
                query.directory.as_ref(),
                executor,
            )
            .map_err(|error| tree_failure(&error))
        })
    }
    /// Loads a page keyed by revision, byte path, traversal kind and complete rename cursor.
    /// # Errors
    /// Returns a queue or repository lifecycle error.
    pub fn path_history(
        &self,
        query: PathHistoryQuery,
    ) -> Result<ReadTicket<path_history::PathHistoryPage>, RepositoryError> {
        let mut key = b"path-history-v1".to_vec();
        field(
            &mut key,
            query
                .revision
                .as_ref()
                .map_or(&[], |oid| oid.as_str().as_bytes()),
        );
        field(&mut key, query.path.as_bytes());
        field(
            &mut key,
            &[u8::from(query.kind == path_history::PathHistoryKind::File)],
        );
        number(&mut key, query.offset);
        number(&mut key, query.limit);
        number(&mut key, query.rename_hops_before);
        let immutable = query.revision.is_some();
        let executable = self.shared.options.git_executable.clone();
        self.schedule_weighted(
            key,
            immutable,
            path_weight,
            move |repository, executor, _| {
                let request = path_history::PathHistoryQuery {
                    repository: repository.worktree.worktree_root().clone(),
                    revision: query.revision,
                    path: query.path,
                    kind: query.kind,
                    offset: query.offset,
                    limit: query.limit,
                    rename_hops_before: query.rename_hops_before,
                };
                path_history::load(&executable, &request, executor)
                    .map_err(|error| path_failure(&error))
            },
        )
    }
    /// Loads committed line history, coalescing identical requests.
    /// # Errors
    /// Returns a queue or repository lifecycle error.
    pub fn range_history(
        &self,
        query: RangeHistoryQuery,
    ) -> Result<ReadTicket<tree::RangeHistoryResult>, RepositoryError> {
        let mut key = b"range-history-v1".to_vec();
        field(&mut key, query.revision.as_str().as_bytes());
        field(&mut key, query.path.as_bytes());
        field(&mut key, &query.first_line.to_be_bytes());
        field(&mut key, &query.last_line.to_be_bytes());
        number(&mut key, query.limit);
        let executable = self.shared.options.git_executable.clone();
        self.schedule_weighted(key, true, range_weight, move |repository, executor, _| {
            let request = tree::RangeHistoryQuery {
                repository: repository.worktree.worktree_root().clone(),
                revision: query.revision,
                path: query.path,
                first_line: query.first_line,
                last_line: query.last_line,
                limit: query.limit,
            };
            tree::load_range(&executable, &request, executor).map_err(|error| tree_failure(&error))
        })
    }
    /// Loads line attribution. Commit, byte path and all ancestry options participate in caching.
    /// # Errors
    /// Returns a queue or repository lifecycle error.
    pub fn blame(
        &self,
        query: BlameQuery,
    ) -> Result<ReadTicket<blame::BlameResult>, RepositoryError> {
        let mut key = b"blame-v1".to_vec();
        let revision = match &query.revision {
            blame::BlameRevision::WorkingTree => b"working".as_slice(),
            blame::BlameRevision::Head => b"HEAD".as_slice(),
            blame::BlameRevision::Commit(oid) => oid.as_str().as_bytes(),
        };
        field(&mut key, revision);
        field(&mut key, query.path.as_bytes());
        field(
            &mut key,
            &[
                u8::from(query.options.detect_moves),
                u8::from(query.options.detect_copies),
            ],
        );
        if let Some(range) = query.options.range {
            field(&mut key, &range.first.to_be_bytes());
            field(&mut key, &range.last.to_be_bytes());
        }
        let immutable = matches!(query.revision, blame::BlameRevision::Commit(_));
        let executable = self.shared.options.git_executable.clone();
        self.schedule_weighted(
            key,
            immutable,
            blame_weight,
            move |repository, executor, _| {
                let request = blame::BlameQuery {
                    repository: repository.worktree.worktree_root().clone(),
                    revision: query.revision,
                    path: query.path,
                    options: query.options,
                };
                blame::load(&executable, &request, executor).map_err(|error| blame_failure(&error))
            },
        )
    }
    /// Loads a bounded mutable reflog page.
    /// # Errors
    /// Returns a queue or repository lifecycle error.
    pub fn reflog(
        &self,
        query: reflog::ReflogQuery,
    ) -> Result<ReadTicket<reflog::ReflogPage>, RepositoryError> {
        let mut key = b"reflog-v1".to_vec();
        number(&mut key, query.offset);
        number(&mut key, query.limit);
        let executable = self.shared.options.git_executable.clone();
        self.schedule_weighted(
            key,
            false,
            reflog_weight,
            move |repository, executor, cancellation| {
                reflog::load(
                    &executable,
                    repository.worktree.worktree_root(),
                    query,
                    cancellation,
                    executor,
                )
                .map_err(|error| reflog_failure(&error))
            },
        )
    }
}

fn allocation<T>(values: &Vec<T>) -> usize {
    values.capacity().saturating_mul(std::mem::size_of::<T>())
}
fn sum(values: impl IntoIterator<Item = usize>) -> usize {
    values.into_iter().fold(0, usize::saturating_add)
}
pub(super) fn activities(records: &Vec<crate::activity::ActivityRecord>) -> usize {
    allocation(records).saturating_add(sum(records.iter().map(activity)))
}
pub(super) fn activity(record: &crate::activity::ActivityRecord) -> usize {
    let command = &record.command;
    sum([
        command.executable.capacity(),
        allocation(&command.arguments),
        sum(command.arguments.iter().map(|arg| arg.display.capacity())),
        allocation(&command.environment),
        sum(command
            .environment
            .iter()
            .map(|env| env.name.capacity().saturating_add(env.display.capacity()))),
        command
            .working_directory
            .as_ref()
            .map_or(0, String::capacity),
        record.stdout.bytes.capacity(),
        record.stderr.bytes.capacity(),
        record
            .last_progress
            .as_ref()
            .map_or(0, |p| p.phase.capacity().saturating_add(p.unit.capacity())),
        match &record.status {
            crate::activity::ActivityStatus::TimedOut { semantic, .. } => semantic.capacity(),
            crate::activity::ActivityStatus::LaunchError { message } => message.capacity(),
            _ => 0,
        },
    ])
}
fn tree_weight(value: &tree::CommitTreeSnapshot) -> usize {
    sum([
        std::mem::size_of_val(value),
        allocation(&value.entries),
        activities(&value.activities),
        sum(value.entries.iter().map(|row| {
            row.path
                .as_bytes()
                .len()
                .saturating_mul(2)
                .saturating_add(128)
        })),
    ])
}
fn path_weight(value: &path_history::PathHistoryPage) -> usize {
    sum([
        std::mem::size_of_val(value),
        allocation(&value.entries),
        activities(&value.activities),
        value.incomplete_reason.as_ref().map_or(0, String::capacity),
        sum(value.entries.iter().map(|row| {
            sum([
                128,
                row.author.capacity(),
                row.summary.capacity(),
                row.path.as_bytes().len().saturating_mul(2),
                row.old_path
                    .as_ref()
                    .map_or(0, |p| p.as_bytes().len().saturating_mul(2)),
            ])
        })),
    ])
}
fn range_weight(value: &tree::RangeHistoryResult) -> usize {
    sum([
        std::mem::size_of_val(value),
        allocation(&value.entries),
        activities(&value.activities),
        sum(value
            .entries
            .iter()
            .map(|row| sum([128, row.author.capacity(), row.summary.capacity()]))),
    ])
}
fn attribution_weight(value: &blame::BlameAttribution) -> usize {
    sum([
        128,
        value.author.capacity(),
        value.author_mail.capacity(),
        value.summary.capacity(),
        value.original_path.as_bytes().len().saturating_mul(2),
    ])
}
fn blame_weight(value: &blame::BlameResult) -> usize {
    sum([
        std::mem::size_of_val(value),
        allocation(&value.lines),
        allocation(&value.spans),
        activities(&value.activities),
        sum(value.lines.iter().map(|line| {
            line.content
                .capacity()
                .saturating_add(attribution_weight(&line.attribution))
        })),
        sum(value
            .spans
            .iter()
            .map(|span| attribution_weight(&span.attribution))),
    ])
}
fn reflog_weight(value: &reflog::ReflogPage) -> usize {
    sum([
        std::mem::size_of_val(value),
        allocation(&value.entries),
        sum(value.entries.iter().map(|row| {
            sum([
                128,
                row.reference().len(),
                row.summary().len(),
                row.actor_name().len(),
                row.actor_email().len(),
                row.timezone().len(),
            ])
            .saturating_mul(2)
        })),
    ])
}

fn tree_failure(error: &tree::RichHistoryError) -> RepositoryError {
    let kind = match error.kind() {
        tree::RichHistoryErrorKind::InvalidRequest => RepositoryErrorKind::InvalidOptions,
        tree::RichHistoryErrorKind::Canceled => RepositoryErrorKind::Canceled,
        tree::RichHistoryErrorKind::TooLarge => RepositoryErrorKind::TooLarge,
        tree::RichHistoryErrorKind::Parse => RepositoryErrorKind::InvalidOutput,
        _ => RepositoryErrorKind::Read,
    };
    RepositoryError::new(kind, error.to_string())
}
fn path_failure(error: &path_history::PathHistoryError) -> RepositoryError {
    let kind = match error.kind() {
        path_history::PathHistoryErrorKind::InvalidRequest => RepositoryErrorKind::InvalidOptions,
        path_history::PathHistoryErrorKind::Canceled => RepositoryErrorKind::Canceled,
        path_history::PathHistoryErrorKind::TooLarge => RepositoryErrorKind::TooLarge,
        path_history::PathHistoryErrorKind::Parse => RepositoryErrorKind::InvalidOutput,
        path_history::PathHistoryErrorKind::GitFailed => RepositoryErrorKind::Read,
    };
    RepositoryError::new(kind, error.to_string())
}
fn blame_failure(error: &blame::BlameError) -> RepositoryError {
    let kind = match error.kind() {
        blame::BlameErrorKind::InvalidRequest => RepositoryErrorKind::InvalidOptions,
        blame::BlameErrorKind::Canceled => RepositoryErrorKind::Canceled,
        blame::BlameErrorKind::TooLarge => RepositoryErrorKind::TooLarge,
        blame::BlameErrorKind::Parse => RepositoryErrorKind::InvalidOutput,
        blame::BlameErrorKind::GitFailed => RepositoryErrorKind::Read,
    };
    RepositoryError::new(kind, error.to_string())
}
fn reflog_failure(error: &reflog::ReflogFailure) -> RepositoryError {
    let kind = match error.kind() {
        reflog::ReflogFailureKind::InvalidRequest => RepositoryErrorKind::InvalidOptions,
        reflog::ReflogFailureKind::Canceled => RepositoryErrorKind::Canceled,
        reflog::ReflogFailureKind::TooLarge => RepositoryErrorKind::TooLarge,
        reflog::ReflogFailureKind::MalformedOutput => RepositoryErrorKind::InvalidOutput,
        reflog::ReflogFailureKind::ProcessFailed => RepositoryErrorKind::Read,
    };
    RepositoryError::new(kind, error.to_string()).with_activity(error.activity_id())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cached_directory_pages_invoke_ls_tree_once() {
        use crate::activity::ActivityEvent;
        use crate::model::RepositoryOptions;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use std::time::{Duration, SystemTime, UNIX_EPOCH};
        let path = std::env::temp_dir().join(format!(
            "devmil-tree-pages-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        let git = |args: &[&str]| {
            let result = std::process::Command::new("git")
                .args(args)
                .current_dir(&path)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            result.stdout
        };
        git(&["init", "-b", "main"]);
        std::fs::write(path.join("one"), b"one").unwrap();
        std::fs::write(path.join("two"), b"two").unwrap();
        git(&["add", "--", "one", "two"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "base",
        ]);
        let commit = ObjectId::parse(
            String::from_utf8(git(&["rev-parse", "HEAD"]))
                .unwrap()
                .trim(),
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&calls);
        let repository=Repository::new(&path,RepositoryOptions{watch:false,activity_sink:Some(Arc::new(move |event|{
            if matches!(event,ActivityEvent::Created(record) if record.command.arguments.first().is_some_and(|arg|arg.display=="ls-tree")){count.fetch_add(1,Ordering::Relaxed);}
        })),..RepositoryOptions::default()}).unwrap();
        let query = TreeQuery {
            commit,
            directory: None,
        };
        let first = repository
            .tree(query.clone())
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert_eq!(first.page(0, 1).unwrap().next_offset, Some(1));
        assert_eq!(first.page(1, 1).unwrap().next_offset, None);
        let second = repository
            .tree(query)
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        repository.close();
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn byte_key_framing_disambiguates_adjacent_fields() {
        let mut first = Vec::new();
        field(&mut first, b"a");
        field(&mut first, b"bc");
        let mut second = Vec::new();
        field(&mut second, b"ab");
        field(&mut second, b"c");
        assert_ne!(first, second);
    }
}
