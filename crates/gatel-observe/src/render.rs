//! Prometheus text exposition.
//!
//! Written by hand rather than through a client library. The metric set is
//! fixed and small, the format is a dozen lines of rules, and a dependency that
//! owns a process-global registry would quietly undo the property this crate
//! exists to keep: that a series is created during prepare and nowhere else.

use std::fmt::Write as _;

use crate::Telemetry;
use crate::instrument::{BOUNDS, Histogram};
use crate::metrics::{Attempt, Class, Condition, Metrics, Reload};
use crate::series::Labels;

/// The content type a scrape expects.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Render every metric, in a fixed order, so two scrapes of an unchanged
/// process produce identical bytes.
pub fn prometheus(telemetry: &Telemetry) -> String {
    let metrics = &telemetry.metrics;
    let mut out = String::with_capacity(4096);
    process(metrics, &mut out);
    health(telemetry, &mut out);
    telemetry.workers.append(&mut out);
    listeners(metrics, &mut out);
    routes(metrics, &mut out);
    upstreams(metrics, &mut out);
    sinks(telemetry, &mut out);
    out
}

const SINK: &[&str] = &["sink"];

fn health(telemetry: &Telemetry, out: &mut String) {
    // Sample once so readiness and its reasons belong to the same report.
    let report = telemetry.health.report();
    family(
        out,
        "gatel_ready",
        "Whether this instance is ready to receive traffic, including degraded service.",
        "gauge",
    );
    let _ = writeln!(out, "gatel_ready {}", u8::from(report.state.is_serving()));
    family(
        out,
        "gatel_health_reason",
        "Presence of a health reason, aggregated without resource identities or diagnostic text.",
        "gauge",
    );
    for code in crate::health::Reason::CODES {
        let present = report.reasons.iter().any(|reason| reason.code() == code);
        let _ = writeln!(
            out,
            "gatel_health_reason{{reason=\"{code}\"}} {}",
            u8::from(present)
        );
    }
}

fn sinks(telemetry: &Telemetry, out: &mut String) {
    family(
        out,
        "gatel_log_lines_dropped_total",
        "Log lines a full queue could not take. A request is never delayed for one.",
        "counter",
    );
    for (name, help, kind) in [
        (
            "gatel_log_write_errors_total",
            "Log line write failures, separate from queue drops.",
            "counter",
        ),
        ("gatel_log_flush_errors_total", "Log flush failures.", "counter"),
        (
            "gatel_log_rotation_errors_total",
            "Log rotation failures.",
            "counter",
        ),
        (
            "gatel_log_last_success_timestamp_seconds",
            "Most recent successful log line write; zero before the first.",
            "gauge",
        ),
        (
            "gatel_log_io_failed",
            "Whether this sink currently has an unrecovered I/O failure.",
            "gauge",
        ),
    ] {
        family(out, name, help, kind);
    }
    for sink in telemetry.sinks.snapshot() {
        point(
            out,
            "gatel_log_lines_dropped_total",
            SINK,
            &vec![sink.name().to_string()],
            &[],
            sink.dropped(),
        );
        for (name, value) in [
            ("gatel_log_write_errors_total", sink.write_errors()),
            ("gatel_log_flush_errors_total", sink.flush_errors()),
            ("gatel_log_rotation_errors_total", sink.rotation_errors()),
            ("gatel_log_last_success_timestamp_seconds", sink.last_success()),
            ("gatel_log_io_failed", u64::from(sink.io_failed())),
        ] {
            point(out, name, SINK, &vec![sink.name().to_string()], &[], value);
        }
    }
}

