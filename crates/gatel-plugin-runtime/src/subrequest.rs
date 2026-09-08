//! Asking a configured upstream a question about a request.
//! Resource handles are bound at prepare time, never selected by request input.
//!
//! Some directives cannot decide alone. `forward-auth` has to ask an identity
//! service whether this request may proceed; a cache that revalidates has to
//! ask the origin whether what it holds is still good. Both need a backend,
//! and a directive lives in a bundle, which sits above the kernel and below
//! the transport — so it cannot open a connection and must not want to.
//!
//! It could have had its own HTTP client. That is the wrong answer, and the
//! reason is written down elsewhere in this project: a second way to reach a
//! backend is a second set of timeouts, no connection reuse, no health, no
//! TLS configuration and no balancing — every one of which the proxy already
//! does, and none of which anyone would remember to do twice. So a directive
//! gets a capability instead, and the capability is satisfied by the same
//! pool, the same health and the same policy as a proxied request.
//!
//! # Why this shape
//!
//! [`Ask`] and [`Answer`] describe a *question*, not a transport. A question
//! carries no body at all, and an answer carries at most [`Answer::LIMIT`] of
//! one, read to the end before the call returns. Nothing streams and no
//! connection stays open past the answer, so this cannot become a second way
//! to proxy — while a refusal that an identity service wrote a page for still
//! arrives intact, which is the whole reason the answer has a body and the
//! question does not.
//!
//! An implementation of [`Upstreams`] lives in `gatel-state`, next to the pool
//! it uses. This crate only says what the question is.

use std::sync::Arc;

use gatel_model::ResourceId;
// Re-exported rather than merely used: a crate that implements [`Upstreams`]
// needs exactly this vocabulary and nothing else from Salvo, so it can have it
// from here instead of taking a dependency to reach one attribute macro.
pub use salvo_core::async_trait;
pub use salvo_core::http::{HeaderMap, Method, StatusCode};

/// What is asked of an upstream.
#[derive(Debug, Clone)]
pub struct Ask {
    pub method: Method,
    /// The path and query to request, as the directive decided it — usually a
    /// fixed verification endpoint rather than the caller's own path.
    pub path: String,
    /// Everything the upstream is told about the request being decided.
    ///
    /// The directive chooses these. Forwarding the caller's whole header set
    /// by default would hand an identity service headers it did not ask for,
    /// including ones a previous hop set.
    pub headers: HeaderMap,
}

/// What the upstream said.
#[derive(Debug, Clone)]
pub struct Answer {
    pub status: StatusCode,
    pub headers: HeaderMap,
    /// The whole body, already read, truncated to nothing if it was larger
    /// than [`Answer::LIMIT`].
    ///
    /// It exists so a refusal an identity service wrote — a sign-in page, a
    /// problem document — can be shown to the client who was refused. It is
    /// buffered rather than streamed and bounded rather than open-ended,
    /// because relaying a refusal is a different thing from proxying.
    pub body: Vec<u8>,
}

impl Answer {
    /// The most of an answer that is read.
    ///
    /// A verdict and the page that explains it fit easily; anything past this
    /// is a response somebody meant to proxy, and this is not the way to do
    /// that.
    pub const LIMIT: usize = 64 * 1024;

    /// Whether the upstream agreed. Anything but a 2xx is a refusal, and the
    /// refusal itself is what gets shown to the client.
    pub fn agreed(&self) -> bool {
        self.status.is_success()
    }
}

/// Reaching the upstreams a configuration declared.
///
/// Given to a directive when its handler is built, and only then: a directive
/// cannot acquire one later, so there is no path from the request path to an
/// arbitrary backend.
#[async_trait]
pub trait Upstreams: Send + Sync {
    /// Whether an upstream by this name exists.
    ///
    /// Asked while compiling, so a name with a typo in it is an error with a
    /// position rather than a 502 the first time somebody is refused.
    fn has(&self, upstream: &ResourceId) -> bool;

    /// Put one question to one upstream.
    ///
    /// Goes through the same pool, health, timeouts and TLS as a proxied
    /// request, because it is the same backend.
    async fn ask(&self, upstream: &ResourceId, ask: Ask) -> Result<Answer, String>;
}

/// An [`Upstreams`] that has nothing, for tests and for the paths where a
/// directive is built without a runtime.
///
/// It refuses rather than pretending, so a directive built without upstreams
/// fails where it is built rather than where it is used.
#[derive(Debug, Default)]
pub struct NoUpstreams;

#[async_trait]
impl Upstreams for NoUpstreams {
    fn has(&self, _upstream: &ResourceId) -> bool {
        false
    }

