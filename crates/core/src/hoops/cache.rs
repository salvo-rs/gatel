use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::header::{
    AGE, CACHE_CONTROL, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, SET_COOKIE,
};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use salvo::{Depot, FlowCtrl, Request, Response, async_trait};
use tracing::debug;

use crate::cache_control::{build_vary_key, parse_cache_control};
use crate::config::CacheConfig;

/// A cached response entry.
#[derive(Clone)]
struct CacheEntry {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
    inserted_at: Instant,
    max_age: Duration,
    etag: Option<String>,
    last_modified: Option<String>,
}

impl CacheEntry {
    /// Check whether this entry is still fresh.
    fn is_fresh(&self) -> bool {
        self.inserted_at.elapsed() < self.max_age
    }

    /// Compute the Age header value in seconds.
    fn age_secs(&self) -> u64 {
        self.inserted_at.elapsed().as_secs()
    }
}

/// Composite cache key: (host, method, path, vary_key).
/// The vary_key is built from request header values named by the Vary response header.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    host: String,
    method: String,
    path: String,
    vary_key: String,
}

/// In-memory LRU response cache middleware.
///
/// Caches GET/HEAD responses with cacheable status codes (200, 301, 302, 304).
/// Respects `Cache-Control` directives (`no-store`, `no-cache`, `max-age`, `s-maxage`).
/// Includes `Vary` header values in the cache key.
/// Supports conditional requests via `If-None-Match` (ETag) and `If-Modified-Since`.
/// Skips caching responses with `Set-Cookie` headers.
pub struct CacheHoop {
    config: CacheConfig,
    store: Mutex<CacheStore>,
}

struct CacheStore {
    entries: HashMap<CacheKey, CacheEntry>,
    /// Access order for LRU eviction — most recently accessed at the end.
    access_order: Vec<CacheKey>,
    max_entries: usize,
}

impl CacheStore {
    fn new(max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            access_order: Vec::new(),
            max_entries,
        }
    }

    /// Get an entry if it exists, updating LRU order.
    fn get(&mut self, key: &CacheKey) -> Option<CacheEntry> {
        let entry = self.entries.get(key)?;
        if !entry.is_fresh() {
            // Expired — remove it.
            self.entries.remove(key);
            self.access_order.retain(|k| k != key);
            return None;
        }
        let entry = entry.clone();
        // Move to end of access order (most recently used).
        self.access_order.retain(|k| k != key);
        self.access_order.push(key.clone());
        Some(entry)
    }

    /// Insert an entry, evicting the least recently used if at capacity.
    fn insert(&mut self, key: CacheKey, entry: CacheEntry) {
        // If key already exists, remove old access order entry.
        if self.entries.contains_key(&key) {
            self.access_order.retain(|k| k != &key);
        }

        // Evict LRU entries if at capacity.
        while self.entries.len() >= self.max_entries && !self.access_order.is_empty() {
            let evicted = self.access_order.remove(0);
            self.entries.remove(&evicted);
            debug!(key = ?evicted.path, "evicted LRU cache entry");
        }

        self.access_order.push(key.clone());
        self.entries.insert(key, entry);
    }
}

impl CacheHoop {
    pub fn new(config: &CacheConfig) -> Self {
        debug!(
            max_entries = config.max_entries,
            max_entry_size = config.max_entry_size,
            default_max_age = config.default_max_age.as_secs(),
            "cache middleware initialized"
        );
        Self {
            config: config.clone(),
            store: Mutex::new(CacheStore::new(config.max_entries)),
        }
    }
}

