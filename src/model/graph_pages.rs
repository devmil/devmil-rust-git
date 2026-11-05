//! Repository-owned frozen history sessions.
use super::{lock, ReadTicket, Repository, RepositoryError, RepositoryErrorKind, Shared};
use crate::cache::CacheLimit;
use crate::loading::graph::{
    GraphHistoryOrder, GraphLoader, GraphMetadataStatus, GraphRequest, GraphTraversalScope,
};
use crate::loading::graph_search::{
    GraphPathQuery, PathSearchLoader, PathSearchRequest, PathSearchResult,
};
use crate::loading::history_index::{
    HistoryFilter, HistoryIndex, HistoryPage, HistoryPageToken, HistorySearchScope,
};
use crate::types::ObjectId;
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

static NEXT_QUERY: AtomicU64 = AtomicU64::new(1);

pub(super) struct HistorySession {
    index: HistoryIndex,
    metadata_complete: bool,
    metadata_loaded: usize,
    paths: BTreeMap<String, std::collections::BTreeSet<ObjectId>>,
    charge: usize,
}

#[derive(Default)]
pub(super) struct HistorySessions {
    sessions: BTreeMap<(GraphHistoryOrder, GraphTraversalScope), HistorySession>,
}

pub(super) fn invalidate_indices(shared: &Shared) {
    lock(&shared.history_indices).sessions.clear();
}

impl Repository {
    /// Searches paths reachable from immutable frozen tips; repeated requests share cached results.
    /// # Errors
    /// Rejects invalid requests and queue/lifecycle failures. Git failures arrive through the ticket.
    pub fn search_history_paths(
        &self,
        roots: Vec<ObjectId>,
        query: GraphPathQuery,
        maximum_matches: usize,
    ) -> Result<ReadTicket<PathSearchResult>, RepositoryError> {
        if roots.is_empty()
            || roots.len() > crate::loading::graph_search::MAX_PATH_SEARCH_ROOTS
            || maximum_matches == 0
            || maximum_matches > 100_000
        {
            return Err(RepositoryError::new(
                RepositoryErrorKind::InvalidOptions,
                "invalid frozen path search request",
            ));
        }
        let executable = self.shared.options.git_executable.clone();
        let key = format!("history-path:{roots:?}:{query:?}:{maximum_matches}").into_bytes();
        self.schedule_weighted(
            key,
            true,
            |result: &PathSearchResult| result.matches.len().saturating_mul(256),
            move |repository, executor, cancellation| {
                PathSearchLoader::new(executable)
                    .load(
                        &PathSearchRequest {
                            worktree: repository.worktree.clone(),
                            topology_roots: roots,
                            query,
                            maximum_matches,
                        },
                        cancellation,
                        executor,
                    )
                    .map_err(|error| path_error(&error))
            },
        )
    }

