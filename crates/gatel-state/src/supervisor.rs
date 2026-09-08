//! Background services.
//!
//! Probes are tasks, and tasks outlive the generation that asked for them: a
//! reload that does not change an upstream's health configuration must not
//! restart its probing, or a gateway reloaded every minute would never
//! accumulate enough consecutive results to change a verdict.
//!
//! Nothing starts here until a generation is committed. A candidate that fails
//! to prepare, or loses a race, leaves no worker behind because it never
//! created one.

use std::collections::BTreeMap;
use std::sync::Arc;

use gatel_model::{Fingerprint, Hasher, ResourceId};
use gatel_observe::Telemetry;
use tokio::sync::watch;

use crate::census::{self, Census};
use crate::endpoint::{EndpointSet, probe_delay};
use crate::probe::probe_with_tls;
use crate::resolve;
use crate::upstream::{UpstreamRuntime, UpstreamRuntimes};

struct Worker {
    /// What the worker was started for. When this changes, it is replaced.
    identity: Fingerprint,
    stop: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        self.handle.abort();
    }
}

/// Owns every background task, and keeps them in step with the current plan.
pub struct Supervisor {
    /// Serializes complete reconciliations and terminal shutdown, not traffic.
    closed: tokio::sync::Mutex<bool>,
    workers: tokio::sync::Mutex<BTreeMap<ResourceId, Worker>>,
    /// Kept apart from the probe workers because the two are wanted
    /// independently: an upstream may be probed and not resolved, resolved and
    /// not probed, or both - and a reload that changes one must not restart
    /// the other.
    resolvers: tokio::sync::Mutex<BTreeMap<ResourceId, Worker>>,
    /// Sampling what probing does not report. One task for the whole process.
    census: Census,
    telemetry: Arc<Telemetry>,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor").finish_non_exhaustive()
    }
}

impl Supervisor {
    pub fn new(telemetry: Arc<Telemetry>) -> Self {
        Self {
            closed: tokio::sync::Mutex::new(false),
            workers: tokio::sync::Mutex::new(BTreeMap::new()),
            resolvers: tokio::sync::Mutex::new(BTreeMap::new()),
            census: Census::new(),
            telemetry,
        }
    }

    /// Start, keep or stop probes so they match the committed generation.
    ///
    /// Called after a commit, never before.
    pub async fn reconcile(&self, runtimes: &Arc<UpstreamRuntimes>) {
        let closed = self.closed.lock().await;
        if *closed {
            return;
        }
        self.census.follow(runtimes, &self.telemetry).await;
        self.reconcile_resolvers(runtimes).await;
        let mut workers = self.workers.lock().await;

        let wanted: Vec<(&ResourceId, Fingerprint, &Arc<UpstreamRuntime>)> = runtimes
            .iter()
            .filter(|runtime| runtime.spec.health.is_some())
            .map(|runtime| (&runtime.spec.id, worker_identity(runtime), runtime))
            .collect();

        for (id, identity, runtime) in &wanted {
            match workers.get(*id) {
                // Same probe over the same endpoints: leave it running, so the
                // consecutive-result counters survive the reload.
                Some(existing) if existing.identity == *identity && !existing.handle.is_finished() => {
                    continue;
                }
                Some(_) => {
                    if let Some(previous) = workers.remove(*id) {
                        stop(previous).await;
                    }
                }
                None => {}
            }
            let (sender, receiver) = watch::channel(false);
            let runtime = Arc::clone(runtime);
            let telemetry = Arc::clone(&self.telemetry);
            let handle = tokio::spawn(crate::worker::supervise(
                gatel_observe::workers::WorkerKind::Probe,
                id.name().to_string(),
                Arc::clone(&telemetry),
                receiver,
                move |receiver| probe_loop(Arc::clone(&runtime), Arc::clone(&telemetry), receiver),
            ));
            workers.insert(
                (*id).clone(),
                Worker {
                    identity: *identity,
                    stop: sender,
                    handle,
                },
            );
            tracing::debug!(
                target: "gatel::runtime",
                event = "probe_started",
                upstream = id.name(),
            );
        }

        let retired: Vec<ResourceId> = workers
            .keys()
            .filter(|id| !wanted.iter().any(|(wanted, ..)| wanted == id))
            .cloned()
            .collect();
        for id in retired {
            if let Some(worker) = workers.remove(&id) {
                stop(worker).await;
                tracing::debug!(
                    target: "gatel::runtime",
                    event = "probe_stopped",
                    upstream = id.name(),
                );
            }
        }
    }

