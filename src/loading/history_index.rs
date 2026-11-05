//! Snapshot-bound history filtering and pagination without additional Git commands.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use super::graph::{GraphCommitMetadata, GraphMetadataRecord, LoadedTopology};
use crate::cache::{CacheLimit, CacheStats, WeightedLru};
use crate::cancellation::Cancellation;
use crate::types::{ObjectId, RefId};

pub const MAX_HISTORY_PAGE_ROWS: usize = 4096;
pub const MAX_HISTORY_SEARCH_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum HistorySearchScope {
    Hash,
    Message,
    Author,
    Path,
}

/// Text scopes are combined with OR. A ref filter intersects the result with the
/// ancestry reachable from those frozen refs. Path matches come from a frozen-root search.
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HistoryFilter {
    pub text: String,
    pub scopes: BTreeSet<HistorySearchScope>,
    pub refs: BTreeSet<RefId>,
    pub path_matches: BTreeSet<ObjectId>,
}

#[derive(Clone, Debug)]
pub struct HistoryPageToken {
    identity: Arc<()>,
    metadata_revision: u64,
    filter: Arc<HistoryFilter>,
    offset: usize,
}

#[derive(Clone, Debug)]
pub struct HistoryPage {
    pub commits: Vec<ObjectId>,
    pub total_matches: usize,
    pub next: Option<HistoryPageToken>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryIndexError {
    InvalidLimit,
    InvalidSearch,
    StaleToken,
    Canceled,
    RevisionExhausted,
}

impl std::fmt::Display for HistoryIndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "history query failed: {self:?}")
    }
}
impl std::error::Error for HistoryIndexError {}

/// Holds one frozen topology, incrementally hydrated metadata, and bounded cached
/// match positions. Pages clone only their requested object IDs.
#[derive(Debug)]
pub struct HistoryIndex {
    topology: Arc<LoadedTopology>,
    identity: Arc<()>,
    metadata: BTreeMap<ObjectId, GraphCommitMetadata>,
    metadata_revision: u64,
    positions: HashMap<ObjectId, usize>,
    matches: WeightedLru<HistoryFilter, Arc<Vec<usize>>>,
}

impl HistoryIndex {
    pub fn new(topology: Arc<LoadedTopology>, cache_limit: CacheLimit) -> Self {
        let positions = topology
            .rows
            .iter()
            .enumerate()
            .map(|(position, row)| (row.id.clone(), position))
            .collect();
        Self {
            topology,
            identity: Arc::new(()),
            metadata: BTreeMap::new(),
            metadata_revision: 0,
            positions,
            matches: WeightedLru::new(cache_limit),
        }
    }

    /// Visits the start commit and its ancestors using the retained topology index.
    /// Returns each snapshot position once, in depth-first order. Missing boundary
    /// parents and starts outside this snapshot are omitted. No Git read occurs.
    pub fn ancestor_positions(&self, start: &ObjectId) -> Vec<usize> {
        let Some(&start) = self.positions.get(start) else {
            return Vec::new();
        };
        let mut visited = vec![false; self.topology.rows.len()];
        let mut pending = vec![start];
        let mut result = Vec::new();
        while let Some(position) = pending.pop() {
            if std::mem::replace(&mut visited[position], true) {
                continue;
            }
            result.push(position);
            pending.extend(
                self.topology.rows[position]
                    .parents
                    .iter()
                    .filter_map(|parent| self.positions.get(parent).copied()),
            );
        }
        result
    }

    pub fn topology(&self) -> &Arc<LoadedTopology> {
        &self.topology
    }
    pub const fn cache_stats(&self) -> CacheStats {
        self.matches.stats()
    }

