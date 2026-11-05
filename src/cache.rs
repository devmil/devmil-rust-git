//! Entry- and payload-bounded least-recently-used cache.
pub mod graph;
use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheLimit {
    pub entries: usize,
    pub bytes: usize,
}

impl CacheLimit {
    /// Creates simultaneous entry and accounted-payload limits.
    ///
    /// # Errors
    ///
    /// Returns [`CacheConfigurationError::ZeroLimit`] when either limit is zero.
    pub fn new(entries: usize, bytes: usize) -> Result<Self, CacheConfigurationError> {
        if entries == 0 || bytes == 0 {
            return Err(CacheConfigurationError::ZeroLimit);
        }
        Ok(Self { entries, bytes })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheConfigurationError {
    ZeroLimit,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub insertions: u64,
    pub evictions: u64,
    pub invalidations: u64,
    pub oversize_rejections: u64,
    pub entries: usize,
    /// Caller-supplied payload weights used for deterministic eviction. This deliberately does
    /// not claim to measure allocator, map-node, key, or capacity overhead.
    pub accounted_payload_bytes: usize,
    pub max_entries: usize,
    pub max_accounted_payload_bytes: usize,
}

#[derive(Clone, Debug)]
struct CacheEntry<V> {
    value: V,
    weight: usize,
    accessed: u64,
}

#[derive(Clone, Debug)]
pub struct WeightedLru<K, V> {
    limit: CacheLimit,
    entries: HashMap<K, CacheEntry<V>>,
    access_order: BTreeMap<u64, K>,
    next_access: u64,
    stats: CacheStats,
}

impl<K, V> WeightedLru<K, V>
where
    K: Clone + Eq + Hash + Ord,
{
    pub fn new(limit: CacheLimit) -> Self {
        Self {
            limit,
            entries: HashMap::with_capacity(limit.entries),
            access_order: BTreeMap::new(),
            next_access: 1,
            stats: CacheStats {
                max_entries: limit.entries,
                max_accounted_payload_bytes: limit.bytes,
                ..CacheStats::default()
            },
        }
    }

    pub fn get(&mut self, key: &K) -> Option<&V> {
        if !self.entries.contains_key(key) {
            self.stats.misses = self.stats.misses.saturating_add(1);
            return None;
        }
        self.stats.hits = self.stats.hits.saturating_add(1);
        let next = self.allocate_access();
        let entry = self.entries.get_mut(key)?;
        self.access_order.remove(&entry.accessed);
        entry.accessed = next;
        self.access_order.insert(next, key.clone());
        Some(&entry.value)
    }

    pub fn insert(&mut self, key: K, value: V, weight: usize) -> bool {
        if weight > self.limit.bytes {
            self.stats.oversize_rejections = self.stats.oversize_rejections.saturating_add(1);
            return false;
        }
        if let Some(replaced) = self.entries.remove(&key) {
            self.access_order.remove(&replaced.accessed);
            self.stats.accounted_payload_bytes = self
                .stats
                .accounted_payload_bytes
                .saturating_sub(replaced.weight);
        }
        let accessed = self.allocate_access();
        self.entries.insert(
            key.clone(),
            CacheEntry {
                value,
                weight,
                accessed,
            },
        );
        self.access_order.insert(accessed, key);
        self.stats.insertions = self.stats.insertions.saturating_add(1);
        self.stats.accounted_payload_bytes =
            self.stats.accounted_payload_bytes.saturating_add(weight);
        self.enforce_limit();
        self.sync_counts();
        true
    }

    pub fn remove_where(&mut self, predicate: impl Fn(&K) -> bool) {
        let keys = self
            .entries
            .keys()
            .filter(|key| predicate(key))
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            self.invalidate_key(&key);
        }
        self.sync_counts();
    }

    pub fn clear(&mut self) {
        let count = u64::try_from(self.entries.len()).unwrap_or(u64::MAX);
        self.stats.invalidations = self.stats.invalidations.saturating_add(count);
        self.entries.clear();
        self.access_order.clear();
        self.stats.accounted_payload_bytes = 0;
        self.sync_counts();
    }

    pub const fn stats(&self) -> CacheStats {
        self.stats
    }

    fn invalidate_key(&mut self, key: &K) {
        if let Some(entry) = self.entries.remove(key) {
            self.access_order.remove(&entry.accessed);
            self.stats.accounted_payload_bytes = self
                .stats
                .accounted_payload_bytes
                .saturating_sub(entry.weight);
            self.stats.invalidations = self.stats.invalidations.saturating_add(1);
        }
    }

    fn enforce_limit(&mut self) {
        while self.entries.len() > self.limit.entries
            || self.stats.accounted_payload_bytes > self.limit.bytes
        {
            let Some((&accessed, key)) = self.access_order.first_key_value() else {
                break;
            };
            let key = key.clone();
            self.access_order.remove(&accessed);
            if let Some(entry) = self.entries.remove(&key) {
                self.stats.accounted_payload_bytes = self
                    .stats
                    .accounted_payload_bytes
                    .saturating_sub(entry.weight);
                self.stats.evictions = self.stats.evictions.saturating_add(1);
            }
        }
    }

    fn allocate_access(&mut self) -> u64 {
        if self.next_access == u64::MAX {
            let old = std::mem::take(&mut self.access_order);
            for (index, (_, key)) in old.into_iter().enumerate() {
                let accessed = u64::try_from(index + 1).unwrap_or(u64::MAX - 1);
                if let Some(entry) = self.entries.get_mut(&key) {
                    entry.accessed = accessed;
                }
                self.access_order.insert(accessed, key);
            }
            self.next_access = u64::try_from(self.access_order.len())
                .unwrap_or(u64::MAX - 2)
                .saturating_add(1);
        }
        let allocated = self.next_access;
        self.next_access = self.next_access.saturating_add(1);
        allocated
    }

    fn sync_counts(&mut self) {
        self.stats.entries = self.entries.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn limit(entries: usize, bytes: usize) -> CacheLimit {
        CacheLimit::new(entries, bytes).unwrap()
    }
    #[test]
    fn weighted_lru_enforces_entry_and_byte_limits_with_exact_stats() {
        let mut cache = WeightedLru::new(limit(2, 6));
        assert!(cache.insert(1, "a", 2));
        assert!(cache.insert(2, "b", 2));
        assert_eq!(cache.get(&1), Some(&"a"));
        assert!(cache.insert(3, "c", 3));
        assert_eq!(cache.get(&2), None);
        assert_eq!(cache.get(&1), Some(&"a"));
        assert!(!cache.insert(4, "oversize", 7));
        let stats = cache.stats();
        assert_eq!(stats.entries, 2);
        assert_eq!(stats.accounted_payload_bytes, 5);
        assert_eq!(stats.hits, 2);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.evictions, 1);
        assert_eq!(stats.oversize_rejections, 1);
    }

    #[test]
    fn invalidation_is_separate_from_lru_eviction() {
        let mut cache = WeightedLru::new(limit(4, 32));
        cache.insert(1, "one", 3);
        cache.insert(2, "two", 3);
        cache.remove_where(|key| *key == 1);
        assert_eq!(cache.stats().invalidations, 1);
        assert_eq!(cache.stats().evictions, 0);
        assert_eq!(cache.stats().entries, 1);
        cache.clear();
        assert_eq!(cache.stats().invalidations, 2);
        assert_eq!(cache.stats().entries, 0);
        assert_eq!(cache.stats().accounted_payload_bytes, 0);
    }
}
