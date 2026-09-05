//! `cache`: keep a response and serve it again.
//!
//! nginx's most-used module, and the one Gatel had no answer for. A gateway in
//! front of an application that renders the same page for everybody should not
//! render it for everybody.
//!
//! It runs in the request phase, which puts it after every guard and outside
//! the terminal. Both halves of that matter. After the guards, because serving
//! a cached copy to a request that `basic-auth` or `ip-filter` would have
//! refused turns a cache into a way past them. Outside the terminal — and
//! outside `encode`, which is a response-phase directive — because the thing
//! worth storing is the answer as it goes on the wire, compression and all,
//! and `Vary: accept-encoding` is what keeps one client's copy from reaching
//! another.
//!
//! # What it does not do
//!
//! It does not revalidate. A stale entry is a miss, and the origin is asked
//! again in full rather than with `If-None-Match` — which is what nginx does
//! without `proxy_cache_revalidate`. It is worth adding and is not here.
//!
//! It does not coalesce. Ten simultaneous requests for one uncached page are
//! ten requests to the origin — nginx has `proxy_cache_lock` for this, and a
//! cache that stampedes on a cold start is worth knowing about before it
//! happens rather than after.
//!
//! Expired entries are reclaimed when something reads them or when the budget
//! forces an eviction, not by a sweeper. So a cache full of things nobody asks
//! for again holds its bytes until eviction reaches them. That is bounded, and
//! it is not prompt.
//!
//! It is per-process and in memory. Two gateways behind a balancer keep two
//! caches, and a restart starts empty. Two routes that each write `cache` get
//! a store each, so the memory a configuration can use is the sum of every
//! `max-size` in it.

mod policy;
mod store;

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use gatel_macros::FromNode;
use gatel_model::{ByteSize, Code, Diagnostic, Diagnostics, FromNode as _, Hasher, Node};
use gatel_plan::{Directive, DirectiveConfig, DirectiveMeta, DirectiveState, Phase, Scopes};
pub use policy::Refusal;
use salvo_core::http::{Request, ResBody, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};
pub use store::Store;

#[derive(Debug, FromNode)]
struct Fields {
    /// Everything held, together.
    #[gatel(prop, default = ByteSize::mib(64))]
    max_size: ByteSize,
    /// The largest single response worth keeping.
    #[gatel(prop, default = ByteSize::mib(1))]
    max_entry: ByteSize,
    /// How long to keep a response that does not say for itself.
    ///
    /// Absent means "do not keep it", which is what nginx does without
    /// `proxy_cache_valid`: a cache that invents a lifetime for a response
    /// that declined to state one is a cache that serves stale pages nobody
    /// asked it to.
    #[gatel(prop)]
    default_ttl: Option<Duration>,
}

#[derive(Debug)]
pub struct Config {
    max_size: u64,
    max_entry: u64,
    default_ttl: Option<Duration>,
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_u64(self.max_size);
        hasher.update_u64(self.max_entry);
        match self.default_ttl {
            Some(ttl) => hasher.update_duration(ttl),
            None => hasher.update_none(),
        };
    }

    fn state(&self) -> Option<DirectiveState> {
        // What is already stored stays stored across a reload. Everything held
        // was decided by the response that produced it, so changing a size
        // bound does not make any of it wrong — and emptying the cache on a
        // reload would send every request to the origin at once, which is the
        // one moment a gateway can least afford it.
        //
        // Nothing goes into the compatibility hash, so no change to this
        // directive throws the cache away. A changed budget is applied by
        // eviction on the next write.
        let max_size = self.max_size;
        Some(DirectiveState {
            compatibility: Hasher::new().finish(),
            create: Box::new(move || Arc::new(Store::new(max_size)) as Arc<dyn Any + Send + Sync>),
        })
    }

    fn build(&self, cx: gatel_plan::BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        let store: Arc<Store> = cx.state("the cache")?;
        Ok(Arc::new(CacheHandler {
            store,
            max_entry: self.max_entry,
            default_ttl: self.default_ttl,
        }))
    }

    fn describe(&self) -> String {
        match self.default_ttl {
            Some(ttl) => format!("cache up to {} bytes, {ttl:?} by default", self.max_size),
            None => format!("cache up to {} bytes", self.max_size),
        }
    }
}

pub struct Cache;

