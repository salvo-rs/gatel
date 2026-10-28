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

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;

/// What a request is looked up by, before `Vary` is taken into account.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub method: String,
    pub namespace: String,
    pub scheme: String,
    pub host: String,
    pub target: String,
}

impl Key {
    pub fn new(method: &http::Method, scheme: &str, host: &str, target: &str) -> Self {
        Self {
            method: method.as_str().to_string(),
            namespace: String::new(),
            scheme: scheme.to_owned(),
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
    /// Corrected initial age from Date, Age, and downstream response delay.
    pub age_on_arrival: Duration,
}

impl Entry {
    pub fn age(&self) -> Duration {
        self.stored.elapsed()
    }

    pub fn is_fresh(&self) -> bool {
        self.current_age() < self.ttl
    }

    pub fn current_age(&self) -> Duration {
        self.age_on_arrival.saturating_add(self.age())
    }

    /// Roughly what this costs to hold. Headers are counted because a response
    /// with a hundred of them is not free just because its body is empty.
    fn weight(&self, key: &Key, signature: &str, vary: &[String]) -> u64 {
        let headers: usize = self
            .headers
            .iter()
            .map(|(name, value)| name.as_str().len() + value.len() + 64)
            .sum();
        // Logical retained-data charge, not an exact allocator/RSS reading.
        // Nonzero base bounds even zero-body/header entries. Charge duplicated
        // map keys and per-variant metadata conservatively for every copy.
        let keys =
            key.method.len() + key.namespace.len() + key.scheme.len() + key.host.len() + key.target.len();
        256u64
            .saturating_add(self.body.len() as u64)
            .saturating_add(headers as u64)
            .saturating_add((keys as u64).saturating_mul(2))
            .saturating_add(signature.len() as u64)
            .saturating_add(vary.iter().map(|name| name.len() as u64 + 24).sum::<u64>())
    }
}

/// A stored response together with the request headers it was chosen by.
#[derive(Debug)]
struct Held {
    identity: Arc<()>,
    entry: Entry,
    weight: u64,
    /// Which `Vary` signature this copy answers.
    signature: String,
    /// When it was last handed out, for eviction.
    used: u64,
}

pub(super) struct Observed {
    key: Key,
    signature: String,
    identity: Arc<()>,
}

pub(super) struct Retirement<'a> {
    store: &'a Store,
    observed: Option<Observed>,
}

impl Drop for Retirement<'_> {
    fn drop(&mut self) {
        if let Some(observed) = self.observed.take() {
            self.store.discard_observed(observed);
        }
    }
}

#[derive(Debug, Default)]
struct Contents {
    held: HashMap<Key, Vec<Held>>,
    vary: HashMap<Key, Vec<String>>,
    revision: u64,
}

/// The shared store, which outlives any generation that uses it.
#[derive(Debug)]
pub struct Store {
    pub(super) fills: Arc<super::fill::Budget>,
    /// Every copy of a resource, by base key and then by `Vary` signature.
    ///
    /// Two levels because `Vary` is a property of the *response*: the headers
    /// a request must match on are not known until a response for that
    /// resource has been seen once.
    contents: Mutex<Contents>,
    /// Stored payload/header bytes, updated under the contents lock.
    bytes: AtomicU64,
    max_bytes: u64,
    clock: AtomicU64,
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub evictions: AtomicU64,
}

impl Store {
    pub(super) fn observe(&self, key: &Key, headers: &http::HeaderMap) -> Option<Observed> {
        let contents = self.contents.lock().unwrap_or_else(|p| p.into_inner());
        let signature = signature(headers, contents.vary.get(key)?);
        let copy = contents
            .held
            .get(key)?
            .iter()
            .find(|copy| copy.signature == signature)?;
        Some(Observed {
            key: key.clone(),
            signature,
            identity: copy.identity.clone(),
        })
    }

