//! What is held, and what is thrown away to make room.
//!
//! In memory, bounded by bytes rather than by entries, because a bound on
//! entries is not a bound on anything: a thousand entries is a megabyte or a
//! gigabyte depending on what was cached.
//!
//! The state lives here rather than in the handler because a cache is exactly
//! the kind of thing a reload must not empty. Reformatting a configuration
//! file, or changing a header somewhere else, would otherwise send every
//! request to the origin at once.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;

/// What a request is looked up by, before `Vary` is taken into account.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub method: String,
    pub host: String,
    pub target: String,
}

impl Key {
    pub fn new(method: &http::Method, host: &str, target: &str) -> Self {
        Self {
            method: method.as_str().to_string(),
            // Case-insensitive, so `Example.com` and `example.com` are one
            // entry rather than two copies of the same page.
            host: host.to_ascii_lowercase(),
            target: target.to_string(),
        }
    }
}

/// One stored response.
#[derive(Debug, Clone)]
pub struct Entry {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub body: Bytes,
    pub stored: Instant,
    pub ttl: Duration,
    /// How old it already was when it arrived, from an `Age` the origin sent.
    /// Age accumulates along a chain of caches.
    pub age_on_arrival: u64,
}

impl Entry {
    pub fn age(&self) -> Duration {
        self.stored.elapsed()
    }

    pub fn is_fresh(&self) -> bool {
        self.age() < self.ttl
    }

    /// Roughly what this costs to hold. Headers are counted because a response
    /// with a hundred of them is not free just because its body is empty.
    fn weight(&self) -> u64 {
        let headers: usize = self
            .headers
            .iter()
            .map(|(name, value)| name.as_str().len() + value.len())
            .sum();
        (self.body.len() + headers) as u64
    }
}

/// A stored response together with the request headers it was chosen by.
#[derive(Debug)]
struct Held {
    entry: Entry,
    /// Which `Vary` signature this copy answers.
    signature: String,
    /// When it was last handed out, for eviction.
    used: u64,
}

/// The shared store, which outlives any generation that uses it.
#[derive(Debug)]
pub struct Store {
    /// Every copy of a resource, by base key and then by `Vary` signature.
    ///
    /// Two levels because `Vary` is a property of the *response*: the headers
    /// a request must match on are not known until a response for that
    /// resource has been seen once.
    held: Mutex<HashMap<Key, Vec<Held>>>,
    /// Which request headers each resource varies on, learned from the first
    /// response and applied to every later lookup.
    vary: Mutex<HashMap<Key, Vec<String>>>,
    bytes: AtomicU64,
    max_bytes: u64,
    clock: AtomicU64,
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub evictions: AtomicU64,
}