fn process(metrics: &Metrics, out: &mut String) {
    let process = &metrics.process;
    family(out, "gatel_build_info", "The running build.", "gauge");
    let _ = writeln!(
        out,
        "gatel_build_info{{version=\"{}\"}} 1",
        escape(&metrics.version)
    );

    family(
        out,
        "gatel_generation",
        "Highest accepted generation reported in this process. Zero before the first report.",
        "gauge",
    );
    let _ = writeln!(out, "gatel_generation {}", process.generation.get());

    family(
        out,
        "gatel_reloads_total",
        "Configuration transactions, by how they ended.",
        "counter",
    );
    for outcome in Reload::ALL {
        let _ = writeln!(
            out,
            "gatel_reloads_total{{outcome=\"{}\"}} {}",
            outcome.as_str(),
            process.reloads(outcome)
        );
    }

    family(
        out,
        "gatel_reload_last_success_timestamp_seconds",
        "When a configuration was last accepted. Zero if none ever was.",
        "gauge",
    );
    let _ = writeln!(
        out,
        "gatel_reload_last_success_timestamp_seconds {}",
        process.last_reload_success.get()
    );

    family(
        out,
        "gatel_start_timestamp_seconds",
        "When the process started.",
        "gauge",
    );
    let _ = writeln!(
        out,
        "gatel_start_timestamp_seconds {}",
        process.start_timestamp.get()
    );
}

const LISTENER: &[&str] = &["listener"];
const SITE_ROUTE: &[&str] = &["site", "route"];
const UPSTREAM: &[&str] = &["upstream"];

fn listeners(metrics: &Metrics, out: &mut String) {
    let series = metrics.listeners.snapshot();

    family(out, "gatel_connections_total", "Connections accepted.", "counter");
    for (labels, listener) in &series {
        point(
            out,
            "gatel_connections_total",
            LISTENER,
            labels,
            &[],
            listener.connections_total.get(),
        );
    }

    family(
        out,
        "gatel_connections_active",
        "Connections currently open.",
        "gauge",
    );
    for (labels, listener) in &series {
        point(
            out,
            "gatel_connections_active",
            LISTENER,
            labels,
            &[],
            listener.connections_active.get(),
        );
    }

    family(
        out,
        "gatel_unmatched_requests_total",
        "Requests that reached a listener but no route.",
        "counter",
    );
    for (labels, listener) in &series {
        let name = "gatel_unmatched_requests_total";
        point(
            out,
            name,
            LISTENER,
            labels,
            &[],
            listener.unmatched_requests.get(),
        );
    }

    family(
        out,
        "gatel_tls_handshakes_total",
        "TLS handshakes, by whether they completed.",
        "counter",
    );
    for (labels, listener) in &series {
        let name = "gatel_tls_handshakes_total";
        point(
            out,
            name,
            LISTENER,
            labels,
            &[("outcome", "ok")],
            listener.handshakes_ok.get(),
        );
        point(
            out,
            name,
            LISTENER,
            labels,
            &[("outcome", "failed")],
            listener.handshakes_failed.get(),
        );
    }
}

fn routes(metrics: &Metrics, out: &mut String) {
    let series = metrics.routes.snapshot();

    family(
        out,
        "gatel_requests_total",
        "Requests whose response body ended, by status class.",
        "counter",
    );
    for (labels, route) in &series {
        for class in Class::ALL {
            point(
                out,
                "gatel_requests_total",
                SITE_ROUTE,
                labels,
                &[("class", class.as_str())],
                route.requests(class),
            );
        }
    }

    family(
        out,
        "gatel_requests_in_flight",
        "Requests being served right now.",
        "gauge",
    );
    for (labels, route) in &series {
        point(
            out,
            "gatel_requests_in_flight",
            SITE_ROUTE,
            labels,
            &[],
            route.in_flight.get(),
        );
    }

    family(
        out,
        "gatel_response_bytes_total",
        "Response body bytes written to clients.",
        "counter",
    );
    for (labels, route) in &series {
        point(
            out,
            "gatel_response_bytes_total",
            SITE_ROUTE,
            labels,
            &[],
            route.response_bytes.get(),
        );
    }

    family(
        out,
        "gatel_request_duration_seconds",
        "Time from the first byte of a request to the end of its response body.",
        "histogram",
    );
    for (labels, route) in &series {
        histogram(
            out,
            "gatel_request_duration_seconds",
            SITE_ROUTE,
            labels,
            &route.duration,
        );
    }
}

