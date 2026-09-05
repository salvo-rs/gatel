//! Answering a directive's question by asking a backend.
//!
//! This is the runtime half of [`gatel_plan::Upstreams`]. The half in
//! `gatel-plan` says what a question is; this one puts it on a socket, and it
//! does so through the same balancer, the same pool, the same health and the
//! same timeouts a proxied request goes through — because it is the same
//! backend, and a second path to it would be a second set of everything.
//!
//! # What it deliberately does not do
//!
//! It sends no body. It reads one, to the end and into memory, but only up to
//! [`Answer::LIMIT`] — enough for the page an identity service writes when it
//! refuses somebody, and not enough to be a proxy. Past that bound the answer
//! keeps its head, loses its body, and takes its connection with it rather
//! than reading further.

use std::sync::Arc;

use gatel_model::ResourceId;
use gatel_plan::{Answer, Ask, Upstreams, async_trait};
use http_body_util::{BodyExt, Empty};

use crate::endpoint::Endpoint;
use crate::pool::{Body, CheckoutError};
use crate::selector::{RequestKey, select};
use crate::upstream::{UpstreamRuntime, UpstreamRuntimes};

#[async_trait]
impl Upstreams for UpstreamRuntimes {
    fn has(&self, upstream: &ResourceId) -> bool {
        self.get(upstream).is_some()
    }

    async fn ask(&self, upstream: &ResourceId, ask: Ask) -> Result<Answer, String> {
        let runtime = self
            .get(upstream)
            .ok_or_else(|| format!("upstream `{}` has no runtime", upstream.name()))?;
        ask_upstream(runtime, ask).await
    }
}

/// Put one question to one upstream, retrying elsewhere while nothing has been
/// sent.
///
/// A question carries no body, so every failure before the response head is
/// safe to retry on another endpoint: there is nothing the backend could have
/// acted on. That is a stronger guarantee than the proxy has, and it is why
/// this loop is shorter than the proxy's rather than shared with it.
pub async fn ask_upstream(runtime: &Arc<UpstreamRuntime>, ask: Ask) -> Result<Answer, String> {
    let spec = &runtime.spec;
    let timeouts = &spec.timeouts;
    let deadline = tokio::time::Instant::now() + timeouts.overall;
    let attempts = spec
        .retry
        .as_ref()
        .map(|retry| retry.attempts.max(1))
        .unwrap_or(1);

    let key = RequestKey {
        client_ip: None,
        uri: &ask.path,
        header: None,
        cookie: None,
    };

    let mut excluded: Vec<String> = Vec::new();
    let mut last: Option<String> = None;

    for _ in 0..attempts {
        let eligible: Vec<Arc<Endpoint>> = runtime
            .endpoints
            .eligible()
            .into_iter()
            .filter(|endpoint| !excluded.contains(&endpoint.id))
            .collect();
        let Some(selection) = select(&spec.balance, &eligible, &runtime.selector, &key) else {
            return Err(
                last.unwrap_or_else(|| format!("upstream `{}` has no endpoint to ask", spec.id.name()))
            );
        };
        let endpoint = Arc::clone(&selection.endpoint);
        // Held for the whole exchange, so `least-conn` counts this against the
        // endpoint exactly as it counts a proxied request.
        let _in_flight = runtime.endpoints.claim(&endpoint);

        let checkout = tokio::time::timeout_at(deadline, runtime.pool.checkout(&endpoint, timeouts)).await;
        let mut lease = match checkout {
            Ok(Ok(lease)) => lease,
            Ok(Err(error)) => {
                runtime.endpoints.report_traffic(&endpoint.id, true);
                excluded.push(endpoint.id.clone());
                last = Some(describe(&error));
                continue;
            }
            Err(_) => return Err(format!("no answer within {:?}", timeouts.overall)),
        };

        let request = build(&ask, &endpoint)?;
        let sent = tokio::time::timeout(timeouts.response_head, lease.send(request)).await;
        let response = match sent {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                // Nothing was sent that the backend could have acted on, so
                // unlike a proxied request this may be tried elsewhere.
                runtime.endpoints.report_traffic(&endpoint.id, true);
                excluded.push(endpoint.id.clone());
                lease.poison();
                last = Some(error.to_string());
                continue;
            }
            Err(_) => {
                runtime.endpoints.report_traffic(&endpoint.id, true);
                lease.poison();
                return Err(format!("no answer within {:?}", timeouts.response_head));
            }
        };

        let status = response.status();
        let failed = runtime.endpoints.counts_as_failure(status.as_u16());
        runtime.endpoints.report_traffic(&endpoint.id, failed);

        let (head, body) = response.into_parts();
        // A refusal is an answer, so the connection is worth keeping — but only
        // if what is left on it is read off first. Under the overall deadline
        // like everything else: a backend that sends a head and then stalls
        // must not hold a request open on the strength of having answered.
        let read = tokio::time::timeout_at(deadline, read_bounded(body))
            .await
            .unwrap_or(None);
        if read.is_none() {
            lease.poison();
        }
        return Ok(Answer {
            status: head.status,
            headers: head.headers,
            body: read.unwrap_or_default(),
        });
    }

    Err(last.unwrap_or_else(|| format!("upstream `{}` could not be asked", spec.id.name())))
}

fn build(ask: &Ask, endpoint: &Endpoint) -> Result<http::Request<Body>, String> {
    let mut builder = http::Request::builder()
        .method(ask.method.clone())
        .uri(ask.path.as_str());
    let headers = builder.headers_mut().expect("a fresh builder has no error");
    for (name, value) in &ask.headers {
        headers.append(name.clone(), value.clone());
    }
    // The endpoint's own authority, unless the caller said otherwise: an
    // upstream that virtual-hosts is told which host it is being asked about by
    // the directive, which knows, rather than by this function, which does not.
    if !headers.contains_key(http::header::HOST)
        && let Ok(value) = http::HeaderValue::from_str(&endpoint.target.describe())
    {
        headers.insert(http::header::HOST, value);
    }
    builder
        .body(
            Empty::<bytes::Bytes>::new()
                .map_err(|never| match never {})
                .boxed(),
        )
        .map_err(|error| error.to_string())
}

/// Read the whole answer, or give up on it.
///
/// `None` means the body did not finish — it failed, or it was larger than an
/// answer is allowed to be. Either way what is left is unread, so the
/// connection cannot go back to the pool.
async fn read_bounded(mut body: hyper::body::Incoming) -> Option<Vec<u8>> {
    let mut collected: Vec<u8> = Vec::new();
    loop {
        match body.frame().await {
            None => return Some(collected),
            Some(Err(_)) => return None,
            Some(Ok(frame)) => {
                if let Some(data) = frame.data_ref() {
                    if collected.len() + data.len() > Answer::LIMIT {
                        return None;
                    }
                    collected.extend_from_slice(data);
                }
            }
        }
    }
}

fn describe(error: &CheckoutError) -> String {
    error.summary().to_string()
}

#[cfg(test)]
mod tests {
    use gatel_model::ResourceKind;
    use gatel_plan::{HeaderMap, Method};

    use super::*;

    #[tokio::test]
    async fn a_name_that_is_not_there_says_so_rather_than_refusing() {
        let runtimes = UpstreamRuntimes::default();
        let id = ResourceId::new(ResourceKind::Upstream, "auth");
        assert!(!runtimes.has(&id));
        let error = runtimes
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
    }
}