    async fn ask(&self, upstream: &ResourceId, _ask: Ask) -> Result<Answer, String> {
        Err(format!(
            "no upstream runtime is available to reach `{}`",
            upstream.name()
        ))
    }
}

/// A shared handle, which is what a built handler keeps.
pub type SharedUpstreams = Arc<dyn Upstreams>;

/// A capability for exactly one backend. Requests cannot substitute another ID.
#[derive(Clone)]
pub struct UpstreamHandle {
    runtime: SharedUpstreams,
    id: ResourceId,
}

impl UpstreamHandle {
    /// Host-side binding; BuildContext additionally checks runtime availability.
    pub fn new(runtime: SharedUpstreams, id: ResourceId) -> Self {
        Self { runtime, id }
    }

    pub fn id(&self) -> &ResourceId {
        &self.id
    }

    pub async fn ask(&self, ask: Ask) -> Result<Answer, String> {
        self.runtime.ask(&self.id, ask).await
    }
}

/// Host-created allowlist. Even direct use of `BuildContext.upstreams` cannot
/// reach a backend the configuration did not declare.
pub struct ScopedUpstreams {
    runtime: SharedUpstreams,
    allowed: std::collections::BTreeSet<ResourceId>,
}

impl ScopedUpstreams {
    pub fn restrict(runtime: SharedUpstreams, allowed: Vec<ResourceId>) -> SharedUpstreams {
        Arc::new(Self {
            runtime,
            allowed: allowed.into_iter().collect(),
        })
    }
}

#[async_trait]
impl Upstreams for ScopedUpstreams {
    fn has(&self, id: &ResourceId) -> bool {
        self.allowed.contains(id) && self.runtime.has(id)
    }

    async fn ask(&self, id: &ResourceId, ask: Ask) -> Result<Answer, String> {
        if !self.allowed.contains(id) {
            return Err("upstream access was not declared by this plugin".into());
        }
        self.runtime.ask(id, ask).await
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::ResourceKind;

    use super::*;

    struct Recording(std::sync::Mutex<Vec<ResourceId>>);
    #[async_trait]
    impl Upstreams for Recording {
        fn has(&self, _: &ResourceId) -> bool {
            true
        }
        async fn ask(&self, id: &ResourceId, _: Ask) -> Result<Answer, String> {
            self.0.lock().unwrap().push(id.clone());
            Ok(Answer {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: vec![],
            })
        }
    }

    #[tokio::test]
    async fn access_is_scoped_at_dispatch_not_just_discovery() {
        let runtime = Arc::new(Recording(std::sync::Mutex::new(vec![])));
        let auth = ResourceId::new(ResourceKind::Upstream, "auth");
        let admin = ResourceId::new(ResourceKind::Upstream, "admin");
        let scoped = ScopedUpstreams::restrict(runtime.clone(), vec![auth.clone()]);
        let ask = || Ask {
            method: Method::GET,
            path: "/".into(),
            headers: HeaderMap::new(),
        };
        assert!(scoped.has(&auth));
        assert!(!scoped.has(&admin));
        assert!(scoped.ask(&admin, ask()).await.is_err());
        assert!(runtime.0.lock().unwrap().is_empty());
        let handle = UpstreamHandle::new(scoped.clone(), auth.clone());
        assert!(handle.ask(ask()).await.unwrap().agreed());
        // Wrapping an existing capability must not be able to enlarge its grant.
        let widened = ScopedUpstreams::restrict(scoped, vec![admin.clone()]);
        assert!(widened.ask(&admin, ask()).await.is_err());
        assert_eq!(*runtime.0.lock().unwrap(), vec![auth]);
    }

    #[test]
    fn a_two_hundred_is_agreement_and_nothing_else_is() {
        let answer = |status: u16| Answer {
            status: StatusCode::from_u16(status).unwrap(),
            headers: HeaderMap::new(),
            body: Vec::new(),
        };
        assert!(answer(200).agreed());
        assert!(answer(204).agreed());
        for refused in [301, 302, 401, 403, 500, 503] {
            assert!(!answer(refused).agreed(), "{refused}");
        }
    }

    #[tokio::test]
    async fn nothing_to_ask_fails_by_saying_so() {
        // Rather than by looking like a refusal, which would turn a missing
        // runtime into a 403 nobody could explain.
        let id = ResourceId::new(ResourceKind::Upstream, "auth");
        let error = NoUpstreams
            .ask(
                &id,
                Ask {
                    method: Method::GET,
                    path: "/verify".into(),
                    headers: HeaderMap::new(),
                },
            )
            .await
            .unwrap_err();
        assert!(error.contains("auth"), "{error}");
        assert!(!NoUpstreams.has(&id));
    }
}