fn upstreams(metrics: &Metrics, out: &mut String) {
    let series = metrics.upstreams.snapshot();

    family(
        out,
        "gatel_upstream_attempts_total",
        "Attempts against a backend, by how each ended. A retried request counts more than once.",
        "counter",
    );
    for (labels, upstream) in &series {
        for outcome in Attempt::ALL {
            point(
                out,
                "gatel_upstream_attempts_total",
                UPSTREAM,
                labels,
                &[("outcome", outcome.as_str())],
                upstream.attempts(outcome),
            );
        }
    }

    family(
        out,
        "gatel_upstream_endpoints",
        "Endpoints, by condition, as of the last probe round.",
        "gauge",
    );
    for (labels, upstream) in &series {
        for condition in Condition::ALL {
            point(
                out,
                "gatel_upstream_endpoints",
                UPSTREAM,
                labels,
                &[("condition", condition.as_str())],
                upstream.endpoints(condition),
            );
        }
    }

    family(
        out,
        "gatel_upstream_pool_connections",
        "Pooled backend connections, by whether one is carrying a request.",
        "gauge",
    );
    for (labels, upstream) in &series {
        let name = "gatel_upstream_pool_connections";
        point(
            out,
            name,
            UPSTREAM,
            labels,
            &[("state", "idle")],
            upstream.pool_idle.get(),
        );
        point(
            out,
            name,
            UPSTREAM,
            labels,
            &[("state", "in_use")],
            upstream.pool_in_use.get(),
        );
    }

    family(
        out,
        "gatel_upstream_duration_seconds",
        "Time from dialling a backend to its response head.",
        "histogram",
    );
    for (labels, upstream) in &series {
        histogram(
            out,
            "gatel_upstream_duration_seconds",
            UPSTREAM,
            labels,
            &upstream.duration,
        );
    }
}

fn family(out: &mut String, name: &str, help: &str, kind: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

/// One sample: the series labels, then whatever this family adds.
fn point<V: std::fmt::Display>(
    out: &mut String,
    name: &str,
    names: &[&str],
    values: &Labels,
    extra: &[(&str, &str)],
    value: V,
) {
    let _ = writeln!(out, "{name}{} {value}", labels(names, values, extra));
}

fn histogram(out: &mut String, name: &str, names: &[&str], values: &Labels, histogram: &Histogram) {
    let cumulative = histogram.cumulative();
    for (index, bound) in BOUNDS.iter().enumerate() {
        let _ = writeln!(
            out,
            "{name}_bucket{} {}",
            labels(names, values, &[("le", &bound.to_string())]),
            cumulative[index]
        );
    }
    let _ = writeln!(
        out,
        "{name}_bucket{} {}",
        labels(names, values, &[("le", "+Inf")]),
        cumulative[BOUNDS.len()]
    );
    let _ = writeln!(
        out,
        "{name}_sum{} {}",
        labels(names, values, &[]),
        histogram.sum_seconds()
    );
    let _ = writeln!(
        out,
        "{name}_count{} {}",
        labels(names, values, &[]),
        histogram.count()
    );
}

fn labels(names: &[&str], values: &Labels, extra: &[(&str, &str)]) -> String {
    if names.is_empty() && extra.is_empty() {
        return String::new();
    }
    let mut out = String::from("{");
    for (index, (name, value)) in names.iter().zip(values).enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(out, "{name}=\"{}\"", escape(value));
    }
    for (index, (name, value)) in extra.iter().enumerate() {
        if index > 0 || !names.is_empty() {
            out.push(',');
        }
        let _ = write!(out, "{name}=\"{}\"", escape(value));
    }
    out.push('}');
    out
}