    /// Start, keep or stop the workers that look names up.
    ///
    /// Wanted for any upstream with a named backend, whether or not it is also
    /// probed: a name still has to be followed when nobody asked for health
    /// checks.
    async fn reconcile_resolvers(&self, runtimes: &Arc<UpstreamRuntimes>) {
        let mut resolvers = self.resolvers.lock().await;

        let wanted: Vec<(&ResourceId, Fingerprint, &Arc<UpstreamRuntime>)> = runtimes
            .iter()
            .filter(|runtime| runtime.spec.has_names())
            .map(|runtime| (&runtime.spec.id, resolve::worker_identity(runtime), runtime))
            .collect();

        for (id, identity, runtime) in &wanted {
            match resolvers.get(*id) {
                // The same names, looked up the same way: leave it running, so
                // a reload does not cost a round of lookups.
                Some(existing) if existing.identity == *identity && !existing.handle.is_finished() => {
                    continue;
                }
                Some(_) => {
                    if let Some(previous) = resolvers.remove(*id) {
                        stop(previous).await;
                    }
                }
                None => {}
            }
            let (sender, receiver) = watch::channel(false);
            let runtime = Arc::clone(runtime);
            let handle = tokio::spawn(crate::worker::supervise(
                gatel_observe::workers::WorkerKind::Resolver,
                id.name().to_string(),
                Arc::clone(&self.telemetry),
                receiver,
                move |receiver| resolve::resolve_loop(Arc::clone(&runtime), receiver),
            ));
            resolvers.insert(
                (*id).clone(),
                Worker {
                    identity: *identity,
                    stop: sender,
                    handle,
                },
            );
            tracing::debug!(
                target: "gatel::runtime",
                event = "resolver_started",
                upstream = id.name(),
            );
        }

        let retired: Vec<ResourceId> = resolvers
            .keys()
            .filter(|id| !wanted.iter().any(|(wanted, ..)| wanted == id))
            .cloned()
            .collect();
        for id in retired {
            if let Some(worker) = resolvers.remove(&id) {
                stop(worker).await;
                tracing::debug!(
                    target: "gatel::runtime",
                    event = "resolver_stopped",
                    upstream = id.name(),
                );
            }
        }
    }

    /// Stop every worker and wait for it, so a process exit leaves nothing
    /// half-done.
    pub async fn shutdown(&self) {
        let mut closed = self.closed.lock().await;
        *closed = true;
        self.census.shutdown().await;
        for map in [&self.workers, &self.resolvers] {
            let mut workers = map.lock().await;
            for (_, worker) in std::mem::take(&mut *workers) {
                stop(worker).await;
            }
        }
    }

