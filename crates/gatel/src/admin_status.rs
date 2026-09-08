//! Bounded, redacted operations schema. Never serialize internal Status/Report.
use gatel_observe::health::{Reason, Report, State};

use super::*;

pub(super) fn reply(
    status: &gatel_plan::Status,
    activated: u64,
    health: &Report,
    checkpoint_mode: bool,
) -> Response<Full<Bytes>> {
    // Configuration and health are separate samples, not a global transaction.
    // The token is for configuration CAS only: health can change independently,
    // so never put this token in an HTTP ETag for the complete status response.
    let tag = status.version().etag();
    // Fixed reason set, no per-resource names or free-form diagnostics. Report
    // may contain many upstream/site reasons; output cardinality stays bounded.
    let reasons = Reason::CODES
        .into_iter()
        .filter(|code| health.reasons.iter().any(|reason| reason.code() == *code))
        .collect::<Vec<_>>();
    let failure = status.last_failure.as_ref().map(|failure| serde_json::json!({
        "attempts": failure.attempts,
        "stage": match failure.stage { "normalize" => "normalize", "prepare" => "prepare", "observe" => "observation", _ => "other" },
    }));
    let body = serde_json::json!({
        "schema_version": 1,
        "configuration_etag": tag,
        // String preserves full u64 precision in JavaScript clients.
        "generation": status.generation.number().to_string(),
        "last_activated_generation": activated.to_string(),
        "published": status.is_serving(),
        "observed_differs": status.observed_differs,
        "last_failure": failure,
        "write_mode": if checkpoint_mode { "checkpoint" } else { "memory" },
        "persistence_unknown": health.reasons.contains(&Reason::PersistenceUnknown),
        "health": {
            "state": match health.state { State::Ready => "ready", State::Degraded => "degraded", State::NotReady => "not_ready" },
            "ready": health.state.is_serving(),
            "reasons": reasons,
        },
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::CONNECTION, "close")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

#[cfg(test)]
#[path = "admin_status_tests.rs"]
mod tests;