impl Directive for Cache {
    fn meta(&self) -> &DirectiveMeta {
        // The request phase: after every guard, so a cached copy is never a
        // way past one, and outside the terminal, so there is a response to
        // store.
        static META: DirectiveMeta = DirectiveMeta::middleware("cache", Phase::Request, Scopes::BOTH);
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let fields = Fields::from_node(node, diagnostics)?;
        if fields.max_entry.bytes() > fields.max_size.bytes() {
            diagnostics.push(
                Diagnostic::error(
                    Code::InvalidValue,
                    node.origin.clone(),
                    "`max-entry` is larger than `max-size`, so nothing would ever be kept",
                )
                .with_help("raise `max-size`, or lower `max-entry`"),
            );
            return None;
        }
        if fields.max_size.bytes() == 0 {
            diagnostics.error(
                Code::InvalidValue,
                node.origin.clone(),
                "`max-size` must be more than nothing; remove `cache` to turn it off",
            );
            return None;
        }
        Some(Arc::new(Config {
            max_size: fields.max_size.bytes(),
            max_entry: fields.max_entry.bytes(),
            default_ttl: fields.default_ttl,
        }))
    }
}

struct CacheHandler {
    store: Arc<Store>,
    max_entry: u64,
    default_ttl: Option<Duration>,
}

impl CacheHandler {
    /// What this request would be stored under, before `Vary`.
    fn key(&self, request: &Request) -> store::Key {
        let host = crate::authority(request).unwrap_or("");
        let target = request
            .uri()
            .path_and_query()
            .map(|path| path.as_str())
            .unwrap_or("/");
        store::Key::new(request.method(), host, target)
    }

    /// Replay a stored response.
    fn serve(&self, entry: &store::Entry, response: &mut Response) {
        response.status_code(StatusCode::from_u16(entry.status).unwrap_or(StatusCode::OK));
        *response.headers_mut() = entry.headers.clone();
        // How long it has been sitting here, plus however long it had already
        // been somewhere else. Age accumulates along a chain of caches; a
        // gateway that reports only its own resident time tells a browser a
        // response is fresher than it is, and the error grows with every hop.
        let age = entry.age().as_secs() + entry.age_on_arrival;
        if let Ok(value) = http::HeaderValue::from_str(&age.to_string()) {
            response.headers_mut().insert(http::header::AGE, value);
        }
        status_header(
            response,
            &format!("hit; ttl={}", entry.ttl.as_secs().saturating_sub(age)),
        );
        response.body(ResBody::Once(entry.body.clone()));
    }
}

