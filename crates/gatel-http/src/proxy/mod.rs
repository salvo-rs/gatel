//! The reverse proxy terminal.
//!
//! This lives in the kernel rather than in a bundle because endpoint
//! selection, health, connection reuse and retry are one correctness chain: a
//! plugin that could replace any single link could also break the others.
//!
//! Three files: the attempt loop here, [`outbound`] for the request that goes
//! out and the answer that comes back, and [`tunnel`] for the point where a
//! connection stops being HTTP.
//!
//! # What may be tried again
//!
//! A retry is only ever safe because of something the proxy knows about *this*
//! attempt, so the rule is written in terms of when the failure happened
//! rather than what it was:
//!
//! - Anything that failed **before a byte was sent** — nothing healthy to pick, no connection to be had, a
//!   queue that timed out — may be tried on another endpoint whatever the method, because the backend has not
//!   seen the request. This is `retry mode="safe"`.
//! - Anything that failed **after the request was on the wire** may only be tried again if the request can be
//!   reproduced exactly, which means the body was buffered or there was none. This is `retry mode="replay"`,
//!   and it can duplicate a non-idempotent request, which is why the configuration refuses to enable it
//!   without an explicit buffer size.
//! - An upgrade is never retried. The client is committed to one connection from the moment it asks.
//!
//! And on top of all three, a budget: retries are capped as a share of the
//! upstream's live traffic, so a backend that starts failing everything cannot
//! have its remaining capacity doubled by the gateway trying to help.

mod outbound;
mod tunnel;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use gatel_state::{CheckoutError, Endpoint, RequestKey, UpstreamRuntime, select};
use http_body_util::BodyExt;
use salvo_core::http::{Request, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

use crate::dispatch::request_host;
use crate::upgrade;

/// A proxy terminal bound to one upstream's runtime.
pub struct ProxyTerminal {
    upstream: Arc<UpstreamRuntime>,
    metrics: Arc<gatel_observe::metrics::Upstream>,
}

impl std::fmt::Debug for ProxyTerminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyTerminal")
            .field("upstream", &self.upstream.spec.id.name())
            .finish()
    }
}

impl ProxyTerminal {
    pub fn new(upstream: Arc<UpstreamRuntime>, metrics: Arc<gatel_observe::metrics::Upstream>) -> Self {
        Self { upstream, metrics }
    }

    /// Record one attempt. Every path out of the attempt loop passes through
    /// here, so the four counters add up to the attempts actually made.
    fn attempted(&self, outcome: gatel_observe::Attempt, since: tokio::time::Instant) {
        self.metrics.attempted(outcome, since.elapsed());
    }
}

/// What this request can send, once or more than once.
#[derive(Debug)]
pub(crate) enum Payload {
    /// Nothing to send, so every attempt sends the same thing: nothing.
    Empty,
    /// Held in memory because the configuration asked for replay and the body
    /// was small enough to fit the buffer it named.
    Buffered(Bytes),
    /// Passed straight through. Reproducing it is impossible, so nothing that
    /// happens after it starts moving may be tried again.
    Streaming,
}

impl Payload {
    /// Whether a failure *after* the request was sent may be tried again.
    fn is_reproducible(&self) -> bool {
        !matches!(self, Payload::Streaming)
    }
}

/// Why a proxied request failed, mapped to what the client is told.
#[derive(Debug)]
enum ProxyError {
    /// Nothing is healthy, so there is nowhere to send it.
    NoEndpoint,
    /// A connection could not be obtained.
    Checkout(CheckoutError),
    /// The upstream did not answer in time.
    Timeout(&'static str, Duration),
    /// The upstream answered badly, or not at all.
    Upstream(String),
    /// The client's own body could not be read.
    ClientBody(String),
}

impl ProxyError {
    fn status(&self) -> StatusCode {
        match self {
            ProxyError::NoEndpoint => StatusCode::SERVICE_UNAVAILABLE,
            ProxyError::Checkout(CheckoutError::QueueFull { .. } | CheckoutError::QueueTimeout(_)) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            ProxyError::Checkout(_) => StatusCode::BAD_GATEWAY,
            ProxyError::Timeout(..) => StatusCode::GATEWAY_TIMEOUT,
            ProxyError::Upstream(_) => StatusCode::BAD_GATEWAY,
            // Not the backend's fault and not ours: the request never arrived
            // in full, so 400 rather than a gateway error.
            ProxyError::ClientBody(_) => StatusCode::BAD_REQUEST,
        }
    }