    /// Hydrates metadata and invalidates text results and continuation tokens.
    /// # Errors
    /// Returns an error if the metadata revision counter has been exhausted.
    pub fn hydrate(
        &mut self,
        records: impl IntoIterator<Item = GraphMetadataRecord>,
    ) -> Result<(), HistoryIndexError> {
        let next = self
            .metadata_revision
            .checked_add(1)
            .ok_or(HistoryIndexError::RevisionExhausted)?;
        let mut changed = false;
        for record in records {
            if self.positions.contains_key(&record.id)
                && self.metadata.get(&record.id) != Some(&record.metadata)
            {
                self.metadata.insert(record.id, record.metadata);
                changed = true;
            }
        }
        if changed {
            self.metadata_revision = next;
            self.matches.clear();
        }
        Ok(())
    }

    /// Filters this snapshot and returns a bounded page in its original topology order.
    /// Repeated pages share cached match positions and issue no Git commands.
    /// # Errors
    /// Rejects invalid bounds, canceled work, and tokens from another snapshot,
    /// another filter, or an earlier metadata revision.
    pub fn query(
        &mut self,
        filter: &HistoryFilter,
        limit: usize,
        token: Option<&HistoryPageToken>,
        cancellation: &dyn Cancellation,
    ) -> Result<HistoryPage, HistoryIndexError> {
        if limit == 0 || limit > MAX_HISTORY_PAGE_ROWS {
            return Err(HistoryIndexError::InvalidLimit);
        }
        if filter.text.len() > MAX_HISTORY_SEARCH_BYTES
            || (!filter.text.is_empty() && filter.scopes.is_empty())
        {
            return Err(HistoryIndexError::InvalidSearch);
        }
        if cancellation.is_cancelled() {
            return Err(HistoryIndexError::Canceled);
        }
        let offset = if let Some(token) = token {
            if !Arc::ptr_eq(&token.identity, &self.identity)
                || token.metadata_revision != self.metadata_revision
                || token.filter.as_ref() != filter
            {
                return Err(HistoryIndexError::StaleToken);
            }
            token.offset
        } else {
            0
        };
        let positions = if let Some(cached) = self.matches.get(filter) {
            Arc::clone(cached)
        } else {
            let matched = Arc::new(self.evaluate(filter, cancellation)?);
            let weight = matched
                .capacity()
                .saturating_mul(std::mem::size_of::<usize>())
                .saturating_add(filter.text.len())
                .saturating_add(filter.path_matches.len().saturating_mul(256))
                .saturating_add(
                    filter
                        .refs
                        .iter()
                        .map(|r| r.as_bytes().len() + 64)
                        .sum::<usize>(),
                );
            self.matches
                .insert(filter.clone(), Arc::clone(&matched), weight);
            matched
        };
        let end = offset.saturating_add(limit).min(positions.len());
        let commits = positions
            .get(offset..end)
            .ok_or(HistoryIndexError::StaleToken)?
            .iter()
            .map(|position| self.topology.rows[*position].id.clone())
            .collect();
        let next = (end < positions.len()).then(|| HistoryPageToken {
            identity: Arc::clone(&self.identity),
            metadata_revision: self.metadata_revision,
            filter: Arc::new(filter.clone()),
            offset: end,
        });
        if cancellation.is_cancelled() {
            return Err(HistoryIndexError::Canceled);
        }
        Ok(HistoryPage {
            commits,
            total_matches: positions.len(),
            next,
        })
    }