/// A label value is a configuration name, so it can contain anything the
/// operator typed. Three characters have to be escaped for the parser.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn telemetry() -> Telemetry {
        Telemetry::default()
    }

    #[test]
    fn health_metrics_distinguish_degraded_from_persistence_failure_without_sensitive_labels() {
        let telemetry = telemetry();
        let initial = prometheus(&telemetry);
        assert!(initial.contains("gatel_ready 0\n"));
        assert!(initial.contains("gatel_health_reason{reason=\"starting\"} 1\n"));
        telemetry.health.committed(1);
        telemetry.health.reload_failed("private-config-value");
        let degraded = prometheus(&telemetry);
        assert!(degraded.contains("gatel_ready 1\n"));
        assert!(degraded.contains("gatel_health_reason{reason=\"reload_failing\"} 1\n"));
        telemetry.health.persistence_unknown();
        let failed = prometheus(&telemetry);
        assert!(failed.contains("gatel_ready 0\n"));
        assert!(failed.contains("gatel_health_reason{reason=\"persistence_unknown\"} 1\n"));
        assert_eq!(
            failed
                .lines()
                .filter(|line| line.starts_with("gatel_health_reason{"))
                .count(),
            crate::health::Reason::CODES.len()
        );
        assert!(!failed.contains("private-config-value"));
    }

    #[test]
    fn a_counter_carries_its_series_labels_and_its_own() {
        let telemetry = telemetry();
        let metrics = &telemetry.metrics;
        metrics
            .route("example.com", "api")
            .completed(200, Duration::ZERO, 0);
        let text = prometheus(&telemetry);
        assert!(
            text.contains("gatel_requests_total{site=\"example.com\",route=\"api\",class=\"2xx\"} 1"),
            "{text}"
        );
    }

    #[test]
    fn a_family_is_declared_once_with_its_type() {
        let telemetry = telemetry();
        let metrics = &telemetry.metrics;
        metrics.route("a", "one");
        metrics.route("b", "two");
        let text = prometheus(&telemetry);
        assert_eq!(
            text.matches("# TYPE gatel_requests_total counter").count(),
            1,
            "two series, one declaration"
        );
    }

    #[test]
    fn a_histogram_ends_with_inf_sum_and_count() {
        let telemetry = telemetry();
        let metrics = &telemetry.metrics;
        metrics
            .route("*", "api")
            .completed(200, Duration::from_millis(20), 0);
        let text = prometheus(&telemetry);
        assert!(text.contains("le=\"0.001\"} 0"), "{text}");
        assert!(text.contains("le=\"0.025\"} 1"), "{text}");
        assert!(text.contains("le=\"+Inf\"} 1"), "{text}");
        assert!(text.contains("gatel_request_duration_seconds_count{site=\"*\",route=\"api\"} 1"));
        assert!(text.contains("gatel_request_duration_seconds_sum{site=\"*\",route=\"api\"} 0.02"));
    }

    #[test]
    fn a_label_value_cannot_break_out_of_its_quotes() {
        let telemetry = telemetry();
        let metrics = &telemetry.metrics;
        metrics.route("evil\"} fake_metric 1 #", "api");
        let text = prometheus(&telemetry);
        assert!(text.contains("site=\"evil\\\"} fake_metric 1 #\""), "{text}");
        assert!(!text.contains("\nfake_metric"), "{text}");
    }

    #[test]
    fn an_unlabelled_metric_has_no_braces() {
        let telemetry = telemetry();
        let metrics = &telemetry.metrics;
        metrics.process.generation.observe(3);
        assert!(prometheus(&telemetry).contains("\ngatel_generation 3\n"));
    }

    #[test]
    fn two_scrapes_of_an_unchanged_process_agree() {
        let telemetry = telemetry();
        let metrics = &telemetry.metrics;
        metrics.route("b", "two");
        metrics.route("a", "one");
        metrics.upstream("api");
        metrics.listener("http");
        assert_eq!(prometheus(&telemetry), prometheus(&telemetry));
    }

    #[test]
    fn every_family_is_declared_before_it_is_used() {
        let telemetry = telemetry();
        let metrics = &telemetry.metrics;
        metrics.route("*", "api");
        metrics.upstream("api");
        metrics.listener("http");
        let text = prometheus(&telemetry);
        let mut declared = std::collections::HashSet::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                declared.insert(rest.split(' ').next().unwrap().to_string());
                continue;
            }
            if line.starts_with('#') {
                continue;
            }
            let name = line.split(['{', ' ']).next().unwrap();
            let base = name
                .strip_suffix("_bucket")
                .or_else(|| name.strip_suffix("_sum"))
                .or_else(|| name.strip_suffix("_count"))
                .unwrap_or(name);
            assert!(
                declared.contains(name) || declared.contains(base),
                "{name} was never declared"
            );
        }
    }
}
