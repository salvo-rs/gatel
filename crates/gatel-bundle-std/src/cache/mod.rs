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
//! Stale unencoded GET responses with an ETag or Last-Modified can be revalidated. A candidate
//! is taken by one request, and a valid 304 reconstructs its complete body.
//! Client-conditional requests, HEAD, encoded representations and entries
//! without valid validators still go to the origin without this optimization. There is
//! no stale-on-error fallback or automatic retry of an invalid 304.
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
//! a store each. Each store has a retained-data budget of `max-size` and a
//! separate unfinished-fill budget of `max-size` logical payload bytes, with
//! at most 64 fills. Unknown-length fills reserve `max-entry` before reading;
//! when no reservation fits the response streams uncached. These bounds do
//! not include transport buffers or allocator overhead.

mod age;
#[cfg(test)]
mod cancellation;
mod conditional;
mod control;
mod fill;
mod invalidation;
mod policy;
mod revalidate;
mod store;

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use fill::{Collected, collect};
use gatel_macros::FromNode;
use gatel_model::{ByteSize, Code, Diagnostic, Diagnostics, FromNode as _, Hasher, Node};
use gatel_plan::{Directive, DirectiveConfig, DirectiveMeta, DirectiveState, Phase, Scopes};
pub use policy::Refusal;
use salvo_core::http::{Request, ResBody, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};
pub use store::Store;