/// How old the response already was when it got here.
///
/// A response from an origin that is itself behind a cache arrives with an
/// `Age`, and RFC 9111 says age accumulates: what a client is told has to be
/// this plus however long it then sat here.
fn arrived_with_age(response: &Response) -> u64 {
    response
        .headers()
        .get(http::header::AGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

/// RFC 9211 `Cache-Status`, so "why did this not cache" has an answer on the
/// response rather than only in a log nobody is reading at the time.
fn status_header(response: &mut Response, detail: &str) {
    if let Ok(value) = http::HeaderValue::from_str(&format!("gatel; {detail}")) {
        response.headers_mut().insert("cache-status", value);
    }
}

#[async_trait]
impl Handler for CacheHandler {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        if let Err(refusal) = policy::request_is_cacheable(request) {
            ctrl.call_next(request, depot, response).await;
            status_header(response, &format!("fwd=uri-miss; detail={}", refusal.as_str()));
            return;
        }

        let key = self.key(request);
        let vary = self.store.vary_for(&key).unwrap_or_default();
        let signature = store::signature(request.headers(), &vary);

        if policy::may_serve_from_cache(request)
            && let Some(entry) = self.store.get(&key, &signature)
        {
            // The terminal is never reached, which is the point.
            crate::generated(depot);
            self.serve(&entry, response);
            ctrl.skip_rest();
            return;
        }

        ctrl.call_next(request, depot, response).await;

        let ttl = match policy::freshness(request, response, self.default_ttl) {
            Ok(ttl) => ttl,
            Err(refusal) => {
                status_header(response, &format!("fwd=miss; detail={}", refusal.as_str()));
                return;
            }
        };

        // The body has to be in hand to be stored, and it may be a stream. Read
        // it up to the limit; anything larger is forwarded untouched rather
        // than held, because one response is not worth the whole budget.
        let body = std::mem::take(response.body_mut());
        let collected = match collect(body, self.max_entry).await {
            Collected::Whole(bytes) => bytes,
            Collected::TooLarge(body) => {
                *response.body_mut() = body;
                status_header(
                    response,
                    &format!("fwd=miss; detail={}", Refusal::TooLarge.as_str()),
                );
                return;
            }
            Collected::Failed(body) => {
                // Reading it failed part way. Hand back what there is and store
                // nothing: half a response is not a response.
                *response.body_mut() = body;
                status_header(response, "fwd=miss; detail=body-error");
                return;
            }
        };

        // `Vary` is a property of the response, so the signature this copy
        // answers is only known now.
        let vary = policy::vary_on(response);
        let signature = store::signature(request.headers(), &vary);
        self.store.put(
            key,
            signature,
            vary,
            store::Entry {
                status: response.status_code.unwrap_or(StatusCode::OK).as_u16(),
                headers: response.headers().clone(),
                body: collected.clone(),
                stored: std::time::Instant::now(),
                ttl,
                age_on_arrival: arrived_with_age(response),
            },
        );

        status_header(response, "fwd=miss; stored");
        *response.body_mut() = ResBody::Once(collected);
    }
}

enum Collected {
    Whole(Bytes),
    TooLarge(ResBody),
    Failed(ResBody),
}

/// Read a body into memory, giving up if it passes `limit`.
///
/// A body that is too large is handed back whole rather than truncated: the
/// client is owed the response either way, and only the caching is abandoned.
async fn collect(body: ResBody, limit: u64) -> Collected {
    use http_body_util::BodyExt;

    // Already in memory, and cheap to measure.
    if let ResBody::Once(bytes) = &body {
        return if bytes.len() as u64 > limit {
            Collected::TooLarge(body)
        } else {
            Collected::Whole(bytes.clone())
        };
    }
    if matches!(body, ResBody::None) {
        return Collected::Whole(Bytes::new());
    }

    match body.collect().await {
        Ok(collected) => {
            let bytes = collected.to_bytes();
            if bytes.len() as u64 > limit {
                // Read, and too big after all — a stream that declared nothing.
                // The client still gets it; it is simply not kept.
                Collected::TooLarge(ResBody::Once(bytes))
            } else {
                Collected::Whole(bytes)
            }
        }
        Err(_) => Collected::Failed(ResBody::None),
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, Value};

    use super::*;

    fn origin() -> Origin {
        Origin::synthetic("test")
    }

    fn decode(node: Node) -> (Option<Arc<dyn DirectiveConfig>>, Diagnostics) {
        let mut diagnostics = Diagnostics::new();
        let config = Cache.decode(&node, &mut diagnostics);
        (config, diagnostics)
    }

    #[test]
    fn it_is_a_request_phase_directive() {
        // After the guards, so a cached copy is never a way past one.
        assert_eq!(Cache.meta().role, gatel_plan::Role::Middleware(Phase::Request));
    }

    #[test]
    fn the_defaults_are_bounded() {
        let (config, diagnostics) = decode(Node::new("cache", origin()));
        assert!(config.is_some(), "{diagnostics}");
        assert!(config.unwrap().describe().contains("cache up to"));
    }

    #[test]
    fn an_entry_bound_above_the_total_is_refused() {
        // It would compile and then never store anything, which is worse than
        // not compiling.
        let node = Node::new("cache", origin())
            .with_prop("max-size", Value::string("1MiB", origin()))
            .with_prop("max-entry", Value::string("8MiB", origin()));
        let (config, diagnostics) = decode(node);
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("nothing would ever be kept")),
            "{diagnostics}"
        );
    }

    #[test]
    fn a_cache_that_holds_nothing_is_refused() {
        let node = Node::new("cache", origin())
            .with_prop("max-size", Value::string("0", origin()))
            .with_prop("max-entry", Value::string("0", origin()));
        assert!(decode(node).0.is_none());
    }

    #[tokio::test]
    async fn a_body_already_in_memory_is_taken_as_it_is() {
        let body = ResBody::Once(Bytes::from_static(b"hello"));
        assert!(matches!(collect(body, 1024).await, Collected::Whole(bytes) if bytes == "hello"));
    }

    #[tokio::test]
    async fn a_body_over_the_limit_is_handed_back_rather_than_cut() {
        // The client is owed the whole response either way; only the caching
        // is abandoned.
        let body = ResBody::Once(Bytes::from(vec![b'x'; 100]));
        assert!(matches!(collect(body, 10).await, Collected::TooLarge(_)));
    }

    #[tokio::test]
    async fn an_empty_body_is_a_body() {
        // A 204 has nothing in it and is still worth storing.
        assert!(matches!(
            collect(ResBody::None, 1024).await,
            Collected::Whole(bytes) if bytes.is_empty()
        ));
    }
}
