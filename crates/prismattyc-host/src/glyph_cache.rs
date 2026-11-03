//! Bounded least-recently-used cache for rasterized glyphs.
//!
//! One cache lives in each `FontMetrics`, so a font swap (zoom, scale change,
//! config reload) starts with an empty cache: a new `FontMetrics` replaces the
//! old one and drops its entries.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;

/// Lookup counters since the cache was created.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct GlyphCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub entries: usize,
}

impl GlyphCacheStats {
    pub(crate) fn add(self, other: Self) -> Self {
        Self {
            hits: self.hits.saturating_add(other.hits),
            misses: self.misses.saturating_add(other.misses),
            evictions: self.evictions.saturating_add(other.evictions),
            entries: self.entries.saturating_add(other.entries),
        }
    }
}

pub(crate) struct LruCache<K, V> {
    map: HashMap<K, (V, u64)>,
    tick: u64,
    capacity: usize,
    stats: GlyphCacheStats,
}

impl<K: Eq + Hash, V: Clone> LruCache<K, V> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::new(),
            tick: 0,
            capacity: capacity.max(2),
            stats: GlyphCacheStats::default(),
        }
    }

    /// The cached value, marked as most recently used. Counts a hit or a miss.
    pub(crate) fn get<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.tick += 1;
        match self.map.get_mut(key) {
            Some((value, used)) => {
                *used = self.tick;
                self.stats.hits += 1;
                Some(value.clone())
            }
            None => {
                self.stats.misses += 1;
                None
            }
        }
    }

    /// Store `value`. A full cache first drops its least recently used half,
    /// so eviction work stays constant per insert on average.
    pub(crate) fn insert(&mut self, key: K, value: V) {
        if self.map.len() >= self.capacity && !self.map.contains_key(&key) {
            self.evict_older_half();
        }
        self.tick += 1;
        self.map.insert(key, (value, self.tick));
    }

    pub(crate) fn stats(&self) -> GlyphCacheStats {
        GlyphCacheStats {
            entries: self.map.len(),
            ..self.stats
        }
    }

    fn evict_older_half(&mut self) {
        let mut ticks: Vec<u64> = self.map.values().map(|(_, used)| *used).collect();
        let keep = ticks.len() / 2;
        let cutoff_index = ticks.len() - keep - 1;
        let (_, cutoff, _) = ticks.select_nth_unstable(cutoff_index);
        let cutoff = *cutoff;
        let before = self.map.len();
        self.map.retain(|_, (_, used)| *used > cutoff);
        self.stats.evictions += (before - self.map.len()) as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_counts_hits_and_misses() {
        let mut cache = LruCache::new(8);
        assert_eq!(cache.get(&'a'), None::<u32>);
        cache.insert('a', 1);
        assert_eq!(cache.get(&'a'), Some(1));
        assert_eq!(cache.get(&'a'), Some(1));
        assert_eq!(
            cache.stats(),
            GlyphCacheStats {
                hits: 2,
                misses: 1,
                evictions: 0,
                entries: 1,
            }
        );
    }

    #[test]
    fn a_full_cache_evicts_the_least_recently_used_half() {
        let mut cache = LruCache::new(4);
        for key in 0..4u32 {
            cache.insert(key, key);
        }
        // Touch 0 and 1 so 2 and 3 are the least recently used.
        assert_eq!(cache.get(&0), Some(0));
        assert_eq!(cache.get(&1), Some(1));
        cache.insert(4, 4);
        assert_eq!(cache.stats().evictions, 2);
        assert_eq!(cache.stats().entries, 3);
        assert_eq!(cache.get(&0), Some(0));
        assert_eq!(cache.get(&1), Some(1));
        assert_eq!(cache.get(&4), Some(4));
        assert_eq!(cache.get(&2), None);
        assert_eq!(cache.get(&3), None);
    }

    #[test]
    fn replacing_a_key_in_a_full_cache_does_not_evict() {
        let mut cache = LruCache::new(2);
        cache.insert('a', 1);
        cache.insert('b', 2);
        cache.insert('a', 3);
        assert_eq!(cache.stats().evictions, 0);
        assert_eq!(cache.get(&'a'), Some(3));
        assert_eq!(cache.get(&'b'), Some(2));
    }

    #[test]
    fn size_stays_bounded_under_a_stream_of_new_keys() {
        let mut cache = LruCache::new(64);
        for key in 0..10_000u32 {
            if cache.get(&key).is_none() {
                cache.insert(key, key);
            }
            assert!(cache.stats().entries <= 64);
        }
        assert!(cache.stats().evictions >= 10_000 - 64);
    }

    #[test]
    fn borrowed_str_keys_find_boxed_entries() {
        let mut cache: LruCache<Box<str>, u8> = LruCache::new(4);
        cache.insert("🇺🇸".into(), 7);
        assert_eq!(cache.get("🇺🇸"), Some(7));
    }
}
