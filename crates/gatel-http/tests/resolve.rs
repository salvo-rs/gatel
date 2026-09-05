//! Backends given by name rather than by address.
//!
//! Every containerised deployment names its backends, so this is the shape a
//! real configuration has. What these prove is the part that is easy to get
//! wrong: the name is the endpoint's *identity*, and what it resolves to is
//! runtime state — so a resolver failing is a backend being unreachable rather
//! than a configuration being broken.
//!
//! `localhost` is the only name a test may depend on. The parts of resolution
//! that are genuinely ours — filtering, ordering, deduplicating, and what an
//! empty answer means — are unit-tested in `gatel-state` with no network at
//! all, because a machine whose resolver answers every name would otherwise
//! fail them for a reason that has nothing to do with the code.

mod support;

use support::*;

#[tokio::test]
async fn a_backend_given_by_name_is_reached() {
    let gateway = Gateway::new("resolve-name");
    let port = free_port();
    let backend = backend("named").await;

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/*" {{ proxy "localhost:{}" }}
}}
"#,
            backend.port()
        ))
        .await;

    assert!(get(gateway.address(), "/x").await.contains("named"));
}

#[tokio::test]
async fn a_name_and_an_address_can_be_backends_of_the_same_upstream() {
    let gateway = Gateway::new("resolve-mixed");
    let port = free_port();
    let backend = backend("named").await;

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "api" {{
    endpoint "127.0.0.1:{}" id="numeric"
    endpoint "localhost:{}" id="named"
    resolve interval="5s" family="ipv4"
    balance "round-robin"
}}
site "*" {{ route "/*" {{ proxy upstream="api" }} }}
"#,
            backend.port(),
            backend.port()
        ))
        .await;

    // Both reach the same backend, so every request must answer however the
    // balancer picks. What would break this is a name being treated as an
    // address, or a family narrowing the numeric endpoint away too.
    for _ in 0..4 {
        assert!(get(gateway.address(), "/x").await.contains("named"));
    }
}

#[tokio::test]
async fn a_name_that_will_not_resolve_is_an_unreachable_backend_not_a_refusal_to_start() {
    // A resolver outage must not take down a gateway whose configuration never
    // changed. So this has to commit, serve, and then fail the request — not
    // reject the configuration.
    let gateway = Gateway::new("resolve-missing");
    let port = free_port();

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "api" {{
    endpoint "no-such-host.localhost.invalid:9"
    timeouts queue="1s" connect="1s" response-head="1s" overall="5s"
}}
site "*" {{ route "/*" {{ proxy upstream="api" }} }}
"#
        ))
        .await;

    let response = get(gateway.address(), "/x").await;
    assert!(
        response.starts_with("HTTP/1.1 50"),
        "an unreachable backend is a gateway error: {response}"
    );
}

#[tokio::test]
async fn a_name_may_be_written_where_an_address_was() {
    // The short `proxy "..."` form and the `upstream` block have to agree, or
    // the two spellings would have different capabilities.
    let gateway = Gateway::new("resolve-short-form");
    let port = free_port();
    let backend = backend("short").await;

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/named" {{ proxy "localhost:{}" }}
    route "/numeric" {{ proxy "{backend}" }}
}}
"#,
            backend.port()
        ))
        .await;

    assert!(get(gateway.address(), "/named").await.contains("short"));
    assert!(get(gateway.address(), "/numeric").await.contains("short"));
}