    /// What the client sees. Never the upstream's address or error text: that
    /// belongs in the log, not in a response to whoever asked.
    fn public_message(&self) -> &'static str {
        match self {
            ProxyError::NoEndpoint => "503 no healthy backend\n",
            ProxyError::Checkout(CheckoutError::QueueFull { .. } | CheckoutError::QueueTimeout(_)) => {
                "503 backend at capacity\n"
            }
            ProxyError::Checkout(_) => "502 could not reach the backend\n",
            ProxyError::Timeout(..) => "504 the backend did not answer in time\n",
            ProxyError::Upstream(_) => "502 the backend replied badly\n",
            ProxyError::ClientBody(_) => "400 the request body could not be read\n",
        }
    }

    fn detail(&self) -> String {
        match self {
            ProxyError::NoEndpoint => "no eligible endpoint".to_string(),
            ProxyError::Checkout(error) => error.summary(),
            ProxyError::Timeout(stage, after) => format!("{stage} exceeded {after:?}"),
            ProxyError::Upstream(message) | ProxyError::ClientBody(message) => message.clone(),
        }
    }
}

#[async_trait]
impl Handler for ProxyTerminal {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        _ctrl: &mut FlowCtrl,
    ) {
        let started = std::time::Instant::now();
        match self.proxy(request, depot, response).await {
            Ok(attempts) => {
                depot.insert("gatel.upstream_attempts", attempts);
            }
            Err(error) => {
                tracing::warn!(
                    target: "gatel::upstream",
                    event = "upstream_failed",
                    upstream = self.upstream.spec.id.name(),
                    status = error.status().as_u16(),
                    detail = error.detail(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                );
                // The gateway could not reach the backend, so this answer is
                // the gateway's own and an error page may replace it. A status
                // the backend itself returned is not marked.
                depot.insert("gatel.generated", true);
                response.status_code(error.status());
                let _ = response.write_body(error.public_message());
            }
        }
    }
}