    pub(super) fn retire_on_drop(&self, observed: Option<Observed>) -> Retirement<'_> {
        Retirement {
            store: self,
            observed,
        }
    }

    fn discard_observed(&self, observed: Observed) {
        let mut contents = self.contents.lock().unwrap_or_else(|p| p.into_inner());
        let Some(copies) = contents.held.get_mut(&observed.key) else {
            return;
        };
        let Some(index) = copies.iter().position(|copy| {
            copy.signature == observed.signature && Arc::ptr_eq(&copy.identity, &observed.identity)
        }) else {
            return;
        };
        let old = copies.remove(index);
        self.bytes.fetch_sub(old.weight, Ordering::Relaxed);
        if copies.is_empty() {
            contents.held.remove(&observed.key);
            contents.vary.remove(&observed.key);
        }
        // Cancel older fills too, using the existing bounded global revision.
        contents.revision = contents.revision.saturating_add(1);
    }

    pub fn new(max_bytes: u64) -> Self {
        Self {
            fills: Arc::new(super::fill::Budget::new(max_bytes)),
            contents: Mutex::new(Contents::default()),
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
    pub fn take_stale(&self, key: &Key, headers: &http::HeaderMap) -> Option<(u64, Entry)> {
        let mut contents = self.contents.lock().unwrap_or_else(|p| p.into_inner());
        let signature = signature(headers, contents.vary.get(key)?);
        let copies = contents.held.get_mut(key)?;
        let index = copies
            .iter()
            .position(|copy| copy.signature == signature && !copy.entry.is_fresh())?;
        let held = copies.remove(index);
        self.bytes.fetch_sub(held.weight, Ordering::Relaxed);
        if copies.is_empty() {
            contents.held.remove(key);
            contents.vary.remove(key);
        }
        Some((contents.revision, held.entry))
    }

    #[cfg(test)]
    pub fn vary_for(&self, key: &Key) -> Option<Vec<String>> {
        self.contents
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .vary
            .get(key)
            .cloned()
    }

    /// A fresh entry for this key and signature, if there is one.
    ///
    /// A stale entry is dropped as it is found: keeping it would only mean
    /// holding bytes nobody may be served.
    #[cfg(test)]
    pub fn get(&self, key: &Key, signature: &str) -> Option<Entry> {
        self.lookup(key, |_| signature.to_owned(), |_| true)
    }

    pub fn get_matching(
        &self,
        key: &Key,
        headers: &http::HeaderMap,
        accepts: impl Fn(&Entry) -> bool,
    ) -> Option<Entry> {
        self.lookup(key, |vary| signature(headers, vary), accepts)
    }

    fn lookup(
        &self,
        key: &Key,
        signature_for: impl FnOnce(&[String]) -> String,
        accepts: impl Fn(&Entry) -> bool,
    ) -> Option<Entry> {
        let mut contents = self.contents.lock().unwrap_or_else(|p| p.into_inner());
        let Contents { held, vary, .. } = &mut *contents;
        let signature = signature_for(vary.get(key).map(Vec::as_slice).unwrap_or_default());
        let Some(copies) = held.get_mut(key) else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };

        let mut freed = 0;
        copies.retain(|copy| {
            if copy.entry.is_fresh() {
                return true;
            }
            freed += copy.weight;
            false
        });
        if freed > 0 {
            self.bytes.fetch_sub(freed, Ordering::Relaxed);
        }

        let now = self.clock.fetch_add(1, Ordering::Relaxed);
        let found = copies
            .iter_mut()
            .find(|copy| copy.signature == signature && accepts(&copy.entry))
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
            vary.remove(key);
        }
        drop(contents);

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
    #[cfg(test)]
    pub fn put(&self, key: Key, signature: String, vary: Vec<String>, entry: Entry) {
        self.put_if_current(self.revision(), key, signature, vary, entry);
    }

    pub fn revision(&self) -> u64 {
        self.contents.lock().unwrap_or_else(|p| p.into_inner()).revision
    }

    /// Invalidate all method/variant copies of the effective request URI.
    /// A store-wide revision conservatively cancels other in-flight fills too,
    /// without an unbounded per-URI tombstone table.
    pub fn invalidate(&self, key: &Key) {
        let mut contents = self.contents.lock().unwrap_or_else(|p| p.into_inner());
        contents.revision = contents.revision.saturating_add(1);
        let mut freed = 0;
        contents.held.retain(|candidate, copies| {
            if candidate.scheme == key.scheme && candidate.host == key.host && candidate.target == key.target
            {
                freed += copies.iter().map(|copy| copy.weight).sum::<u64>();
                false
            } else {
                true
            }
        });
        contents.vary.retain(|candidate, _| {
            candidate.scheme != key.scheme || candidate.host != key.host || candidate.target != key.target
        });
        self.bytes.fetch_sub(freed, Ordering::Relaxed);
    }

    pub fn put_if_current(
        &self,
        revision: u64,
        key: Key,
        signature: String,
        vary: Vec<String>,
        entry: Entry,
    ) -> bool {
        self.put_guarded(revision, key, signature, vary, entry, false)
    }

    pub fn put_guarded(
        &self,
        revision: u64,
        key: Key,
        signature: String,
        vary: Vec<String>,
        entry: Entry,
        only_if_absent: bool,
    ) -> bool {
        let weight = entry.weight(&key, &signature, &vary);
        if weight > self.max_bytes {
            // Nothing to do but refuse: making room for it would empty the
            // whole cache for one response.
            return false;
        }

        let mut contents = self.contents.lock().unwrap_or_else(|p| p.into_inner());
        if contents.revision != revision || revision == u64::MAX {
            return false;
        }
        if only_if_absent
            && (contents.vary.get(&key).is_some_and(|old| old != &vary)
                || contents
                    .held
                    .get(&key)
                    .is_some_and(|copies| copies.iter().any(|copy| copy.signature == signature)))
        {
            return false;
        }
        let Contents {
            held, vary: varies, ..
        } = &mut *contents;
        if varies.get(&key).is_some_and(|old| old != &vary)
            && let Some(old) = held.remove(&key)
        {
            let freed: u64 = old.iter().map(|copy| copy.weight).sum();
            self.bytes.fetch_sub(freed, Ordering::Relaxed);
        }
        varies.insert(key.clone(), vary);
        let now = self.clock.fetch_add(1, Ordering::Relaxed);
        let copies = held.entry(key).or_default();
        if let Some(existing) = copies.iter_mut().find(|copy| copy.signature == signature) {
            existing.identity = Arc::new(());
            self.bytes.fetch_sub(existing.weight, Ordering::Relaxed);
            existing.weight = weight;
            existing.entry = entry;
            existing.used = now;
        } else {
            copies.push(Held {
                identity: Arc::new(()),
                entry,
                weight,
                signature,
                used: now,
            });
        }
        self.bytes.fetch_add(weight, Ordering::Relaxed);
        self.evict_down_to_budget(&mut contents);
        true
    }

    /// Throw away least-recently-used copies until the budget is met.
    ///
    /// Collect candidates once, then remove the oldest sufficient set in one
    /// pass per resource. Re-scanning for each victim made one large insertion
    /// quadratic, including when many `Vary` copies shared a base key. The
    /// temporary ordering exists only while over budget, so ordinary hits and
    /// writes do not maintain a second, potentially stale identity index.
    fn evict_down_to_budget(&self, contents: &mut Contents) {
        let excess = self.bytes.load(Ordering::Relaxed).saturating_sub(self.max_bytes);
        if excess == 0 {
            return;
        }
        let Contents { held, vary, .. } = contents;
        let victims = {
            let mut candidates: Vec<_> = held
                .iter()
                .flat_map(|(key, copies)| copies.iter().map(move |copy| (key, copy)))
                .collect();
            candidates.sort_unstable_by_key(|(_, copy)| copy.used);
            let mut victims: HashMap<Key, HashSet<String>> = HashMap::new();
            let mut selected = 0u64;
            for (key, copy) in candidates {
                victims
                    .entry(key.clone())
                    .or_default()
                    .insert(copy.signature.clone());
                selected = selected.saturating_add(copy.weight);
                if selected >= excess {
                    break;
                }
            }
            victims
        };
        let mut freed = 0u64;
        let mut removed = 0u64;
        for (key, signatures) in victims {
            if let Some(copies) = held.get_mut(&key) {
                copies.retain(|copy| {
                    if !signatures.contains(&copy.signature) {
                        return true;
                    }
                    freed += copy.weight;
                    removed += 1;
                    false
                });
                if copies.is_empty() {
                    held.remove(&key);
                    vary.remove(&key);
                }
            }
        }
        self.bytes.fetch_sub(freed, Ordering::Relaxed);
        self.evictions.fetch_add(removed, Ordering::Relaxed);
    }

    pub fn held_bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    pub fn len(&self) -> usize {
        self.contents
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .held
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
        self.contents.lock().unwrap_or_else(|p| p.into_inner()).vary.len()
    }
}