    /// Returns a filtered page from a repository-owned frozen history index.
    /// Continuations retain the same topology; ref/config refresh invalidates their tokens.
    /// Message/author searches hydrate all commits in bounded batches before matching;
    /// path searches resolve frozen tips once per distinct text. Incomplete search data
    /// returns an error instead of silently omitting matches.
    /// # Errors
    /// Rejects invalid limits/searches, stale tokens, cache capacity overflow, cancellation,
    /// and incomplete Git results. Dropping the ticket never closes the repository.
    pub fn history_page(
        &self,
        order: GraphHistoryOrder,
        scope: GraphTraversalScope,
        filter: HistoryFilter,
        limit: usize,
        token: Option<HistoryPageToken>,
    ) -> Result<ReadTicket<HistoryPage>, RepositoryError> {
        let key = page_key(&filter, limit)?;
        let shared = Arc::clone(&self.shared);
        self.schedule(
            key,
            usize::MAX,
            false,
            move |repository, executor, cancellation| {
                let mut sessions = lock(&shared.history_indices);
                let session_key = (order, scope);
                let budget = shared.options.cache_bytes - shared.options.cache_bytes / 2;
                if !sessions.sessions.contains_key(&session_key) {
                    if token.is_some() {
                        return Err(RepositoryError::new(
                            RepositoryErrorKind::InvalidOptions,
                            "history page token is stale",
                        ));
                    }
                    let topology = GraphLoader::new(shared.options.git_executable.clone())
                        .with_known_version(repository.git_version)
                        .load(
                            &GraphRequest {
                                worktree: repository.worktree.clone(),
                                order,
                                scope,
                            },
                            cancellation,
                            executor,
                        )
                        .map_err(|e| super::graph::graph_error(&e))?;
                    let base_charge = super::graph::topology_bytes(&topology)
                        .saturating_add(topology.rows.len().saturating_mul(320));
                    let filter_budget = (budget.saturating_sub(base_charge)).min(1024 * 1024);
                    let charge = base_charge.saturating_add(filter_budget);
                    if charge > budget || filter_budget == 0 {
                        return Err(RepositoryError::new(
                            RepositoryErrorKind::Capacity,
                            "history index exceeds repository cache budget",
                        ));
                    }
                    while sessions.sessions.len() >= shared.options.cache_entries
                        || sessions
                            .sessions
                            .values()
                            .fold(charge, |sum, s| sum.saturating_add(s.charge))
                            > budget
                    {
                        sessions.sessions.pop_first();
                    }
                    sessions.sessions.insert(
                        session_key,
                        HistorySession {
                            index: HistoryIndex::new(
                                Arc::new(topology),
                                CacheLimit {
                                    entries: 32,
                                    bytes: filter_budget,
                                },
                            ),
                            metadata_complete: false,
                            metadata_loaded: 0,
                            paths: BTreeMap::new(),
                            charge,
                        },
                    );
                }
                let other_charge = sessions
                    .sessions
                    .iter()
                    .filter(|(key, _)| **key != session_key)
                    .fold(0usize, |sum, (_, session)| {
                        sum.saturating_add(session.charge)
                    });
                let budget = budget.saturating_sub(other_charge);
                let session = sessions.sessions.get_mut(&session_key).ok_or_else(|| {
                    RepositoryError::new(RepositoryErrorKind::Read, "history index is unavailable")
                })?;
                session.hydrate_search(
                    &shared.options.git_executable,
                    repository,
                    &filter,
                    budget,
                    executor,
                    cancellation,
                )?;
                let mut filter = filter;
                session.resolve_paths(
                    &shared.options.git_executable,
                    repository,
                    &mut filter,
                    budget,
                    executor,
                    cancellation,
                )?;
                session
                    .index
                    .query(&filter, limit, token.as_ref(), cancellation)
                    .map_err(index_error)
            },
        )
    }
}

fn index_error(error: crate::loading::history_index::HistoryIndexError) -> RepositoryError {
    let kind = if error == crate::loading::history_index::HistoryIndexError::Canceled {
        RepositoryErrorKind::Canceled
    } else {
        RepositoryErrorKind::InvalidOptions
    };
    RepositoryError::new(kind, error.to_string())
}
fn path_error(error: &crate::loading::graph_search::GraphPathSearchFailure) -> RepositoryError {
    use crate::loading::graph_search::GraphPathSearchFailureKind as Kind;
    let kind = match error.kind() {
        Kind::Canceled => RepositoryErrorKind::Canceled,
        Kind::InvalidPath | Kind::InvalidTopology => RepositoryErrorKind::InvalidOptions,
        Kind::MalformedOutput => RepositoryErrorKind::InvalidOutput,
        Kind::ProcessFailed | Kind::Internal => RepositoryErrorKind::Read,
    };
    RepositoryError::new(kind, error.message()).with_activity(error.activity_id())
}