impl ProxyTerminal {
    async fn proxy(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
    ) -> Result<u32, ProxyError> {
        let spec = &self.upstream.spec;
        let timeouts = &spec.timeouts;
        let deadline = tokio::time::Instant::now() + timeouts.overall;

        let client_ip = depot.get::<std::net::IpAddr>("gatel.client_ip").ok().copied();
        let tls = depot.get::<bool>("gatel.tls").ok().copied().unwrap_or(false);
        let uri = request.uri().clone();
        let target = uri
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/")
            .to_string();
        let host = request_host(request);

        let upgrade_to = upgrade::requested_protocol(request.headers());
        if upgrade_to.is_some() && spec.protocol == gatel_plan::UpstreamProtocol::Http2 {
            // An upgrade over HTTP/2 is extended CONNECT, which is a different
            // mechanism. Saying so beats sending a request the backend will
            // answer confusingly.
            return Err(ProxyError::Upstream(
                "the upstream speaks HTTP/2, which does not carry an upgrade".into(),
            ));
        }
        // Taken before the outbound request is built, because the outbound
        // request must not inherit the client's upgrade handle.
        let client_upgrade = upgrade_to
            .as_ref()
            .and(request.extensions_mut().remove::<hyper::upgrade::OnUpgrade>());

        // An upgrade commits the client to one connection, so it is never
        // retried and its body is never buffered.
        let retry = spec.retry.as_ref().filter(|_| upgrade_to.is_none());
        let payload = self.payload(request, retry).await?;
        let attempts_allowed = retry.map(|retry| retry.attempts.max(1)).unwrap_or(1);
        let per_try = retry.map(|retry| retry.per_try);
        let may_replay = retry.is_some_and(|retry| retry.mode == gatel_plan::RetryMode::Replay)
            && payload.is_reproducible();

        // Copied out of the request rather than borrowed from it, because the
        // attempt loop needs the key on every pass and needs the request itself
        // mutably in between. Only the two hashing strategies pay for it.
        let hashed_header: Option<String> = match &spec.balance {
            gatel_plan::Balance::HeaderHash { header } => request
                .headers()
                .get(header.as_str())
                .and_then(|value| value.to_str().ok())
                .map(str::to_string),
            _ => None,
        };
        let hashed_cookie: Option<String> = match &spec.balance {
            gatel_plan::Balance::CookieHash { cookie } => {
                outbound::cookie_value(request, cookie).map(str::to_string)
            }
            _ => None,
        };
        let key = RequestKey {
            client_ip,
            uri: &target,
            header: hashed_header.as_deref(),
            cookie: hashed_cookie.as_deref(),
        };

        let mut excluded: Vec<String> = Vec::new();
        let mut last: Option<ProxyError> = None;

        for attempt in 1..=attempts_allowed {
            // Every attempt after the first has to fit in the upstream's retry
            // budget. Held for the whole attempt, so what the budget measures
            // is retries actually in flight rather than retries begun.
            let _retry_permit = match attempt {
                1 => None,
                _ => match self.upstream.retry_budget.take(
                    self.upstream.endpoints.in_flight(),
                    retry.map(|retry| retry.budget_percent).unwrap_or(0),
                ) {
                    Some(permit) => Some(permit),
                    None => {
                        tracing::debug!(
                            target: "gatel::upstream",
                            event = "retry_budget_exhausted",
                            upstream = spec.id.name(),
                            attempt,
                        );
                        break;
                    }
                },
            };

            let attempt_started = tokio::time::Instant::now();
            // An attempt gets its own slice of the overall budget, so one slow
            // endpoint cannot spend the time the next attempt would have used.
            let attempt_deadline = match per_try {
                Some(per_try) => deadline.min(attempt_started + per_try),
                None => deadline,
            };
            let out_of_time = |elapsed: tokio::time::Instant| -> ProxyError {
                match (per_try, elapsed >= deadline) {
                    (Some(per_try), false) => ProxyError::Timeout("per-try", per_try),
                    _ => ProxyError::Timeout("overall", timeouts.overall),
                }
            };

            let eligible: Vec<Arc<Endpoint>> = self
                .upstream
                .endpoints
                .eligible()
                .into_iter()
                .filter(|endpoint| !excluded.contains(&endpoint.id))
                .collect();

            let Some(selection) = select(&spec.balance, &eligible, &self.upstream.selector, &key) else {
                self.attempted(gatel_observe::Attempt::Unavailable, attempt_started);
                return Err(last.unwrap_or(ProxyError::NoEndpoint));
            };
            let endpoint = Arc::clone(&selection.endpoint);
            // Held until the response body finishes, so `least-conn` counts
            // work rather than sockets.
            let in_flight = self.upstream.endpoints.claim(&endpoint);

            let checkout =
                tokio::time::timeout_at(attempt_deadline, self.upstream.pool.checkout(&endpoint, timeouts))
                    .await;

            let mut lease = match checkout {
                Ok(Ok(lease)) => lease,
                Ok(Err(error)) => {
                    // Every checkout failure happened before a byte was sent,
                    // so trying elsewhere cannot duplicate anything — whatever
                    // the method, and whether or not there is a body.
                    tracing::debug!(
                        target: "gatel::upstream",
                        event = "checkout_failed",
                        endpoint = %endpoint.id,
                        attempt,
                        detail = error.summary(),
                    );
                    self.upstream.endpoints.report_traffic(&endpoint.id, true);
                    excluded.push(endpoint.id.clone());
                    // A connection that could not be made is a failure; a queue
                    // that was full is capacity. Telling them apart is the
                    // difference between fixing a backend and adding one.
                    self.attempted(
                        match error {
                            CheckoutError::Connect(_) => gatel_observe::Attempt::Failed,
                            _ => gatel_observe::Attempt::Unavailable,
                        },
                        attempt_started,
                    );
                    last = Some(ProxyError::Checkout(error));
                    continue;
                }
                Err(_) => {
                    self.attempted(gatel_observe::Attempt::TimedOut, attempt_started);
                    let now = tokio::time::Instant::now();
                    let error = out_of_time(now);
                    // Running out of the overall budget ends the request; using
                    // up one attempt's slice is what per-try is for, and the
                    // failure is still a pre-send one.
                    if now >= deadline {
                        return Err(error);
                    }
                    excluded.push(endpoint.id.clone());
                    last = Some(error);
                    continue;
                }
            };

            let request_out = self.build_request(
                request,
                &target,
                &host,
                client_ip,
                tls,
                upgrade_to.as_deref(),
                &payload,
            )?;
            let head_deadline = attempt_deadline.min(tokio::time::Instant::now() + timeouts.response_head);
            let sent = tokio::time::timeout_at(head_deadline, lease.send(request_out)).await;

            let upstream_response = match sent {
                Ok(Ok(upstream_response)) => upstream_response,
                Ok(Err(error)) => {
                    // The request was already on the wire, so the upstream may
                    // have acted on it. Only a request that can be reproduced
                    // exactly, under a configuration that asked for replay, is
                    // sent again.
                    self.upstream.endpoints.report_traffic(&endpoint.id, true);
                    self.attempted(gatel_observe::Attempt::Failed, attempt_started);
                    lease.poison();
                    if !may_replay {
                        return Err(ProxyError::Upstream(error.to_string()));
                    }
                    excluded.push(endpoint.id.clone());
                    last = Some(ProxyError::Upstream(error.to_string()));
                    continue;
                }
                Err(_) => {
                    self.upstream.endpoints.report_traffic(&endpoint.id, true);
                    self.attempted(gatel_observe::Attempt::TimedOut, attempt_started);
                    lease.poison();
                    let now = tokio::time::Instant::now();
                    let error = if now >= head_deadline && head_deadline < attempt_deadline {
                        ProxyError::Timeout("response-head", timeouts.response_head)
                    } else {
                        out_of_time(now)
                    };
                    if !may_replay || now >= deadline {
                        return Err(error);
                    }
                    excluded.push(endpoint.id.clone());
                    last = Some(error);
                    continue;
                }
            };

            let status = upstream_response.status().as_u16();
            // Answered means the head came back, whatever it said: a 500 from a
            // backend that is reachable is a different problem from one that is
            // not, and the status class already carries the first.
            self.attempted(gatel_observe::Attempt::Answered, attempt_started);

            // A backend that agreed to upgrade means this connection stops
            // being HTTP. It leaves the pool and becomes a tunnel.
            if status == 101 && upgrade_to.is_some() {
                self.upstream.endpoints.report_traffic(&endpoint.id, false);
                lease.poison();
                self.start_tunnel(
                    response,
                    upstream_response,
                    client_upgrade,
                    in_flight,
                    &endpoint.id,
                );
                return Ok(attempt);
            }

            let failed = self.upstream.endpoints.counts_as_failure(status);
            if self.upstream.endpoints.report_traffic(&endpoint.id, failed) {
                tracing::info!(
                    target: "gatel::upstream",
                    event = "endpoint_ejected",
                    upstream = self.upstream.spec.id.name(),
                    endpoint = %endpoint.id,
                    status,
                );
            }

            // The lease goes with the response: the body is still being read
            // off that connection, so it is not free until the body is done.
            self.write_response(response, upstream_response, in_flight, lease);
            return Ok(attempt);
        }

        Err(last.unwrap_or(ProxyError::NoEndpoint))
    }

    /// Decide what this request is able to send, and how many times.
    ///
    /// Buffering is refused rather than attempted when the size is not
    /// declared. A chunked upload has no length to check against the budget the
    /// operator set, and reading it to find out would mean holding an unbounded
    /// amount of somebody else's data to answer a question about a limit.
    async fn payload(
        &self,
        request: &mut Request,
        retry: Option<&gatel_plan::Retry>,
    ) -> Result<Payload, ProxyError> {
        let Some(length) = outbound::declared_length(request) else {
            return Ok(if outbound::has_body(request) {
                Payload::Streaming
            } else {
                Payload::Empty
            });
        };
        if length == 0 {
            return Ok(Payload::Empty);
        }
        let fits = retry.is_some_and(|retry| {
            retry.mode == gatel_plan::RetryMode::Replay && length <= retry.buffer.bytes()
        });
        if !fits {
            return Ok(Payload::Streaming);
        }
        let collected = request
            .take_body()
            .collect()
            .await
            .map_err(|error| ProxyError::ClientBody(error.to_string()))?;
        Ok(Payload::Buffered(collected.to_bytes()))
    }
}

#[cfg(test)]
mod tests;