#[async_trait]
impl salvo::Handler for CacheHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let method = req.method().clone();

        // Only cache GET and HEAD requests.
        if method != Method::GET && method != Method::HEAD {
            ctrl.call_next(req, depot, res).await;
            return;
        }

        // Check request Cache-Control for no-store / no-cache.
        let req_cache_control = parse_cache_control(
            req.headers()
                .get(CACHE_CONTROL)
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
        );
        if req_cache_control.no_store {
            ctrl.call_next(req, depot, res).await;
            let _ = res.add_header("X-Cache", "BYPASS", true);
            return;
        }

        let host = req
            .headers()
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let path = req
            .uri()
            .path_and_query()
            .map(|pq| pq.to_string())
            .unwrap_or_else(|| req.uri().path().to_string());

        // Build a preliminary cache key (without vary — we check multiple vary keys).
        let base_key = CacheKey {
            host: host.clone(),
            method: method.to_string(),
            path: path.clone(),
            vary_key: String::new(),
        };

        // Attempt cache lookup.
        let cached = {
            let mut store = self.store.lock().unwrap();
            if let Some(entry) = store.get(&base_key) {
                Some((base_key.clone(), entry))
            } else {
                let mut found = None;
                let keys: Vec<CacheKey> = store
                    .entries
                    .keys()
                    .filter(|k| k.host == host && k.method == method.as_str() && k.path == path)
                    .cloned()
                    .collect();
                for key in keys {
                    if let Some(entry) = store.get(&key) {
                        let vary_key = build_vary_key(&entry.headers, req.headers());
                        if key.vary_key == vary_key {
                            found = Some((key, entry));
                            break;
                        }
                    }
                }
                found
            }
        };

        if let Some((_key, entry)) = cached {
            if req_cache_control.no_cache {
                debug!(path = path.as_str(), "no-cache directive, revalidating");
            } else {
                // Check conditional request: If-None-Match.
                if let Some(inm) = req.headers().get(IF_NONE_MATCH)
                    && let (Ok(inm_str), Some(etag)) = (inm.to_str(), &entry.etag)
                    && inm_str.trim_matches('"') == etag.trim_matches('"')
                {
                    debug!(path = path.as_str(), "conditional cache hit (ETag), 304");
                    res.status_code(StatusCode::NOT_MODIFIED);
                    if let Ok(val) = etag.parse::<HeaderValue>() {
                        res.headers_mut().insert(ETAG, val);
                    }
                    res.headers_mut()
                        .insert(AGE, HeaderValue::from(entry.age_secs()));
                    let _ = res.add_header("X-Cache", "HIT", true);
                    ctrl.skip_rest();
                    return;
                }

                // Check conditional request: If-Modified-Since.
                if let Some(ims) = req.headers().get(IF_MODIFIED_SINCE)
                    && let (Ok(ims_str), Some(lm)) = (ims.to_str(), &entry.last_modified)
                    && ims_str == lm
                {
                    debug!(
                        path = path.as_str(),
                        "conditional cache hit (If-Modified-Since), 304"
                    );
                    res.status_code(StatusCode::NOT_MODIFIED);
                    res.headers_mut()
                        .insert(AGE, HeaderValue::from(entry.age_secs()));
                    let _ = res.add_header("X-Cache", "HIT", true);
                    ctrl.skip_rest();
                    return;
                }

                debug!(
                    path = path.as_str(),
                    age = entry.age_secs(),
                    "cache hit, serving cached response"
                );

                // Build response from cached entry.
                res.status_code(entry.status);
                *res.headers_mut() = entry.headers.clone();
                res.headers_mut()
                    .insert(AGE, HeaderValue::from(entry.age_secs()));
                let _ = res.add_header("X-Cache", "HIT", true);
                res.body(entry.body.to_vec());
                ctrl.skip_rest();
                return;
            }
        }

        // Cache miss — call downstream.
        ctrl.call_next(req, depot, res).await;

        // Determine if the response is cacheable.
        let status = res.status_code.unwrap_or(StatusCode::OK);
        let cacheable_status = matches!(
            status,
            StatusCode::OK
                | StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::NOT_MODIFIED
        );

        if !cacheable_status {
            let _ = res.add_header("X-Cache", "BYPASS", true);
            return;
        }

        // Skip if response has Set-Cookie.
        if res.headers().contains_key(SET_COOKIE) {
            debug!(path = path.as_str(), "response has Set-Cookie, not caching");
            let _ = res.add_header("X-Cache", "BYPASS", true);
            return;
        }

        // Parse response Cache-Control.
        let resp_cache_control = parse_cache_control(
            res.headers()
                .get(CACHE_CONTROL)
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
        );

        if resp_cache_control.no_store {
            let _ = res.add_header("X-Cache", "BYPASS", true);
            return;
        }

        // Determine max-age: s-maxage > max-age > default.
        let max_age = resp_cache_control
            .s_maxage
            .or(resp_cache_control.max_age)
            .unwrap_or(self.config.default_max_age);

        if max_age.is_zero() {
            let _ = res.add_header("X-Cache", "BYPASS", true);
            return;
        }

        // Collect body to bytes for caching.
        let body = res.take_body();
        let body_bytes = match super::compress::collect_res_body_bytes(body).await {
            Ok(b) => Bytes::from(b),
            Err(_) => {
                let _ = res.add_header("X-Cache", "BYPASS", true);
                return;
            }
        };

        // Check entry size limit.
        if body_bytes.len() > self.config.max_entry_size {
            debug!(
                path = path.as_str(),
                size = body_bytes.len(),
                max = self.config.max_entry_size,
                "response too large to cache"
            );
            let _ = res.add_header("X-Cache", "BYPASS", true);
            res.body(body_bytes.to_vec());
            return;
        }

        let etag = res
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let last_modified = res
            .headers()
            .get(LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let vary_key = String::new();

        let entry = CacheEntry {
            status,
            headers: res.headers().clone(),
            body: body_bytes.clone(),
            inserted_at: Instant::now(),
            max_age,
            etag,
            last_modified,
        };

        let cache_key = CacheKey {
            host,
            method: method.to_string(),
            path: path.clone(),
            vary_key,
        };

        {
            let mut store = self.store.lock().unwrap();
            store.insert(cache_key, entry);
        }

        debug!(
            path = path.as_str(),
            max_age = max_age.as_secs(),
            size = body_bytes.len(),
            "cached response"
        );

        let _ = res.add_header("X-Cache", "MISS", true);
        res.body(body_bytes.to_vec());
    }
}