#[cfg(test)]
#[path = "eviction_tests.rs"]
mod eviction_tests;

/// The signature a request has for a given `Vary` list.
///
/// A header the request does not send is recorded as absent rather than
/// skipped, so "no Accept-Encoding" and "Accept-Encoding: gzip" cannot collide.
pub fn signature(headers: &http::HeaderMap, vary: &[String]) -> String {
    if vary.is_empty() {
        return String::new();
    }
    let mut hasher = gatel_model::Hasher::new();
    hasher.update_str("cache-vary-v2");
    hasher.update_u64(vary.len() as u64);
    for name in vary {
        hasher.update_str(name);
        let values = headers.get_all(name.as_str());
        hasher.update_u64(values.iter().count() as u64);
        for value in values {
            hasher.update_bytes(value.as_bytes());
        }
    }
    hasher.finish().to_string()
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
            age_on_arrival: Duration::ZERO,
        }
    }

    fn key(target: &str) -> Key {
        Key::new(&http::Method::GET, "http", "example.com", target)
    }

    const MINUTE: Duration = Duration::from_secs(60);

    #[test]
    fn retirement_discards_only_observed_identity_and_cancels_old_fills() {
        let store = Store::new(4096);
        let headers = http::HeaderMap::new();
        store.put(key("/a"), String::new(), vec![], entry("old", MINUTE));
        let observed = store.observe(&key("/a"), &headers);
        // Access changes LRU state, not representation identity.
        assert!(store.get(&key("/a"), "").is_some());
        let revision = store.revision();
        drop(store.retire_on_drop(observed));
        assert_eq!(store.len(), 0);
        assert_eq!(store.remembered_resources(), 0);
        assert_eq!(store.bytes.load(Ordering::Relaxed), 0);
        assert!(!store.put_if_current(revision, key("/a"), String::new(), vec![], entry("late", MINUTE)));

        store.put(key("/a"), String::new(), vec![], entry("old", MINUTE));
        let observed = store.observe(&key("/a"), &headers);
        store.put(key("/a"), String::new(), vec![], entry("replacement", MINUTE));
        let revision = store.revision();
        let bytes = store.bytes.load(Ordering::Relaxed);
        drop(store.retire_on_drop(observed));
        assert_eq!(store.get(&key("/a"), "").unwrap().body, "replacement");
        assert_eq!(store.bytes.load(Ordering::Relaxed), bytes);
        assert_eq!(store.revision(), revision);

        let vary = vec!["accept-language".to_owned()];
        let mut english = http::HeaderMap::new();
        english.insert("accept-language", "en".parse().unwrap());
        let mut french = http::HeaderMap::new();
        french.insert("accept-language", "fr".parse().unwrap());
        for (headers, body) in [(&english, "English"), (&french, "French")] {
            store.put(
                key("/vary"),
                signature(headers, &vary),
                vary.clone(),
                entry(body, MINUTE),
            );
        }
        let observed = store.observe(&key("/vary"), &english);
        drop(store.retire_on_drop(observed));
        assert!(store.get_matching(&key("/vary"), &english, |_| true).is_none());
        assert_eq!(
            store.get_matching(&key("/vary"), &french, |_| true).unwrap().body,
            "French"
        );
        assert_eq!(store.vary_for(&key("/vary")), Some(vary));
    }

    #[test]
    fn taken_stale_candidate_cannot_overwrite_a_concurrent_replacement_or_invalidation() {
        let store = Store::new(4096);
        store.put(
            key("/a"),
            String::new(),
            Vec::new(),
            entry("stale", Duration::ZERO),
        );
        let (revision, _) = store.take_stale(&key("/a"), &http::HeaderMap::new()).unwrap();
        assert_eq!(store.held_bytes(), 0);
        store.put(key("/a"), String::new(), Vec::new(), entry("new", MINUTE));
        assert!(!store.put_guarded(
            revision,
            key("/a"),
            String::new(),
            Vec::new(),
            entry("old304", MINUTE),
            true
        ));
        assert_eq!(store.get(&key("/a"), "").unwrap().body, "new");
        store.invalidate(&key("/a"));
        assert!(!store.put_guarded(
            revision,
            key("/a"),
            String::new(),
            Vec::new(),
            entry("old304", MINUTE),
            true
        ));
    }

    #[test]
    fn empty_responses_and_large_metadata_cannot_escape_the_budget() {
        let store = Store::new(1024);
        for index in 0..1000 {
            store.put(
                key(&format!("/empty/{index}")),
                String::new(),
                Vec::new(),
                entry("", MINUTE),
            );
            assert!(store.held_bytes() <= 1024);
            assert!(store.len() <= 4);
            assert_eq!(store.remembered_resources(), store.len());
        }
        assert!(store.evictions.load(Ordering::Relaxed) > 0);
        let before = store.held_bytes();
        for (target, signature, vary) in [
            (format!("/{}", "x".repeat(2048)), String::new(), Vec::new()),
            ("/sig".to_owned(), "s".repeat(2048), Vec::new()),
            ("/vary".to_owned(), String::new(), vec!["x".repeat(2048)]),
        ] {
            assert!(!store.put_if_current(
                store.revision(),
                key(&target),
                signature,
                vary,
                entry("", MINUTE)
            ));
            assert_eq!(
                store.held_bytes(),
                before,
                "refused entries must not evict existing data"
            );
        }
    }

    #[test]
    fn http_and_https_entries_and_invalidation_are_isolated() {
        let store = Store::new(1024);
        let http = Key::new(&http::Method::GET, "http", "example.com", "/");
        let https = Key::new(&http::Method::GET, "https", "example.com", "/");
        store.put(http.clone(), String::new(), Vec::new(), entry("plain", MINUTE));
        store.put(https.clone(), String::new(), Vec::new(), entry("secure", MINUTE));
        assert_eq!(store.get(&http, "").unwrap().body, "plain");
        assert_eq!(store.get(&https, "").unwrap().body, "secure");
        store.invalidate(&http);
        assert!(store.get(&http, "").is_none());
        assert_eq!(store.get(&https, "").unwrap().body, "secure");
    }

    #[test]
    fn invalidation_removes_all_methods_and_prevents_old_fills_without_tombstones() {
        let store = Store::new(1024);
        let before = store.revision();
        store.put(key("/a"), String::new(), Vec::new(), entry("get", MINUTE));
        let head = Key::new(&http::Method::HEAD, "http", "example.com", "/a");
        store.put(head.clone(), String::new(), Vec::new(), entry("head", MINUTE));
        store.put(key("/b"), String::new(), Vec::new(), entry("other", MINUTE));
        store.invalidate(&key("/a"));
        assert!(store.get(&key("/a"), "").is_none());
        assert!(store.get(&head, "").is_none());
        assert_eq!(
            store.held_bytes(),
            entry("other", MINUTE).weight(&key("/b"), "", &[])
        );
        assert_eq!(store.remembered_resources(), 1);
        assert!(!store.put_if_current(
            before,
            key("/a"),
            String::new(),
            Vec::new(),
            entry("stale", MINUTE)
        ));
        let empty_revision = store.revision();
        store.invalidate(&key("/not-yet-stored"));
        assert!(!store.put_if_current(
            empty_revision,
            key("/not-yet-stored"),
            String::new(),
            Vec::new(),
            entry("stale", MINUTE)
        ));
        assert!(store.put_if_current(
            store.revision(),
            key("/a"),
            String::new(),
            Vec::new(),
            entry("new", MINUTE)
        ));
    }

    #[test]
    fn vary_keys_include_all_raw_values_and_changed_dimensions_replace_old_copies() {
        let vary = vec!["x-variant".to_owned()];
        let mut first = http::HeaderMap::new();
        first.append("x-variant", http::HeaderValue::from_static("common"));
        first.append("x-variant", http::HeaderValue::from_static("one"));
        let mut second = first.clone();
        second.remove("x-variant");
        second.append("x-variant", http::HeaderValue::from_static("common"));
        second.append("x-variant", http::HeaderValue::from_static("two"));
        assert_ne!(signature(&first, &vary), signature(&second, &vary));
        second.insert("x-variant", http::HeaderValue::from_bytes(b"\xff").unwrap());
        assert_ne!(
            signature(&second, &vary),
            signature(&http::HeaderMap::new(), &vary)
        );
        let store = Store::new(1024);
        store.put(
            key("/a"),
            signature(&first, &vary),
            vary,
            entry("private", MINUTE),
        );
        store.put(key("/a"), String::new(), Vec::new(), entry("public", MINUTE));
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.held_bytes(),
            entry("public", MINUTE).weight(&key("/a"), "", &[])
        );
        assert_eq!(
            store.get_matching(&key("/a"), &first, |_| true).unwrap().body,
            "public"
        );
    }

    #[test]
    fn concurrent_vary_replacement_and_lookup_keep_metadata_and_body_together() {
        let store = std::sync::Arc::new(Store::new(1024));
        let reader = store.clone();
        let task = std::thread::spawn(move || {
            for _ in 0..2000 {
                if let Some(found) = reader.get_matching(&key("/race"), &http::HeaderMap::new(), |_| true) {
                    assert_eq!(found.body, "public");
                }
            }
        });
        for _ in 0..1000 {
            let vary = vec!["x-variant".to_owned()];
            let mut headers = http::HeaderMap::new();
            headers.insert("x-variant", http::HeaderValue::from_static("secret"));
            store.put(
                key("/race"),
                signature(&headers, &vary),
                vary,
                entry("private", MINUTE),
            );
            store.put(key("/race"), String::new(), Vec::new(), entry("public", MINUTE));
        }
        task.join().unwrap();
        assert_eq!(store.remembered_resources(), 1);
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.held_bytes(),
            entry("public", MINUTE).weight(&key("/race"), "", &[])
        );
    }

    #[test]
    fn age_includes_upstream_and_rejected_candidates_are_misses() {
        let mut old = entry("old", MINUTE);
        old.age_on_arrival = Duration::from_secs(60);
        assert!(!old.is_fresh());
        old.age_on_arrival = Duration::MAX;
        assert!(!old.is_fresh());
        let store = Store::new(1024);
        store.put(key("/a"), String::new(), Vec::new(), entry("hello", MINUTE));
        assert!(
            store
                .get_matching(&key("/a"), &http::HeaderMap::new(), |_| false)
                .is_none()
        );
        assert_eq!(store.hits.load(Ordering::Relaxed), 0);
        assert_eq!(store.misses.load(Ordering::Relaxed), 1);
        assert!(store.get(&key("/a"), "").is_some());
    }

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
            Key::new(&http::Method::GET, "http", "one.example", "/"),
            String::new(),
            Vec::new(),
            entry("one", MINUTE),
        );
        store.put(
            Key::new(&http::Method::GET, "http", "two.example", "/"),
            String::new(),
            Vec::new(),
            entry("two", MINUTE),
        );
        assert_eq!(
            store
                .get(&Key::new(&http::Method::GET, "http", "TWO.EXAMPLE", "/"), "")
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
        let store = Store::new(1024);
        for index in 0..20 {
            store.put(
                key(&format!("/{index}")),
                String::new(),
                Vec::new(),
                entry("0123456789", MINUTE),
            );
        }
        assert!(
            store.held_bytes() <= 1024,
            "held {} against a budget of 1024",
            store.held_bytes()
        );
        assert!(store.evictions.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn what_is_used_survives_what_is_not() {
        let budget = 4 * entry("0123456789", MINUTE).weight(&key("/filler0"), "", &[]);
        let store = Store::new(budget);
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
            age_on_arrival: Duration::ZERO,
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
            let key = Key::new(&http::Method::GET, "http", "example.com", &format!("/{index}"));
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