impl HistorySession {
    fn hydrate_search(
        &mut self,
        executable: &std::ffi::OsStr,
        repository: &crate::repository::DiscoveredRepository,
        filter: &HistoryFilter,
        budget: usize,
        executor: &mut dyn crate::loading::CommandExecutor,
        cancellation: &dyn crate::cancellation::Cancellation,
    ) -> Result<(), RepositoryError> {
        let needs_metadata = !filter.text.is_empty()
            && (filter.scopes.contains(&HistorySearchScope::Message)
                || filter.scopes.contains(&HistorySearchScope::Author));
        if needs_metadata && !self.metadata_complete {
            let objects: Vec<_> = self
                .index
                .topology()
                .rows
                .iter()
                .map(|row| row.id.clone())
                .collect();
            for batch in objects[self.metadata_loaded..].chunks(512) {
                let loaded = GraphLoader::new(executable.to_os_string())
                    .with_known_version(repository.git_version)
                    .metadata(&repository.worktree, batch, cancellation, executor)
                    .map_err(|e| super::graph::graph_error(&e))?;
                if !loaded.missing.is_empty() {
                    return Err(RepositoryError::new(
                        RepositoryErrorKind::MissingObject,
                        "history search metadata is missing",
                    ));
                }
                if loaded
                    .records
                    .iter()
                    .any(|record| record.status != GraphMetadataStatus::Available)
                {
                    return Err(RepositoryError::new(
                        RepositoryErrorKind::InvalidOutput,
                        "history search metadata is incomplete",
                    ));
                }
                let weight = loaded.records.iter().fold(0usize, |sum, record| {
                    sum.saturating_add(record.metadata.subject.capacity())
                        .saturating_add(record.metadata.author.capacity())
                        .saturating_add(record.metadata.avatar_hash.capacity())
                        .saturating_add(320)
                });
                if self.charge.saturating_add(weight) > budget {
                    return Err(RepositoryError::new(
                        RepositoryErrorKind::Capacity,
                        "history search metadata exceeds repository cache budget",
                    ));
                }
                self.charge = self.charge.saturating_add(weight);
                self.index.hydrate(loaded.records).map_err(index_error)?;
                self.metadata_loaded += batch.len();
            }
            self.metadata_complete = true;
        }
        Ok(())
    }
    fn resolve_paths(
        &mut self,
        executable: &std::ffi::OsStr,
        repository: &crate::repository::DiscoveredRepository,
        filter: &mut HistoryFilter,
        budget: usize,
        executor: &mut dyn crate::loading::CommandExecutor,
        cancellation: &dyn crate::cancellation::Cancellation,
    ) -> Result<(), RepositoryError> {
        if !filter.text.is_empty() && filter.scopes.contains(&HistorySearchScope::Path) {
            if self.index.topology().rows.is_empty() {
                filter.path_matches.clear();
                return Ok(());
            }
            if !self.paths.contains_key(&filter.text) {
                let loaded = PathSearchLoader::new(executable.to_os_string())
                    .load(
                        &PathSearchRequest {
                            worktree: repository.worktree.clone(),
                            topology_roots: self.index.topology().frozen_tips.clone(),
                            query: GraphPathQuery::DisplaySubstring(filter.text.clone()),
                            maximum_matches: 100_000,
                        },
                        cancellation,
                        executor,
                    )
                    .map_err(|error| path_error(&error))?;
                if loaded.truncated {
                    return Err(RepositoryError::new(
                        RepositoryErrorKind::TooLarge,
                        "history path search exceeds result limit",
                    ));
                }
                let weight = loaded
                    .matches
                    .len()
                    .saturating_mul(256)
                    .saturating_add(filter.text.len());
                if self.charge.saturating_add(weight) > budget {
                    return Err(RepositoryError::new(
                        RepositoryErrorKind::Capacity,
                        "history path matches exceed repository cache budget",
                    ));
                }
                self.charge = self.charge.saturating_add(weight);
                self.paths
                    .insert(filter.text.clone(), loaded.matches.into_iter().collect());
            }
            filter.path_matches = self.paths.get(&filter.text).cloned().unwrap_or_default();
        }
        Ok(())
    }
}

fn page_key(filter: &HistoryFilter, limit: usize) -> Result<Vec<u8>, RepositoryError> {
    if limit == 0
        || limit > crate::loading::history_index::MAX_HISTORY_PAGE_ROWS
        || filter.text.len() > crate::loading::history_index::MAX_HISTORY_SEARCH_BYTES
        || (!filter.text.is_empty() && filter.scopes.is_empty())
        || filter.path_matches.len() > 100_000
        || filter.refs.len() > 16_384
    {
        return Err(RepositoryError::new(
            RepositoryErrorKind::InvalidOptions,
            "invalid history page query",
        ));
    }
    let id = NEXT_QUERY
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        .map_err(|_| {
            RepositoryError::new(RepositoryErrorKind::Capacity, "history query IDs exhausted")
        })?;
    Ok(format!("history-page:{id}").into_bytes())
}
