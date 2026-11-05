//! Reusable graph value caches; callers provide revision and formatting identities.
use super::{CacheLimit, CacheStats, WeightedLru};
use crate::loading::graph::GraphCommitMetadata;
use crate::types::{ObjectId, RefId};
use std::collections::BTreeSet;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphCacheLimits {
    pub metadata: CacheLimit,
    pub diff_preview: CacheLimit,
    pub search: CacheLimit,
    pub strings: CacheLimit,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GraphCacheStats {
    pub metadata: CacheStats,
    pub diff_preview: CacheStats,
    pub search: CacheStats,
    pub strings: CacheStats,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum GraphDiffEndpoint {
    Commit(ObjectId),
    WorkingTree { revision: u64 },
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DiffPreviewKey {
    pub first: GraphDiffEndpoint,
    pub second: GraphDiffEndpoint,
    pub options_revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum GraphStringField {
    Subject,
    Author,
    Date,
    RefLabel,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum GraphStringIdentity {
    Commit(ObjectId),
    Reference(RefId),
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StringCacheKey {
    pub identity: GraphStringIdentity,
    pub field: GraphStringField,
    pub formatting_revision: u64,
}

#[derive(Clone, Debug)]
pub struct GraphValueCaches<K = String, V = Vec<ObjectId>> {
    search: WeightedLru<K, V>,
    metadata: WeightedLru<ObjectId, GraphCommitMetadata>,
    diff_preview: WeightedLru<DiffPreviewKey, Vec<u8>>,
    strings: WeightedLru<StringCacheKey, String>,
}
impl<K: Clone + Eq + std::hash::Hash + Ord, V> GraphValueCaches<K, V> {
    pub fn new(limits: GraphCacheLimits) -> Self {
        Self {
            search: WeightedLru::new(limits.search),
            metadata: WeightedLru::new(limits.metadata),
            diff_preview: WeightedLru::new(limits.diff_preview),
            strings: WeightedLru::new(limits.strings),
        }
    }
    pub fn cache_metadata(&mut self, id: ObjectId, value: GraphCommitMetadata) -> bool {
        let weight = value
            .subject
            .len()
            .saturating_add(value.author.len())
            .saturating_add(std::mem::size_of::<GraphCommitMetadata>())
            .saturating_add(std::mem::size_of::<ObjectId>())
            .saturating_add(id.as_str().len());
        self.metadata.insert(id, value, weight)
    }

    pub fn metadata(&mut self, id: &ObjectId) -> Option<&GraphCommitMetadata> {
        self.metadata.get(id)
    }

    pub fn cache_diff_preview(&mut self, key: DiffPreviewKey, bytes: Vec<u8>) -> bool {
        let weight = bytes.len().saturating_add(diff_key_weight(&key));
        self.diff_preview.insert(key, bytes, weight)
    }

    pub fn diff_preview(&mut self, key: &DiffPreviewKey) -> Option<&[u8]> {
        self.diff_preview.get(key).map(Vec::as_slice)
    }

    pub fn cache_string(&mut self, key: StringCacheKey, value: String) -> bool {
        let identity_bytes = match &key.identity {
            GraphStringIdentity::Commit(id) => id.as_str().len(),
            GraphStringIdentity::Reference(id) => id.as_bytes().len(),
        };
        let weight = value
            .len()
            .saturating_add(std::mem::size_of::<StringCacheKey>())
            .saturating_add(identity_bytes);
        self.strings.insert(key, value, weight)
    }

    pub fn string(&mut self, key: &StringCacheKey) -> Option<&str> {
        self.strings.get(key).map(String::as_str)
    }

    pub fn invalidate_refs(&mut self, references: &BTreeSet<RefId>) {
        self.strings.remove_where(|key| {
            matches!(&key.identity, GraphStringIdentity::Reference(id) if references.contains(id))
        });
    }

    pub fn invalidate_worktree(&mut self) {
        self.diff_preview.remove_where(|key| {
            matches!(key.first, GraphDiffEndpoint::WorkingTree { .. })
                || matches!(key.second, GraphDiffEndpoint::WorkingTree { .. })
        });
    }

    pub fn invalidate_formatting(&mut self) {
        self.strings.clear();
    }

    pub fn close(&mut self) {
        self.search.clear();
        self.metadata.clear();
        self.diff_preview.clear();
        self.strings.clear();
    }

    pub fn search(&mut self) -> &mut WeightedLru<K, V> {
        &mut self.search
    }
    pub const fn stats(&self) -> GraphCacheStats {
        GraphCacheStats {
            metadata: self.metadata.stats(),
            diff_preview: self.diff_preview.stats(),
            strings: self.strings.stats(),
            search: self.search.stats(),
        }
    }
}
fn diff_key_weight(key: &DiffPreviewKey) -> usize {
    fn endpoint_weight(endpoint: &GraphDiffEndpoint) -> usize {
        match endpoint {
            GraphDiffEndpoint::Commit(id) => {
                std::mem::size_of::<GraphDiffEndpoint>().saturating_add(id.as_str().len())
            }
            GraphDiffEndpoint::WorkingTree { .. } => std::mem::size_of::<GraphDiffEndpoint>(),
        }
    }
    endpoint_weight(&key.first).saturating_add(endpoint_weight(&key.second))
}