#[derive(Debug, FromNode)]
struct Fields {
    /// Retained-data budget including keys, headers, variants and entry overhead.
    /// This is not a hard limit on allocator overhead or process RSS.
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
        // Policy changes create a new generation-owned store. Mutating the
        // old store during prepare would affect live requests even if another
        // resource later fails. Unchanged policy retains warmed entries.
        let mut compatibility = Hasher::new();
        compatibility.update_str("cache-policy-state-v2");
        self.hash_into(&mut compatibility);
        let max_size = self.max_size;
        Some(DirectiveState {
            compatibility: compatibility.finish(),
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
        static META: DirectiveMeta = DirectiveMeta::middleware("cache", Phase::Request, Scopes::BOTH)
            .with_effects(&[gatel_plan::Effect::CacheLookup])
            .with_version("17");
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
    fn key(&self, request: &Request, depot: &Depot) -> store::Key {
        let host = crate::authority(request).unwrap_or("");
        let target = request
            .uri()
            .path_and_query()
            .map(|path| path.as_str())
            .unwrap_or("/");
        let scheme = if depot.get::<bool>("gatel.tls").copied().unwrap_or(false) {
            "https"
        } else {
            "http"
        };
        let mut key = store::Key::new(request.method(), scheme, host, target);
        key.namespace = depot
            .get::<gatel_plan::ExecutionIdentity>(gatel_plan::ExecutionIdentity::DEPOT_KEY)
            .map(|identity| identity.0.to_string())
            .unwrap_or_default();
        key
    }

    /// Replay a stored response.
    fn serve(&self, entry: &store::Entry, response: &mut Response) {
        response.status_code(StatusCode::from_u16(entry.status).unwrap_or(StatusCode::OK));
        *response.headers_mut() = entry.headers.clone();
        // How long it has been sitting here, plus however long it had already
        // been somewhere else. Age accumulates along a chain of caches; a
        // gateway that reports only its own resident time tells a browser a
        // response is fresher than it is, and the error grows with every hop.
        let age = entry.current_age().as_secs();
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
        let only_if_cached = policy::only_if_cached(request);
        if depot
            .get::<gatel_plan::ExecutionIdentity>(gatel_plan::ExecutionIdentity::DEPOT_KEY)
            .is_err()
        {
            if only_if_cached {
                cache_only_miss(request, depot, response, ctrl);
            } else {
                ctrl.call_next(request, depot, response).await;
                status_header(response, "fwd=uri-miss; detail=missing-execution-identity");
            }
            return;
        }
        let key = self.key(request, depot);
        if let Err(refusal) = policy::request_is_cacheable(request) {
            if only_if_cached {
                cache_only_miss(request, depot, response, ctrl);
                return;
            }
            let unsafe_method = !matches!(
                *request.method(),
                http::Method::GET | http::Method::HEAD | http::Method::OPTIONS | http::Method::TRACE
            );
            ctrl.call_next(request, depot, response).await;
            let status = response.status_code.unwrap_or(StatusCode::OK);
            if unsafe_method && (status.is_success() || status.is_redirection()) {
                self.store.invalidate(&key);
                for associated in invalidation::associated(&key, response.headers()) {
                    self.store.invalidate(&associated);
                }
            }
            status_header(response, &format!("fwd=uri-miss; detail={}", refusal.as_str()));
            return;
        }

        let stale = if !only_if_cached && revalidate::eligible(request) {
            self.store
                .take_stale(&key, request.headers())
                .and_then(|(revision, entry)| {
                    revalidate::validator(&entry).map(|validator| (revision, entry, validator))
                })
        } else {
            None
        };
        if policy::may_serve_from_cache(request)
            && let Some(entry) = self.store.get_matching(&key, request.headers(), |entry| {
                policy::entry_satisfies_request(request, entry)
                    && conditional::evaluate(request.headers(), entry) != conditional::Decision::Forward
            })
        {
            // The terminal is never reached, which is the point.
            crate::generated(depot);
            self.serve(&entry, response);
            if conditional::evaluate(request.headers(), &entry) == conditional::Decision::NotModified {
                response.status_code(StatusCode::NOT_MODIFIED);
                *response.body_mut() = ResBody::None;
                response.headers_mut().remove(http::header::CONTENT_LENGTH);
                response.headers_mut().remove(http::header::CONTENT_RANGE);
            }
            ctrl.skip_rest();
            return;
        }

        if only_if_cached {
            cache_only_miss(request, depot, response, ctrl);
            return;
        }
        let revision = stale
            .as_ref()
            .map(|(revision, ..)| *revision)
            .unwrap_or_else(|| self.store.revision());
        if let Some((_, _, validator)) = &stale {
            request
                .headers_mut()
                .insert(validator.header(), validator.value());
        }
        let started = std::time::Instant::now();
        let observed = self.store.observe(&key, request.headers());
        ctrl.call_next(request, depot, response).await;
        let received = std::time::Instant::now();
        // A new 200 response supersedes the observed copy even
        // when policy/size/body failure prevents storing it. Identity guards
        // preserve concurrent replacements; successful put replaces identity.
        let _retirement = (response.status_code.unwrap_or(StatusCode::OK) == StatusCode::OK)
            .then(|| self.store.retire_on_drop(observed));
        let revalidated = stale.is_some() && response.status_code == Some(StatusCode::NOT_MODIFIED);
        if let Some((_, entry, validator)) = &stale {
            request.headers_mut().remove(validator.header());
            if response.status_code == Some(StatusCode::NOT_MODIFIED)
                && revalidate::merge(entry, validator, response).is_err()
            {
                crate::generated(depot);
                response.status_code(StatusCode::BAD_GATEWAY);
                response.headers_mut().clear();
                *response.body_mut() = ResBody::None;
                let _ = response.write_body("502 invalid cache validation response\n");
                status_header(response, "fwd=miss; detail=invalid-validation");
                return;
            }
        }
        let Some(initial_age) = age::initial(
            response.headers(),
            std::time::SystemTime::now(),
            received.duration_since(started),
        ) else {
            status_header(response, "fwd=miss; detail=invalid-age");
            return;
        };

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
        use http_body::Body as _;
        let declared = response
            .headers()
            .get(http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let hint = response.body_mut().size_hint();
        if declared.is_some_and(|length| length > self.max_entry) || hint.lower() > self.max_entry {
            status_header(response, "fwd=miss; detail=too-large");
            return;
        }
        let reserve = hint
            .exact()
            .or(declared)
            .unwrap_or(self.max_entry)
            .min(self.max_entry);
        // Reserve before touching the stream. Budget exhaustion falls through
        // to ordinary streaming rather than parking or creating a fill task.
        let Some(fill) = self.store.fills.reserve(reserve) else {
            status_header(response, "fwd=miss; detail=fill-budget");
            return;
        };
        let body = std::mem::take(response.body_mut());
        let (collected, _fill) = match collect(body, reserve, Some(fill)).await {
            Collected::Whole(bytes, fill) => (bytes, fill),
            Collected::TooLarge(body) => {
                *response.body_mut() = body;
                status_header(
                    response,
                    &format!("fwd=miss; detail={}", Refusal::TooLarge.as_str()),
                );
                return;
            }
            Collected::Uncacheable(body) => {
                // Preserve trailers/errors and the bytes read before them.
                *response.body_mut() = body;
                status_header(response, "fwd=miss; detail=body-error");
                return;
            }
        };

        // `Vary` is a property of the response, so the signature this copy
        // answers is only known now.
        let vary = policy::vary_on(response);
        let signature = store::signature(request.headers(), &vary);
        let stored = self.store.put_guarded(
            revision,
            key,
            signature,
            vary,
            store::Entry {
                status: response.status_code.unwrap_or(StatusCode::OK).as_u16(),
                headers: response.headers().clone(),
                body: collected.clone(),
                stored: received,
                ttl,
                age_on_arrival: initial_age,
            },
            stale.is_some(),
        );

        status_header(
            response,
            if stored {
                if revalidated {
                    "fwd=stale; fwd-status=304; stored"
                } else {
                    "fwd=miss; stored"
                }
            } else {
                "fwd=miss; detail=fill-rejected"
            },
        );
        *response.body_mut() = ResBody::Once(collected);
    }
}

fn cache_only_miss(request: &Request, depot: &mut Depot, response: &mut Response, ctrl: &mut FlowCtrl) {
    crate::generated(depot);
    response.status_code(StatusCode::GATEWAY_TIMEOUT);
    response.headers_mut().insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    status_header(response, "fwd=uri-miss; detail=only-if-cached");
    *response.body_mut() = ResBody::None;
    if request.method() != http::Method::HEAD {
        let _ = response.write_body("504 no acceptable cached response\n");
    }
    ctrl.skip_rest();
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
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
        assert!(matches!(collect(body, 1024, None).await, Collected::Whole(bytes, _) if bytes == "hello"));
    }

    #[tokio::test]
    async fn a_body_over_the_limit_is_handed_back_rather_than_cut() {
        // The client is owed the whole response either way; only the caching
        // is abandoned.
        let body = ResBody::Once(Bytes::from(vec![b'x'; 100]));
        assert!(matches!(collect(body, 10, None).await, Collected::TooLarge(_)));
    }

    #[tokio::test]
    async fn an_empty_body_is_a_body() {
        // A 204 has nothing in it and is still worth storing.
        assert!(matches!(
            collect(ResBody::None, 1024, None).await,
            Collected::Whole(bytes, _) if bytes.is_empty()
        ));
    }
}