impl Store {
    pub fn new(max_bytes: u64) -> Self {
        Self {
            held: Mutex::new(HashMap::new()),
            vary: Mutex::new(HashMap::new()),
            bytes: AtomicU64::new(0),
            max_bytes,
            clock: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    /// The headers this resource has been seen to vary on, if any response for
    /// it has been stored.
    pub fn vary_for(&self, key: &Key) -> Option<Vec<String>> {
        self.vary
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(key)
            .cloned()
    }

    /// A fresh entry for this key and signature, if there is one.
    ///
    /// A stale entry is dropped as it is found: keeping it would only mean
    /// holding bytes nobody may be served.
    pub fn get(&self, key: &Key, signature: &str) -> Option<Entry> {
        let mut held = self.held.lock().unwrap_or_else(|p| p.into_inner());
        let Some(copies) = held.get_mut(key) else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };

        let mut freed = 0;
        copies.retain(|copy| {
            if copy.entry.is_fresh() {
                return true;
            }
            freed += copy.entry.weight();
            false
        });
        if freed > 0 {
            self.bytes.fetch_sub(freed, Ordering::Relaxed);
        }

        let now = self.clock.fetch_add(1, Ordering::Relaxed);
        let found = copies
            .iter_mut()
            .find(|copy| copy.signature == signature)
            .map(|copy| {
                copy.used = now;
                copy.entry.clone()
            });
        if copies.is_empty() {
            held.remove(key);
            // And forget what it varied on. The byte budget counts bodies, so
            // it does not bound this map: without the removal, a gateway
            // serving pages that expire keeps one entry per URL it has ever
            // seen, for as long as the process runs.
            self.vary.lock().unwrap_or_else(|p| p.into_inner()).remove(key);
        }
        drop(held);

        match found {
            Some(entry) => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(entry)
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Keep a response, replacing any copy with the same signature.
    pub fn put(&self, key: Key, signature: String, vary: Vec<String>, entry: Entry) {
        let weight = entry.weight();
        if weight > self.max_bytes {
            // Nothing to do but refuse: making room for it would empty the
            // whole cache for one response.
            return;
        }

        {
            let mut varies = self.vary.lock().unwrap_or_else(|p| p.into_inner());
            varies.insert(key.clone(), vary);
        }

        let mut held = self.held.lock().unwrap_or_else(|p| p.into_inner());
        let now = self.clock.fetch_add(1, Ordering::Relaxed);
        let copies = held.entry(key).or_default();
        if let Some(existing) = copies.iter_mut().find(|copy| copy.signature == signature) {
            self.bytes.fetch_sub(existing.entry.weight(), Ordering::Relaxed);
            existing.entry = entry;
            existing.used = now;
        } else {
            copies.push(Held {
                entry,
                signature,
                used: now,
            });
        }
        self.bytes.fetch_add(weight, Ordering::Relaxed);
        self.evict_down_to_budget(&mut held);
    }

    /// Throw away least-recently-used copies until the budget is met.
    ///
    /// A scan rather than an index: eviction happens only when the cache is
    /// full, the number of entries is bounded by the budget, and a scan of a
    /// few thousand is measured in microseconds. An index would be a second
    /// structure to keep in step with this one, which is a bug waiting to be
    /// written.
    fn evict_down_to_budget(&self, held: &mut HashMap<Key, Vec<Held>>) {
        while self.bytes.load(Ordering::Relaxed) > self.max_bytes {
            let Some((key, signature, weight)) = held
                .iter()
                .flat_map(|(key, copies)| copies.iter().map(move |copy| (key, copy)))
                .min_by_key(|(_, copy)| copy.used)
                .map(|(key, copy)| (key.clone(), copy.signature.clone(), copy.entry.weight()))
            else {
                // Nothing left to drop, which means the accounting is wrong
                // rather than the cache being full. Stop rather than spin.
                self.bytes.store(0, Ordering::Relaxed);
                return;
            };

            if let Some(copies) = held.get_mut(&key) {
                copies.retain(|copy| copy.signature != signature);
                if copies.is_empty() {
                    held.remove(&key);
                    self.vary.lock().unwrap_or_else(|p| p.into_inner()).remove(&key);
                }
            }
            self.bytes.fetch_sub(weight, Ordering::Relaxed);
            self.evictions.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn held_bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    pub fn len(&self) -> usize {
        self.held
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .map(Vec::len)
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many resources the store remembers a `Vary` list for.
    ///
    /// Exposed because it is the one thing here that the byte budget does not
    /// bound, so it is the one thing that needs watching.
    pub fn remembered_resources(&self) -> usize {
        self.vary.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
}

/// The signature a request has for a given `Vary` list.
///
/// A header the request does not send is recorded as absent rather than
/// skipped, so "no Accept-Encoding" and "Accept-Encoding: gzip" cannot collide.
pub fn signature(headers: &http::HeaderMap, vary: &[String]) -> String {
    let mut parts = Vec::with_capacity(vary.len());
    for name in vary {
        let value = headers
            .get(name.as_str())
            .and_then(|value| value.to_str().ok())
            .unwrap_or("\u{0}absent");
        parts.push(format!("{name}={value}"));
    }
    parts.join("\u{1}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(body: &str, ttl: Duration) -> Entry {
        Entry {
            status: 200,
            headers: http::HeaderMap::new(),
            body: Bytes::from(body.to_string()),
            stored: Instant::now(),
            ttl,
            age_on_arrival: 0,
        }
    }

    fn key(target: &str) -> Key {
        Key::new(&http::Method::GET, "example.com", target)
    }

    const MINUTE: Duration = Duration::from_secs(60);

    #[test]
    fn what_was_put_comes_back() {
        let store = Store::new(1024);
        store.put(key("/a"), String::new(), Vec::new(), entry("hello", MINUTE));
        assert_eq!(store.get(&key("/a"), "").unwrap().body, "hello");
        assert_eq!(store.hits.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_stale_entry_is_dropped_rather_than_served() {
        let store = Store::new(1024);
        store.put(key("/a"), String::new(), Vec::new(), entry("old", Duration::ZERO));
        assert!(store.get(&key("/a"), "").is_none());
        assert_eq!(store.held_bytes(), 0, "and its bytes are given back");
        assert!(store.is_empty());
    }

    #[test]
    fn two_hosts_are_two_entries() {
        let store = Store::new(1024);
        store.put(
            Key::new(&http::Method::GET, "one.example", "/"),
            String::new(),
            Vec::new(),
            entry("one", MINUTE),
        );
        store.put(
            Key::new(&http::Method::GET, "two.example", "/"),
            String::new(),
            Vec::new(),
            entry("two", MINUTE),
        );
        assert_eq!(
            store
                .get(&Key::new(&http::Method::GET, "TWO.EXAMPLE", "/"), "")
                .unwrap()
                .body,
            "two",
            "and a host matches whatever case it was written in"
        );
    }

    #[test]
    fn two_signatures_of_one_resource_are_two_copies() {
        // What `Vary` is for: a gzip copy and a plain copy of one page, each
        // served to the client that asked for it.
        let store = Store::new(1024);
        let vary = vec!["accept-encoding".to_string()];
        store.put(
            key("/a"),
            "gzip".into(),
            vary.clone(),
            entry("compressed", MINUTE),
        );
        store.put(key("/a"), "none".into(), vary.clone(), entry("plain", MINUTE));

        assert_eq!(store.get(&key("/a"), "gzip").unwrap().body, "compressed");
        assert_eq!(store.get(&key("/a"), "none").unwrap().body, "plain");
        assert!(store.get(&key("/a"), "br").is_none());
        assert_eq!(store.vary_for(&key("/a")), Some(vary));
    }

    #[test]
    fn storing_the_same_copy_twice_replaces_it_rather_than_growing() {
        let store = Store::new(1024);
        store.put(key("/a"), String::new(), Vec::new(), entry("first", MINUTE));
        let after_one = store.held_bytes();
        store.put(key("/a"), String::new(), Vec::new(), entry("secnd", MINUTE));

        assert_eq!(store.get(&key("/a"), "").unwrap().body, "secnd");
        assert_eq!(store.held_bytes(), after_one, "one copy, not two");
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn the_budget_is_in_bytes_and_is_kept() {
        let store = Store::new(40);
        for index in 0..20 {
            store.put(
                key(&format!("/{index}")),
                String::new(),
                Vec::new(),
                entry("0123456789", MINUTE),
            );
        }
        assert!(
            store.held_bytes() <= 40,
            "held {} against a budget of 40",
            store.held_bytes()
        );
        assert!(store.evictions.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn what_is_used_survives_what_is_not() {
        let store = Store::new(40);
        store.put(
            key("/keep"),
            String::new(),
            Vec::new(),
            entry("0123456789", MINUTE),
        );
        store.put(
            key("/drop"),
            String::new(),
            Vec::new(),
            entry("0123456789", MINUTE),
        );
        // Touching one makes the other the least recently used.
        assert!(store.get(&key("/keep"), "").is_some());

        // The budget holds four of these, so filling it to five forces
        // exactly one eviction — which is what makes this a test of *which*
        // one rather than of how many.
        for index in 0..3 {
            store.put(
                key(&format!("/filler{index}")),
                String::new(),
                Vec::new(),
                entry("0123456789", MINUTE),
            );
        }
        assert!(
            store.get(&key("/keep"), "").is_some(),
            "the one that was being used should have survived"
        );
        assert!(
            store.get(&key("/drop"), "").is_none(),
            "and the one that was not should be the one that went"
        );
    }

    #[test]
    fn something_larger_than_the_whole_budget_is_refused() {
        // Making room for it would empty the cache for one response.
        let store = Store::new(16);
        store.put(
            key("/big"),
            String::new(),
            Vec::new(),
            entry(&"x".repeat(100), MINUTE),
        );
        assert!(store.is_empty());
        assert_eq!(store.held_bytes(), 0);
    }

    #[test]
    fn a_header_that_is_absent_does_not_collide_with_one_that_is_present() {
        let vary = vec!["accept-encoding".to_string()];
        let mut with = http::HeaderMap::new();
        with.insert("accept-encoding", http::HeaderValue::from_static("gzip"));
        assert_ne!(signature(&with, &vary), signature(&http::HeaderMap::new(), &vary));
    }
}

#[cfg(test)]
mod leak_tests {
    use super::*;

    fn entry(ttl: Duration) -> Entry {
        Entry {
            status: 200,
            headers: http::HeaderMap::new(),
            body: Bytes::from_static(b"x"),
            stored: Instant::now(),
            ttl,
            age_on_arrival: 0,
        }
    }

    #[test]
    fn nothing_is_remembered_about_a_resource_that_is_gone() {
        // The `Vary` map is keyed by resource and written on every store. If
        // it is not cleared when the last copy goes, a gateway serving pages
        // that expire accumulates one entry per URL it has ever seen, for as
        // long as the process runs — a leak that no byte budget bounds,
        // because the budget only counts bodies.
        let store = Store::new(1024);
        for index in 0..200 {
            let key = Key::new(&http::Method::GET, "example.com", &format!("/{index}"));
            store.put(
                key.clone(),
                String::new(),
                vec!["accept-encoding".to_string()],
                entry(Duration::ZERO),
            );
            // Reading it is what discovers it is stale and drops it.
            assert!(store.get(&key, "").is_none());
        }
        assert!(store.is_empty(), "no bodies are held");
        assert_eq!(
            store.remembered_resources(),
            0,
            "and nothing is remembered about them either"
        );
    }
}
