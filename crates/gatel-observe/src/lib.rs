//! What the process can be asked about itself.
//!
//! Two things live here, and they answer different questions. Metrics say what
//! happened; health says whether to keep sending traffic. Both are written from
//! the data plane and read by whoever asks — so this crate sits below all of it
//! and depends on nothing that can open a socket.
//!
//! The rule that shapes everything else: a metric series is only ever created
//! from a name that appears in the plan — during prepare, or by the background
//! task that samples upstream conditions. What the request path holds is a
//! handle resolved beforehand, with no way back to the registry, so the number
//! of series is a property of the configuration rather than of the traffic.

pub mod health;
pub mod instrument;
pub mod metrics;
pub mod render;
pub mod series;

use std::sync::Arc;

pub use health::{Health, Reason, Report, State};
pub use instrument::{Counter, Gauge, Histogram};
pub use metrics::{Attempt, Class, Condition, Live, Metrics, Reload};
pub use render::{CONTENT_TYPE, prometheus};
pub use series::{Labels, Series};

/// The handle the rest of the process carries around.
///
/// One value rather than two arguments, because everything that records
/// something records both kinds: a probe round moves a gauge *and* changes what
/// readiness says.
#[derive(Debug, Default)]
pub struct Telemetry {
    pub metrics: Metrics,
    pub health: Health,
}

impl Telemetry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_process_is_not_ready_and_measures_nothing() {
        let telemetry = Telemetry::new();
        assert_eq!(telemetry.health.report().state, State::NotReady);
        assert_eq!(telemetry.metrics.series(), 0);
        // The process family is always present, even with no configuration.
        assert!(prometheus(&telemetry.metrics).contains("gatel_generation 0"));
    }
}
