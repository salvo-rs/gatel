//! Counting what the runtime currently looks like, on a clock.
//!
//! Endpoint health and pool occupancy are not events — they are conditions, and
//! a condition has to be sampled. Probing publishes one immediately, because a
//! backend going down is exactly when readiness needs to change; a ticker
//! covers upstreams with no active check, whose endpoints still move under
//! passive health, and pool occupancy, which moves with traffic alone.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use gatel_observe::{Condition, Telemetry};
use tokio::sync::{Mutex, watch};

use crate::endpoint::Health;
use crate::upstream::{UpstreamRuntime, UpstreamRuntimes};

/// How often conditions nothing else reports are sampled.
///
/// Short enough that a dashboard is not looking at a stale picture, long enough
/// that a thousand endpoints cost nothing measurable.
pub const INTERVAL: Duration = Duration::from_secs(5);

/// The one background task that samples every upstream.
#[derive(Debug, Default)]
pub struct Census {
    running: Mutex<Option<Task>>,
}

#[derive(Debug)]
struct Task {
    stop: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for Task {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        self.handle.abort();
    }
}

impl Census {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sample this set of upstreams from now on.
    ///
    /// Called after a commit. The previous task is stopped rather than left to
    /// report upstreams the new plan may have dropped.
    pub async fn follow(&self, runtimes: &Arc<UpstreamRuntimes>, telemetry: &Arc<Telemetry>) {
        let mut running = self.running.lock().await;
        if let Some(previous) = running.take() {
            stop(previous).await;
        }
        // Publish once here, so a commit does not leave the previous
        // generation's numbers standing for a whole interval.
        publish(runtimes, telemetry);

        let (sender, receiver) = watch::channel(false);
        let runtimes = Arc::clone(runtimes);
        let telemetry = Arc::clone(telemetry);
        let handle = tokio::spawn(crate::worker::supervise(
            gatel_observe::workers::WorkerKind::Census,
            String::new(),
            Arc::clone(&telemetry),
            receiver,
            move |receiver| {
                let runtimes = Arc::clone(&runtimes);
                let telemetry = Arc::clone(&telemetry);
                sample_loop(receiver, move || publish(&runtimes, &telemetry))
            },
        ));
        *running = Some(Task { stop: sender, handle });
    }

    pub async fn shutdown(&self) {
        if let Some(task) = self.running.lock().await.take() {
            stop(task).await;
        }
    }

    pub async fn is_running(&self) -> bool {
        self.running
            .lock()
            .await
            .as_ref()
            .is_some_and(|task| !task.handle.is_finished())
    }
}

async fn sample_loop(mut stop: watch::Receiver<bool>, mut sample: impl FnMut()) {
    loop {
        if *stop.borrow() {
            return;
        }
        tokio::select! {
            biased;
            _ = stop.changed() => return,
            _ = tokio::time::sleep(INTERVAL) => sample(),
        }
    }
}

#[cfg(test)]
#[path = "census_lifecycle_tests.rs"]
mod lifecycle_tests;

async fn stop(mut task: Task) {
    let _ = task.stop.send(true);
    if tokio::time::timeout(Duration::from_secs(5), &mut task.handle)
        .await
        .is_err()
    {
        task.handle.abort();
        let _ = (&mut task.handle).await;
    }
}

/// Sample every upstream, and forget the ones no longer configured.
pub fn publish(runtimes: &UpstreamRuntimes, telemetry: &Telemetry) {
    let mut live = BTreeSet::new();
    for runtime in runtimes.iter() {
        live.insert(runtime.spec.id.name().to_string());
        publish_one(runtime, telemetry);
    }
    telemetry.health.retain_upstreams(&live);
}

/// Sample one upstream. Called from its probe loop as well, so a health change
/// reaches readiness as fast as the probe found it.
pub fn publish_one(runtime: &UpstreamRuntime, telemetry: &Telemetry) {
    let name = runtime.spec.id.name();
    let metrics = telemetry.metrics.upstream(name);

    let mut counts = [0i64; Condition::ALL.len()];
    for endpoint in runtime.endpoints.all() {
        counts[condition(endpoint.health()) as usize] += 1;
    }
    for kind in Condition::ALL {
        metrics.census(kind, counts[kind as usize]);
    }

    let (idle, in_use) = runtime.pool.stats().iter().fold((0, 0), |(idle, in_use), stats| {
        (idle + stats.idle as i64, in_use + stats.in_use as i64)
    });
    metrics.pool_idle.set(idle);
    metrics.pool_in_use.set(in_use);

    // Not "any endpoint is healthy" but "any endpoint could take a request
    // right now", which is the question readiness is actually asking. It
    // accounts for passive ejection and for the rule that an unprobed endpoint
    // is usable only when nothing was told to probe it.
    telemetry
        .health
        .upstream(name, !runtime.endpoints.eligible().is_empty());
}

const fn condition(health: Health) -> Condition {
    match health {
        Health::Unknown => Condition::Unknown,
        Health::Healthy => Condition::Healthy,
        Health::Unhealthy => Condition::Unhealthy,
        Health::Draining => Condition::Draining,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_runtime_health_has_a_condition() {
        // The two enums are separate on purpose — one is runtime state, the
        // other a label — but nothing may fall between them.
        for (health, expected) in [
            (Health::Unknown, Condition::Unknown),
            (Health::Healthy, Condition::Healthy),
            (Health::Unhealthy, Condition::Unhealthy),
            (Health::Draining, Condition::Draining),
        ] {
            assert_eq!(condition(health), expected);
        }
    }
}