    fn evaluate(
        &self,
        filter: &HistoryFilter,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<usize>, HistoryIndexError> {
        let reachable = if filter.refs.is_empty() {
            None
        } else {
            Some(self.reachable_from(&filter.refs, cancellation)?)
        };
        let needle: Vec<_> = filter
            .text
            .bytes()
            .map(|byte| byte.to_ascii_lowercase())
            .collect();
        let mut result = Vec::new();
        for (position, row) in self.topology.rows.iter().enumerate() {
            if position % 256 == 0 && cancellation.is_cancelled() {
                return Err(HistoryIndexError::Canceled);
            }
            if reachable
                .as_ref()
                .is_some_and(|reachable| !reachable[position])
            {
                continue;
            }
            let metadata = self.metadata.get(&row.id).or(row.metadata.as_ref());
            if needle.is_empty()
                || filter.scopes.iter().any(|scope| match scope {
                    HistorySearchScope::Hash => {
                        contains_folded(row.id.as_str().as_bytes(), &needle)
                    }
                    HistorySearchScope::Message => {
                        metadata.is_some_and(|metadata| contains_folded(&metadata.subject, &needle))
                    }
                    HistorySearchScope::Author => {
                        metadata.is_some_and(|metadata| contains_folded(&metadata.author, &needle))
                    }
                    HistorySearchScope::Path => filter.path_matches.contains(&row.id),
                })
            {
                result.push(position);
            }
        }
        Ok(result)
    }
}

impl HistoryIndex {
    /// Marks every row reachable from a row carrying one of `refs`, walking
    /// parents by topology position. Parents outside the snapshot end the walk.
    fn reachable_from(
        &self,
        refs: &BTreeSet<RefId>,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<bool>, HistoryIndexError> {
        let rows = &self.topology.rows;
        let mut reachable = vec![false; rows.len()];
        let mut stack = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.refs.iter().any(|reference| refs.contains(reference)))
            .map(|(position, _)| position)
            .collect::<Vec<_>>();
        let mut visited = 0usize;
        while let Some(position) = stack.pop() {
            if visited.is_multiple_of(256) && cancellation.is_cancelled() {
                return Err(HistoryIndexError::Canceled);
            }
            visited += 1;
            if std::mem::replace(&mut reachable[position], true) {
                continue;
            }
            stack.extend(
                rows[position]
                    .parents
                    .iter()
                    .filter_map(|parent| self.positions.get(parent).copied())
                    .filter(|parent| !reachable[*parent]),
            );
        }
        Ok(reachable)
    }
}

fn contains_folded(bytes: &[u8], needle: &[u8]) -> bool {
    bytes.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(left, right)| left.to_ascii_lowercase() == *right)
    })
}

#[cfg(test)]
mod tests {
    use super::super::graph::{GraphMetadataStatus, IngestedCommit};
    use super::*;
    use std::sync::atomic::AtomicBool;
    #[test]
    fn ancestry_uses_snapshot_positions_and_stops_at_missing_parents() {
        let original = index();
        let mut topology = original.topology().as_ref().clone();
        topology.rows[0].parents.push(object(1));
        topology.rows[2].parents.push(object(99));
        let index = HistoryIndex::new(Arc::new(topology), CacheLimit::new(4, 4096).unwrap());
        let mut positions = index.ancestor_positions(&object(3));
        positions.sort_unstable();
        assert_eq!(positions, vec![0, 1, 2]);
        assert_eq!(index.ancestor_positions(&object(1)), vec![2]);
        assert!(index.ancestor_positions(&object(99)).is_empty());
    }

