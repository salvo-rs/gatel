//! Opt-in stage timing; never includes asset paths or contents.
use std::time::Instant;

pub(super) struct Phase {
    stage: &'static str,
    started: Instant,
}
impl Phase {
    pub(super) fn start(stage: &'static str) -> Self {
        tracing::debug!(target: "gatel::precompress", event = "precompress_phase_started", stage);
        Self {
            stage,
            started: Instant::now(),
        }
    }
}
impl Drop for Phase {
    fn drop(&mut self) {
        tracing::debug!(target: "gatel::precompress", event = "precompress_phase_finished",
            stage = self.stage, elapsed_ms = self.started.elapsed().as_secs_f64() * 1000.0);
    }
}
