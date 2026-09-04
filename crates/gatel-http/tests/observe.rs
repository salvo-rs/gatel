//! Metrics and health, end to end.

mod support;

use support::*;

fn value(scrape: &str, sample: &str) -> Option<f64> {
    scrape
        .lines()
        .find(|line| line.starts_with(sample) && line[sample.len()..].starts_with(' '))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse().ok())
}

#[tokio::test]
async fn a_request_is_counted_against_the_route_that_served_it() {
    let gateway = Gateway::new("metrics-route");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{
    route "/api/*" id="api" {{ respond "api" }}
    route "/metrics" {{ metrics }}
}}
"#
        ))
        .await;

    let address = gateway.address();
    get(address, "/api/one").await;
    get(address, "/api/two").await;

    let scrape = get(address, "/metrics").await;
    assert!(
        scrape.contains("content-type: text/plain; version=0.0.4"),
        "a scrape must announce the exposition format: {scrape}"
    );
    assert_eq!(
        value(
            &scrape,
            r#"gatel_requests_total{site="all",route="api",class="2xx"}"#
        ),
        Some(2.0),
        "{scrape}"
    );
    assert_eq!(
        value(&scrape, r#"gatel_response_bytes_total{site="all",route="api"}"#),
        Some(6.0),
        "three bytes each: {scrape}"
    );
    assert_eq!(
        value(
            &scrape,
            r#"gatel_request_duration_seconds_count{site="all",route="api"}"#
        ),
        Some(2.0)
    );
    assert_eq!(
        value(&scrape, r#"gatel_requests_in_flight{site="all",route="api"}"#),
        Some(0.0),
        "nothing is still running: {scrape}"
    );
}

#[tokio::test]
async fn a_request_no_route_claimed_does_not_invent_a_series() {
    let gateway = Gateway::new("metrics-unmatched");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/known" {{ respond "here" }}
    route "/metrics" {{ metrics }}
}}
"#
        ))
        .await;

    let address = gateway.address();
    // A path is not a label. Ten different ones must not become ten series.
    for index in 0..10 {
        get(address, &format!("/absent/{index}")).await;
    }

    let scrape = get(address, "/metrics").await;
    assert_eq!(
        value(&scrape, r#"gatel_unmatched_requests_total{listener="http"}"#),
        Some(10.0),
        "{scrape}"
    );
    assert_eq!(
        scrape.matches("gatel_requests_in_flight{").count(),
        2,
        "one series per configured route, and no more: {scrape}"
    );
}

#[tokio::test]
async fn a_reload_does_not_reset_what_a_route_has_counted() {
    let gateway = Gateway::new("metrics-reload");
    let port = free_port();
    let config = |body: &str| {
        format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{
    route "/api" id="api" {{ respond "{body}" }}
    route "/metrics" {{ metrics }}
}}
"#
        )
    };
    gateway.apply(&config("first")).await;

    let address = gateway.address();
    get(address, "/api").await;
    gateway.apply(&config("second")).await;
    get(address, "/api").await;

    let scrape = get(address, "/metrics").await;
    // A counter that restarted at zero would make every rate over a deploy a
    // lie, which is exactly when someone is watching it.
    assert_eq!(
        value(
            &scrape,
            r#"gatel_requests_total{site="all",route="api",class="2xx"}"#
        ),
        Some(2.0),
        "{scrape}"
    );
    assert_eq!(value(&scrape, "gatel_generation"), Some(2.0), "{scrape}");
}

#[tokio::test]
async fn a_route_a_reload_removed_stops_being_reported() {
    let gateway = Gateway::new("metrics-retain");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/old" id="old" {{ respond "old" }}
    route "/metrics" {{ metrics }}
}}
"#
        ))
        .await;
    let address = gateway.address();
    get(address, "/old").await;
    assert!(get(address, "/metrics").await.contains(r#"route="old""#));

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/new" id="new" {{ respond "new" }}
    route "/metrics" {{ metrics }}
}}
"#
        ))
        .await;
    // The harness does the retaining `App::activate` does after a commit.
    gateway.retain_series();

    let scrape = get(address, "/metrics").await;
    assert!(
        !scrape.contains(r#"route="old""#),
        "a deleted route must not sit at a value that will never change: {scrape}"
    );
    assert!(scrape.contains(r#"route="new""#), "{scrape}");
}

#[tokio::test]
async fn readiness_answers_once_a_generation_is_serving() {
    let gateway = Gateway::new("readiness");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/readyz" {{ health "ready" }}
    route "/healthz" {{ health "live" }}
}}
"#
        ))
        .await;

    let address = gateway.address();
    let ready = get(address, "/readyz").await;
    assert!(ready.starts_with("HTTP/1.1 200"), "{ready}");
    assert!(ready.contains("cache-control: no-store"), "{ready}");
    assert!(ready.ends_with("ready\n"), "{ready}");

    let live = get(address, "/healthz").await;
    assert!(live.starts_with("HTTP/1.1 200"), "{live}");
    assert!(live.ends_with("alive\n"), "{live}");
}