    fn object(n: usize) -> ObjectId {
        ObjectId::parse(format!("{n:040x}")).unwrap()
    }
    fn index() -> HistoryIndex {
        let rows = (1..=3)
            .rev()
            .map(|n| IngestedCommit {
                id: object(n),
                parents: if n > 1 { vec![object(n - 1)] } else { vec![] },
                refs: if n == 3 {
                    vec![RefId::from_bytes(b"refs/heads/main".to_vec()).unwrap()]
                } else {
                    vec![]
                },
                metadata: None,
            })
            .collect();
        HistoryIndex::new(
            Arc::new(LoadedTopology {
                rows,
                conditions: vec![],
                frozen_tips: vec![object(3)],
                activities: vec![],
            }),
            CacheLimit::new(4, 4096).unwrap(),
        )
    }
    #[test]
    fn pages_reuse_filter_and_tokens_reject_other_snapshots() {
        let mut index = index();
        let cancel = AtomicBool::new(false);
        let filter = HistoryFilter::default();
        let first = index.query(&filter, 2, None, &cancel).unwrap();
        assert_eq!(first.commits, vec![object(3), object(2)]);
        let second = index
            .query(&filter, 2, first.next.as_ref(), &cancel)
            .unwrap();
        assert_eq!(second.commits, vec![object(1)]);
        assert!(second.next.is_none());
        assert_eq!(index.cache_stats().hits, 1);
        let mut other = HistoryIndex::new(
            Arc::clone(index.topology()),
            CacheLimit::new(4, 4096).unwrap(),
        );
        assert!(matches!(
            other.query(&filter, 2, first.next.as_ref(), &cancel),
            Err(HistoryIndexError::StaleToken)
        ));
    }
    #[test]
    fn ref_filters_keep_the_union_of_ancestries_through_merges() {
        // 6 merges 5 (side) into 4; 3 is an unrelated root; 2 descends from 1.
        let reference = |name: &str| RefId::from_bytes(name.as_bytes().to_vec()).unwrap();
        let row = |n: usize, parents: Vec<usize>, refs: Vec<RefId>| IngestedCommit {
            id: object(n),
            parents: parents.into_iter().map(object).collect(),
            refs,
            metadata: None,
        };
        let mut index = HistoryIndex::new(
            Arc::new(LoadedTopology {
                rows: vec![
                    row(6, vec![4, 5], vec![reference("refs/heads/main")]),
                    row(5, vec![1], vec![reference("refs/heads/side")]),
                    row(4, vec![1, 99], vec![]),
                    row(3, vec![], vec![reference("refs/heads/orphan")]),
                    row(2, vec![1], vec![reference("refs/remotes/origin/topic")]),
                    row(1, vec![], vec![]),
                ],
                conditions: vec![],
                frozen_tips: vec![object(6), object(3), object(2)],
                activities: vec![],
            }),
            CacheLimit::new(8, 4096).unwrap(),
        );
        let cancel = AtomicBool::new(false);
        let mut query = |refs: &[&str]| {
            let filter = HistoryFilter {
                refs: refs.iter().map(|name| reference(name)).collect(),
                ..HistoryFilter::default()
            };
            index.query(&filter, 16, None, &cancel).unwrap().commits
        };
        assert_eq!(
            query(&["refs/heads/main"]),
            vec![object(6), object(5), object(4), object(1)]
        );
        assert_eq!(query(&["refs/heads/side"]), vec![object(5), object(1)]);
        assert_eq!(
            query(&["refs/heads/orphan", "refs/remotes/origin/topic"]),
            vec![object(3), object(2), object(1)]
        );
        assert!(query(&["refs/heads/missing"]).is_empty());
    }

    #[test]
    fn hydration_invalidates_text_cache_and_tokens() {
        let mut index = index();
        let cancel = AtomicBool::new(false);
        let all = HistoryFilter::default();
        let page = index.query(&all, 1, None, &cancel).unwrap();
        index
            .hydrate([GraphMetadataRecord {
                id: object(2),
                metadata: GraphCommitMetadata {
                    subject: b"Fix Parser".to_vec(),
                    author: b"Author".to_vec(),
                    avatar_hash: String::new(),
                    author_time: 0,
                },
                status: GraphMetadataStatus::Available,
            }])
            .unwrap();
        assert!(matches!(
            index.query(&all, 1, page.next.as_ref(), &cancel),
            Err(HistoryIndexError::StaleToken)
        ));
        let filter = HistoryFilter {
            text: "parser".into(),
            scopes: BTreeSet::from([HistorySearchScope::Message]),
            refs: BTreeSet::from([RefId::from_bytes(b"refs/heads/main".to_vec()).unwrap()]),
            path_matches: BTreeSet::new(),
        };
        assert_eq!(
            index.query(&filter, 5, None, &cancel).unwrap().commits,
            vec![object(2)]
        );
        assert!(matches!(
            index.query(&filter, 5, None, &AtomicBool::new(true)),
            Err(HistoryIndexError::Canceled)
        ));
    }
}