    pub async fn running(&self) -> Vec<ResourceId> {
        self.workers
            .lock()
            .await
            .iter()
            .filter(|(_, worker)| !worker.handle.is_finished())
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub async fn resolving(&self) -> Vec<ResourceId> {
        self.resolvers
            .lock()
            .await
            .iter()
            .filter(|(_, worker)| !worker.handle.is_finished())
            .map(|(id, _)| id.clone())
            .collect()
    }
}

async fn stop(mut worker: Worker) {
    let _ = worker.stop.send(true);
    // A probe wakes on the stop signal, so joining is bounded by one poll of
    // its select rather than by the probe interval.
    if tokio::time::timeout(std::time::Duration::from_secs(5), &mut worker.handle)
        .await
        .is_err()
    {
        worker.handle.abort();
        let _ = (&mut worker.handle).await;
    }
}

/// What makes a probe worker the same worker across a reload: the check itself
/// and the set of endpoints it covers. Anything else may change freely.
fn worker_identity(runtime: &UpstreamRuntime) -> Fingerprint {
    let mut hasher = Hasher::new();
    match &runtime.spec.health {
        Some(health) => health.hash_into(&mut hasher),
        None => {
            hasher.update_none();
        }
    }
    hasher.update_u64(runtime.spec.protocol as u64);
    if let Some(tls) = runtime.pool.tls() {
        hasher.update_fingerprint(&tls.identity());
    }
    match &runtime.spec.tls {
        Some(tls) => tls.hash_into(&mut hasher),
        None => {
            hasher.update_none();
        }
    }
    hasher.update_unordered(&runtime.spec.endpoints, |endpoint, hasher| {
        hasher.update_str(&endpoint.id);
        endpoint.target.hash_into(hasher);
    });
    hasher.finish()
}

async fn probe_loop(
    runtime: Arc<UpstreamRuntime>,
    telemetry: Arc<Telemetry>,
    mut stop: watch::Receiver<bool>,
) {
    let Some(policy) = runtime.spec.health.clone() else {
        return;
    };
    let upstream = runtime.spec.id.name().to_string();
    let protocol = runtime.spec.protocol;
    let mut round: u64 = 0;

    loop {
        // Probe every endpoint at once: a hundred backends should take one
        // round trip, not a hundred. Read from the endpoint set rather than
        // the plan, because a name's addresses live there and change without
        // the plan changing.
        let mut checks = Vec::new();
        for endpoint in runtime.endpoints.all() {
            let kind = policy.kind.clone();
            let addresses = endpoint.addresses();
            let unix = endpoint.target.unix_path().map(std::path::Path::to_path_buf);
            let id = endpoint.id.clone();
            let timeout = policy.timeout;
            let tls = runtime.pool.tls();
            checks.push(async move {
                let result = match unix {
                    // A socket path has no addresses, so the address loop would
                    // have called it unhealthy for ever.
                    Some(path) => {
                        crate::probe::probe_unix_with_tls(&path, &kind, protocol, timeout, tls).await
                    }
                    None => probe_any(&addresses, &kind, protocol, timeout, tls).await,
                };
                (id, result)
            });
        }

        let results = tokio::select! {
            biased;
            _ = stop.changed() => return,
            results = join_all(checks) => results,
        };
        for (id, result) in &results {
            report(&runtime.endpoints, &upstream, id, result);
        }
        // Immediately, not on the census tick: a backend going down is exactly
        // the moment readiness should change.
        census::publish_one(&runtime, &telemetry);

        // Spread the next round so a large set does not probe in lockstep.
        let delay = probe_delay(&policy, round as usize, round);
        round = round.wrapping_add(1);
        tokio::select! {
            _ = stop.changed() => return,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

/// Probe every address a target currently has, and count the endpoint as
/// answering if any of them did.
///
/// The endpoint the balancer picks is the *name*, and dialling it tries each
/// address in turn — so "one address is dead" is already handled, and reporting
/// the name as unhealthy for it would take a working backend out of rotation.
async fn probe_any(
    addresses: &[std::net::SocketAddr],
    kind: &gatel_plan::HealthKind,
    protocol: gatel_plan::UpstreamProtocol,
    timeout: std::time::Duration,
    tls: Option<&gatel_tls::ClientTls>,
) -> Result<(), String> {
    if addresses.is_empty() {
        return Err("the name has not resolved to an address".to_string());
    }
    let started = tokio::time::Instant::now();
    let mut failures = Vec::new();
    for address in addresses {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(format!("endpoint probe budget exhausted after {timeout:?}"));
        }
        match probe_with_tls(*address, kind, protocol, remaining, tls).await {
            Ok(()) => return Ok(()),
            Err(error) => failures.push(format!("{address}: {error}")),
        }
    }
    Err(failures.join("; "))
}

fn report(endpoints: &EndpointSet, upstream: &str, id: &str, result: &Result<(), String>) {
    if let Some(changed) = endpoints.report_probe(id, result.is_ok()) {
        // Only a change is worth an event: probing every second must not
        // produce a log line every second.
        tracing::info!(
            target: "gatel::runtime",
            event = "endpoint_health_changed",
            upstream,
            endpoint = id,
            health = changed.as_str(),
            detail = result.as_ref().err().map(String::as_str).unwrap_or(""),
        );
    }
}

/// Await a set of futures concurrently and collect the results.
///
/// Written out rather than pulled in from `futures` so this crate keeps its
/// short dependency list; the set is small and known.
pub(crate) async fn join_all<F, T>(futures: Vec<F>) -> Vec<T>
where
    F: std::future::Future<Output = T> + Send,
    T: Send,
{
    // Poll as children of this future, not detached spawned tasks. Cancelling
    // a probe/resolve round synchronously drops every unfinished operation.
    let mut pending: Vec<_> = futures.into_iter().map(|future| Some(Box::pin(future))).collect();
    let mut results: Vec<Option<T>> = std::iter::repeat_with(|| None).take(pending.len()).collect();
    let mut remaining = pending.len();
    std::future::poll_fn(move |cx| {
        for (index, slot) in pending.iter_mut().enumerate() {
            if let Some(future) = slot
                && let std::task::Poll::Ready(value) = future.as_mut().poll(cx)
            {
                results[index] = Some(value);
                *slot = None;
                remaining -= 1;
            }
        }
        if remaining == 0 {
            std::task::Poll::Ready(
                results
                    .iter_mut()
                    .map(|value| value.take().expect("completed child"))
                    .collect(),
            )
        } else {
            std::task::Poll::Pending
        }
    })
    .await
}

#[cfg(test)]
#[path = "supervisor_cancellation_tests.rs"]
mod cancellation_tests;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gatel_model::{Hasher as ModelHasher, Snapshot, SourceId, SourceRev, SourceSnapshot};
    use gatel_plan::{Bundle, Directive, DirectiveConfig, DirectiveMeta, Plan, Registry, compile, normalize};
    use salvo_core::Handler;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::endpoint::Health;
    use crate::registry::StateRegistry;

    struct Respond(DirectiveMeta);
    struct RespondConfig;

    impl DirectiveConfig for RespondConfig {
        fn hash_into(&self, hasher: &mut ModelHasher) {
            hasher.update_str("respond");
        }

        fn build(&self, _cx: gatel_plan::BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
            Err("not built in tests".into())
        }

        fn describe(&self) -> String {
            "respond".into()
        }
    }

    impl Directive for Respond {
        fn meta(&self) -> &DirectiveMeta {
            &self.0
        }

        fn decode(
            &self,
            _: &gatel_model::Node,
            _: &mut gatel_model::Diagnostics,
        ) -> Option<Arc<dyn DirectiveConfig>> {
            Some(Arc::new(RespondConfig))
        }
    }

    pub(super) fn plan(text: &str) -> Plan {
        let registry = Registry::builder()
            .bundle(Bundle::new("std", "1").with(Respond(DirectiveMeta::terminal("respond"))))
            .build()
            .unwrap();
        let nodes = gatel_kdl::lower(text, "gatel.kdl").expect("valid KDL");
        let mut hasher = ModelHasher::new();
        hasher.update_str(text);
        let snapshot = Snapshot::new().with(SourceSnapshot::new(
            SourceId::file("main"),
            SourceRev(1),
            hasher.finish(),
            nodes,
        ));
        let spec = normalize(&snapshot, &registry).unwrap_or_else(|d| panic!("{d}"));
        compile(Arc::new(spec), &registry)
    }

    async fn backend(status: u16) -> SocketAddrHolder {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buffer = [0u8; 4096];
                    while let Ok(read) = stream.read(&mut buffer).await {
                        if read == 0 {
                            return;
                        }
                        let response = format!("HTTP/1.1 {status} X\r\nContent-Length: 0\r\n\r\n");
                        if stream.write_all(response.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        SocketAddrHolder { address, handle }
    }

    struct SocketAddrHolder {
        address: std::net::SocketAddr,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Drop for SocketAddrHolder {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    pub(super) fn config(address: std::net::SocketAddr, check: &str) -> String {
        format!(
            r#"
global {{ http "127.0.0.1:1" }}
upstream "api" {{
    endpoint "{address}"
    health {check}
}}
site "*" {{ route "/" {{ proxy upstream="api" }} }}
"#
        )
    }

    /// Wait for a condition, so a test does not depend on how fast a probe runs.
    async fn eventually(mut condition: impl FnMut() -> bool) -> bool {
        for _ in 0..100 {
            if condition() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_probe_moves_an_endpoint_from_unknown_to_healthy() {
        let backend = backend(200).await;
        let state = StateRegistry::new();
        let plan = plan(&config(
            backend.address,
            r#""http" path="/healthz" interval="50ms" timeout="40ms" healthy-threshold=1"#,
        ));
        let (runtimes, _) = UpstreamRuntimes::resolve(&plan, &state, std::path::Path::new(".")).unwrap();

        let endpoints = Arc::clone(&runtimes.iter().next().unwrap().endpoints);
        assert_eq!(endpoints.all()[0].health(), Health::Unknown);

        let supervisor = Supervisor::new(Telemetry::new());
        supervisor.reconcile(&Arc::new(runtimes)).await;
        assert!(
            eventually(|| endpoints.all()[0].health() == Health::Healthy).await,
            "the probe should have reported success"
        );
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn a_failing_backend_is_taken_out_of_rotation() {
        let backend = backend(503).await;
        let state = StateRegistry::new();
        let plan = plan(&config(
            backend.address,
            r#""http" path="/healthz" interval="50ms" timeout="40ms" unhealthy-threshold=2"#,
        ));
        let (runtimes, _) = UpstreamRuntimes::resolve(&plan, &state, std::path::Path::new(".")).unwrap();
        let endpoints = Arc::clone(&runtimes.iter().next().unwrap().endpoints);

        let supervisor = Supervisor::new(Telemetry::new());
        supervisor.reconcile(&Arc::new(runtimes)).await;
        assert!(
            eventually(|| endpoints.eligible().is_empty()).await,
            "a 503 backend is not eligible"
        );
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn an_unrelated_reload_does_not_restart_probing() {
        let backend = backend(200).await;
        let state = StateRegistry::new();
        let check = r#""tcp" interval="50ms" timeout="40ms" healthy-threshold=2"#;
        let (runtimes, _) = UpstreamRuntimes::resolve(
            &plan(&config(backend.address, check)),
            &state,
            std::path::Path::new("."),
        )
        .unwrap();
        let runtimes = Arc::new(runtimes);

        let supervisor = Supervisor::new(Telemetry::new());
        supervisor.reconcile(&runtimes).await;
        let endpoints = Arc::clone(&runtimes.iter().next().unwrap().endpoints);
        assert!(eventually(|| endpoints.all()[0].health() == Health::Healthy).await);

        // Same upstream, same check: reconciling again must leave the worker
        // alone rather than restart its counters.
        let (again, _) = UpstreamRuntimes::resolve(
            &plan(&config(backend.address, check)),
            &state,
            std::path::Path::new("."),
        )
        .unwrap();
        supervisor.reconcile(&Arc::new(again)).await;
        assert_eq!(
            endpoints.all()[0].health(),
            Health::Healthy,
            "the verdict must survive a reload"
        );
        assert_eq!(supervisor.running().await.len(), 1);
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn removing_the_health_check_stops_the_worker() {
        let backend = backend(200).await;
        let state = StateRegistry::new();
        let (with, _) = UpstreamRuntimes::resolve(
            &plan(&config(
                backend.address,
                r#""tcp" interval="50ms" timeout="40ms""#,
            )),
            &state,
            std::path::Path::new("."),
        )
        .unwrap();

        let supervisor = Supervisor::new(Telemetry::new());
        supervisor.reconcile(&Arc::new(with)).await;
        assert_eq!(supervisor.running().await.len(), 1);

        let without = format!(
            r#"
global {{ http "127.0.0.1:1" }}
upstream "api" {{ endpoint "{}" }}
site "*" {{ route "/" {{ proxy upstream="api" }} }}
"#,
            backend.address
        );
        let (plain, _) =
            UpstreamRuntimes::resolve(&plan(&without), &state, std::path::Path::new(".")).unwrap();
        supervisor.reconcile(&Arc::new(plain)).await;
        assert!(supervisor.running().await.is_empty(), "no check means no worker");
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn an_upstream_without_a_check_never_starts_a_worker() {
        let state = StateRegistry::new();
        let plan = plan(
            r#"
global { http "127.0.0.1:1" }
upstream "api" { endpoint "127.0.0.1:9" }
site "*" { route "/" { proxy upstream="api" } }
"#,
        );
        let (runtimes, _) = UpstreamRuntimes::resolve(&plan, &state, std::path::Path::new(".")).unwrap();
        let supervisor = Supervisor::new(Telemetry::new());
        supervisor.reconcile(&Arc::new(runtimes)).await;
        assert!(supervisor.running().await.is_empty());
    }
}