#[tokio::test]
async fn a_refused_reload_degrades_readiness_without_removing_the_instance() {
    let gateway = Gateway::new("readiness-degraded");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{ route "/readyz" {{ health "ready" }} }}
"#
        ))
        .await;
    let address = gateway.address();

    let outcome = gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{ route "/readyz" {{ health "ready"
    nonsense
}} }}
"#
        ))
        .await;
    assert!(matches!(outcome, gatel_plan::Outcome::Rejected(_)), "{outcome:?}");

    let ready = get(address, "/readyz").await;
    // Every replica reads the same file. Failing readiness here would turn one
    // bad edit into a total outage while each replica is still serving.
    assert!(ready.starts_with("HTTP/1.1 200"), "{ready}");
    assert!(ready.contains("degraded\n"), "{ready}");
    assert!(
        ready.contains("failed reload"),
        "the reason has to be visible: {ready}"
    );
}

#[tokio::test]
async fn metrics_obey_the_guards_written_on_their_route() {
    let gateway = Gateway::new("metrics-guarded");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/metrics" {{
        ip-filter allow="10.0.0.0/8"
        metrics
    }}
}}
"#
        ))
        .await;

    // Nothing about a metrics route is exempt from the rest of the
    // configuration, which is why it is a route and not an admin port.
    let refused = get(gateway.address(), "/metrics").await;
    assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
}

#[tokio::test]
async fn an_access_log_writes_the_common_log_format_to_a_file() {
    let gateway = Gateway::new("access-log-file");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{
    access-log format="clf" output="file:access.log"
    route "/hello" id="hello" {{ respond "hi\n" }}
}}
"#
        ))
        .await;

    get(gateway.address(), "/hello").await;

    let path = gateway.directory.join("access.log");
    let line = read_eventually(&path).await;
    // `client - - [date] "GET /hello HTTP/1.1" 200 3`
    assert!(line.contains(r#""GET /hello HTTP/1.1" 200 3"#), "{line}");
    assert!(line.starts_with("127.0.0.1 - - ["), "{line}");
}

#[tokio::test]
async fn an_access_log_file_that_cannot_be_opened_stops_the_reload() {
    let gateway = Gateway::new("access-log-refuse");
    let port = free_port();
    let good = format!(
        r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{ route "/" id="root" {{ respond "ok\n" }} }}
"#
    );
    gateway.apply(&good).await;

    // The path names the configuration directory, which cannot be appended to.
    let outcome = gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{
    access-log output="file:."
    route "/" id="root" {{ respond "ok\n" }}
}}
"#
        ))
        .await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::PrepareFailed(_)),
        "opening the file belongs to prepare, not to the first request: {outcome:?}"
    );
    // The previous generation is untouched.
    assert!(get(gateway.address(), "/").await.contains("ok"));
}

/// The writer flushes on a timer, so a test reads the file when it is there
/// rather than guessing how long that takes.
async fn read_eventually(path: &std::path::Path) -> String {
    for _ in 0..60 {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.is_empty()
        {
            return text;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("nothing was written to {}", path.display());
}

#[tokio::test]
async fn a_request_to_an_unknown_path_still_gets_a_log_line() {
    let gateway = Gateway::new("access-log-unmatched");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{
    access-log format="clf" output="file:access.log"
    route "/known" id="known" {{ respond "hi\n" }}
}}
"#
        ))
        .await;

    get(gateway.address(), "/nowhere").await;

    // The requests worth seeing are exactly the ones that matched nothing, so
    // a site's access log covers them even though no route claimed them.
    let line = read_eventually(&gateway.directory.join("access.log")).await;
    assert!(line.contains(r#""GET /nowhere HTTP/1.1" 404"#), "{line}");
}
