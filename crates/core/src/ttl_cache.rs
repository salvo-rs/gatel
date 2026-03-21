use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A simple thread-safe TTL cache.
///
/// Entries are lazily evicted: expired entries are removed on the next
/// `get` or `cleanup` call, not by a background timer.
pub struct TtlCache<K, V> {
    inner: Mutex<CacheInner<K, V>>,
    default_ttl: Duration,
    max_entries: usize,
}

struct CacheInner<K, V> {
    entries: HashMap<K, CacheEntry<V>>,
}

struct CacheEntry<V> {
    value: V,
    expires_at: Instant,
}

impl<K: Eq + Hash + Clone, V: Clone> TtlCache<K, V> {
    /// Create a new TTL cache.
    pub fn new(default_ttl: Duration, max_entries: usize) -> Self {
        Self {
            inner: Mutex::new(CacheInner {
                entries: HashMap::new(),
            }),
            default_ttl,
            max_entries,
        }
    }

    /// Insert a value with the default TTL.
    pub fn insert(&self, key: K, value: V) {
        self.insert_with_ttl(key, value, self.default_ttl);
    }

    /// Insert a value with a custom TTL.
    pub fn insert_with_ttl(&self, key: K, value: V, ttl: Duration) {
        let mut inner = self.inner.lock().unwrap();
        // Evict expired entries if we're at capacity.
        if inner.entries.len() >= self.max_entries {
            let now = Instant::now();
            inner.entries.retain(|_, e| e.expires_at > now);
        }
        // If still at capacity after eviction, drop the oldest entry.
        if inner.entries.len() >= self.max_entries
            && let Some(oldest_key) = inner
                .entries
                .iter()
                .min_by_key(|(_, e)| e.expires_at)
                .map(|(k, _)| k.clone())
        {
            inner.entries.remove(&oldest_key);
        }
        inner.entries.insert(
            key,
            CacheEntry {
                value,
                expires_at: Instant::now() + ttl,
            },
        );
    }

    /// Get a value if it exists and hasn't expired.
    pub fn get(&self, key: &K) -> Option<V> {
        let mut inner = self.inner.lock().unwrap();
        let entry = inner.entries.get(key)?;
        if entry.expires_at <= Instant::now() {
            inner.entries.remove(key);
            None
        } else {
            Some(entry.value.clone())
        }
    }

    /// Remove a value.
    pub fn remove(&self, key: &K) -> Option<V> {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.remove(key).map(|e| e.value)
    }

    /// Remove all expired entries.
    pub fn cleanup(&self) {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        inner.entries.retain(|_, e| e.expires_at > now);
    }

    /// Number of entries (including potentially expired ones).
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_get() {
        let cache = TtlCache::new(Duration::from_secs(60), 100);
        cache.insert("key1", "value1");
        assert_eq!(cache.get(&"key1"), Some("value1"));
    }

    #[test]
    fn expired_entry_returns_none() {
        let cache = TtlCache::new(Duration::from_millis(1), 100);
        cache.insert("key1", "value1");
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(cache.get(&"key1"), None);
    }

    #[test]
    fn max_entries_eviction() {
        let cache = TtlCache::new(Duration::from_secs(60), 2);
        cache.insert("a", 1);
        cache.insert("b", 2);
        cache.insert("c", 3); // should evict oldest
        assert_eq!(cache.len(), 2);
        assert!(cache.get(&"c").is_some());
    }

    #[test]
    fn remove() {
        let cache = TtlCache::new(Duration::from_secs(60), 100);
        cache.insert("key1", "value1");
        assert_eq!(cache.remove(&"key1"), Some("value1"));
        assert_eq!(cache.get(&"key1"), None);
    }

    #[test]
    fn cleanup_removes_expired() {
        let cache = TtlCache::new(Duration::from_millis(1), 100);
        cache.insert("a", 1);
        cache.insert("b", 2);
        std::thread::sleep(Duration::from_millis(10));
        cache.cleanup();
        assert_eq!(cache.len(), 0);
    }
}
