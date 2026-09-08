//! Does anything grow that should not?
//!
//! A gateway is a process that is expected to run for months. Nothing in the
//! rest of the suite would notice a leak: every other test starts a gateway,
//! does one thing and drops it, so anything that grows by a little each time
//! round finishes far too small to see.
//!
//! So this drives traffic and reloads in a loop, samples what the process is
//! holding, and asserts that the samples stop growing. It measures what the
//! gateway itself can count — state slots, metric series, parked connections,
//! background workers, live tasks — rather than only resident memory, because
//! those say *what* leaked, and an allocator that has not returned pages to
//! the operating system says nothing at all.
//!
//! Ignored by default: it takes as long as it is told to. Run it with
//!
//! ```text
//! GATEL_SOAK_SECONDS=300 cargo test -p gatel-http --test soak -- --ignored --nocapture
//! ```
//!
//! A short run can expose per-request or per-reload growth in these workloads;
//! it does not rule out slow leaks or exercise every protocol. A release soak
//! needs hours and a broader workload matrix.

mod support;

use std::time::{Duration, Instant};

use support::*;

/// What the process is holding, at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sample {
    /// Slots in the state registry: endpoint sets, pools, selectors, limiters.
    state: usize,
    /// Lines a Prometheus scrape would produce. Stands in for series, and is
    /// also what the scrape actually costs.
    metrics: usize,
    /// Access-log sinks. Keyed by path and held weakly, so a reload that stops
    /// naming a file has to let it go.
    sinks: usize,
    /// Connections parked in every pool.
    idle: usize,
    /// Tasks the runtime believes are alive.
    tasks: usize,
    /// Resources the cache remembers a `Vary` list for.
    ///
    /// Watched separately to catch regressions in metadata accounting as well
    /// as body retention; the current cache budget includes both.
    cached_resources: usize,
}

impl Sample {
    fn take(gateway: &Gateway) -> Self {
        let idle = gateway
            .store
            .current()
            .map(|committed| {
                committed
                    .runtime
                    .upstreams
                    .iter()
                    .flat_map(|upstream| upstream.pool.stats())
                    .map(|stats| stats.idle)
                    .sum()
            })
            .unwrap_or(0);

        Sample {
            state: gateway.state.len(),
            metrics: gatel_observe::render::prometheus(&gateway.telemetry)
                .lines()
                .count(),
            sinks: gateway.telemetry.sinks.snapshot().len(),
            idle,
            tasks: tokio::runtime::Handle::current().metrics().num_alive_tasks(),
            cached_resources: caches(gateway).map(|store| store.remembered_resources()).sum(),
        }
    }

    /// Whether this is bigger than `earlier` by more than the slack a healthy
    /// process needs — a probe between rounds, a connection not yet reaped.
    fn grew_beyond(&self, earlier: &Sample, slack: usize) -> Option<String> {
        let checks = [
            ("state slots", self.state, earlier.state),
            ("metric lines", self.metrics, earlier.metrics),
            ("log sinks", self.sinks, earlier.sinks),
            ("idle connections", self.idle, earlier.idle),
            ("live tasks", self.tasks, earlier.tasks),
            (
                "remembered resources",
                self.cached_resources,
                earlier.cached_resources,
            ),
        ];
        for (what, now, before) in checks {
            if now > before + slack {
                return Some(format!("{what} went from {before} to {now}"));
            }
        }
        None
    }
}

/// Every cache store the committed generation is using.
fn caches(gateway: &Gateway) -> impl Iterator<Item = std::sync::Arc<gatel_bundle_std::cache::Store>> {
    gateway
        .state
        .keys()
        .into_iter()
        .filter(|key| key.kind == gatel_plan::StateKind::Directive("cache"))
        .filter_map(|key| gateway.state.get(&key.owner, key.kind))
}

/// Resident memory, where the operating system will say. Reported rather than
/// asserted on: an allocator is free to keep pages it is not using, so a rise
/// here is a thing to look at rather than a thing to fail on.
fn resident_kib() -> Option<u64> {
    // /proc reports kB here; do not assume all Linux kernels use 4 KiB pages.
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find_map(|line| line.strip_prefix("VmRSS:"))?;
    let mut parts = line.split_whitespace();
    let amount = parts.next()?.parse().ok()?;
    (parts.next()? == "kB").then_some(amount)
}

fn budget() -> Duration {
    let value = match std::env::var("GATEL_SOAK_SECONDS") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => panic!("GATEL_SOAK_SECONDS must be a positive decimal integer"),
    };
    parse_budget(value.as_deref()).expect("GATEL_SOAK_SECONDS must be a positive decimal integer")
}

fn parse_budget(value: Option<&str>) -> Result<Duration, ()> {
    let Some(value) = value else {
        return Ok(Duration::from_secs(30));
    };
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(());
    }
    let seconds = value.parse::<u64>().map_err(|_| ())?;
    if seconds == 0 {
        return Err(());
    }
    Ok(Duration::from_secs(seconds))
}

#[test]
fn explicit_soak_budget_never_silently_falls_back_to_a_short_run() {
    assert_eq!(parse_budget(None), Ok(Duration::from_secs(30)));
    assert_eq!(parse_budget(Some("1800")), Ok(Duration::from_secs(1800)));
    for value in [
        "",
        "0",
        "-1",
        "+60",
        "60s",
        "1.5",
        " 60",
        "60 ",
        "18446744073709551616",
    ] {
        assert!(parse_budget(Some(value)).is_err(), "{value:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "takes as long as GATEL_SOAK_SECONDS; run it before a release"]
async fn nothing_grows_without_bound_under_traffic_and_reloads() {
    // Validate explicit input before creating fixtures, listeners or workers.
    let budget = budget();
    eprintln!("soak: requested duration {} seconds", budget.as_secs());
    let gateway = Gateway::new("soak");
    let supervisor = gatel_state::Supervisor::new(gateway.telemetry.clone());
    let port = free_port();
    let backend = backend("soak").await;
    // Under `static/`, because `file-server` resolves the request path against
    // the root rather than stripping the route's prefix from it.
    let root = gateway.directory.join("public");
    std::fs::create_dir_all(root.join("static")).unwrap();
    std::fs::write(root.join("static/index.html"), "x".repeat(200_000)).unwrap();
    let root = root.display().to_string().replace('\\', "/");

    // Two configurations that differ in ways a reload treats differently: the
    // first change is compatible and keeps state, the second replaces an
    // upstream's endpoints and does not. Alternating exercises both the path
    // that reuses and the path that retires.
    let configs = [
        format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "api" {{
    endpoint "{backend}"
    pool max-per-endpoint=8
    health "tcp" interval="1s" timeout="500ms"
}}
site "*" {{
    access-log format="clf" output="file:access.log" rotate="1MiB" keep=2
    route "/api/*" {{ cache default-ttl="2s" max-size="4KiB" max-entry="1KiB"
        proxy upstream="api" }}
    route "/static/*" {{ file-server root="{root}" }}
    route "/metrics" {{ metrics }}
    route "/" {{ respond "ok" }}
}}
"#
        ),
        format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "api" {{
    endpoint "{backend}"
    pool max-per-endpoint=16
    health "tcp" interval="1s" timeout="500ms"
}}
site "*" {{
    access-log format="clf" output="file:access.log" rotate="1MiB" keep=2
    route "/api/*" {{ cache default-ttl="2s" max-size="4KiB" max-entry="1KiB"
        proxy upstream="api" }}
    route "/static/*" {{ file-server root="{root}" }}
    route "/metrics" {{ metrics }}
    route "/" {{ respond "ok" }}
}}
"#
        ),
        format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "api" {{
    endpoint "{backend}" id="renamed"
    pool max-per-endpoint=8
}}
site "*" {{
    access-log format="clf" output="file:access.log" rotate="1MiB" keep=2
    route "/api/*" {{ cache default-ttl="2s" max-size="4KiB" max-entry="1KiB"
        proxy upstream="api" }}
    route "/static/*" {{ file-server root="{root}" }}
    route "/metrics" {{ metrics }}
    route "/" {{ respond "ok" }}
}}
"#
        ),
    ];

    let outcome = gateway.apply(&configs[0]).await;
    assert!(!outcome.is_failure(), "initial configuration: {outcome:?}");
    activate(&gateway, &supervisor).await;
    let address = gateway.address();

    // Rounds before the first sample. A process that has just started has not
    // yet opened every connection it will keep, and — more slowly — a cache
    // being fed a new path every round is still filling toward its budget.
    // Measuring before it is full would call the filling a leak; measuring
    // after is what makes the plateau the assertion.
    const WARMUP: usize = 80;
    let started = Instant::now();
    let mut round = 0usize;
    let mut baseline: Option<Sample> = None;
    let started_resident = resident_kib();

    while started.elapsed() < budget {
        // A fresh path each round, so the cache is asked to remember
        // something new every time — which is what makes a leak in what it
        // remembers show up rather than staying at one entry.
        let unique = format!("/api/thing-{round}");
        for path in ["/", unique.as_str(), "/static/index.html", "/metrics"] {
            let answer = get(address, path).await;
            assert!(
                answer.starts_with("HTTP/1.1 2"),
                "round {round} lost {path}: {}",
                answer.lines().next().unwrap_or("")
            );
        }
        // The whole 200 KiB file above streams; this 1001-byte range exercises
        // the inline path and must describe exactly the requested bytes.
        let ranged = request(
            address,
            "GET /static/index.html HTTP/1.1\r\nHost: x\r\nRange: bytes=1000-2000\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(ranged.starts_with("HTTP/1.1 206"), "round {round}: {ranged}");
        assert!(
            ranged
                .to_ascii_lowercase()
                .contains("content-range: bytes 1000-2000/200000")
        );
        assert_eq!(ranged.split_once("\r\n\r\n").unwrap().1, "x".repeat(1001));

        let outcome = gateway.apply(&configs[round % configs.len()]).await;
        assert!(!outcome.is_failure(), "round {round} reload failed: {outcome:?}");
        activate(&gateway, &supervisor).await;
        round += 1;

        if round == WARMUP {
            // Let the reload that just happened settle before the baseline.
            tokio::time::sleep(Duration::from_millis(200)).await;
            baseline = Some(Sample::take(&gateway));
            eprintln!("soak: baseline after {round} rounds: {:?}", baseline.unwrap());
        } else if round > WARMUP && round.is_multiple_of(20) {
            let now = Sample::take(&gateway);
            let baseline = baseline.expect("a baseline is taken before any comparison");
            // Slack covers what is genuinely in motion: a probe between
            // rounds, a connection returned but not yet reaped, the task
            // driving the request that is being served right now.
            if let Some(growth) = now.grew_beyond(&baseline, 8) {
                panic!("round {round}: {growth}\nbaseline {baseline:?}\nnow      {now:?}");
            }
            eprintln!("soak: round {round}: {now:?}");
        }
    }

    assert!(
        round > WARMUP + 20,
        "the budget was too short to prove anything: {round} rounds"
    );

    let finished = Sample::take(&gateway);
    let baseline = baseline.expect("a baseline is taken before any comparison");
    if let Some(growth) = finished.grew_beyond(&baseline, 8) {
        panic!("after {round} rounds: {growth}\nbaseline {baseline:?}\nnow      {finished:?}");
    }

    if let (Some(before), Some(after)) = (started_resident, resident_kib()) {
        eprintln!("soak: resident {before} KiB -> {after} KiB over {round} rounds");
    }
    eprintln!("soak: {round} rounds, flat: {finished:?}");
    tokio::time::timeout(Duration::from_secs(5), async {
        gateway.server.shutdown().await;
        supervisor.shutdown().await;
    })
    .await
    .expect("listeners and background workers drain after traffic");
}

/// Match the host's post-commit activation for the components under test.
/// Gateway::apply starts listeners, but does not run the host's supervisor.
async fn activate(gateway: &Gateway, supervisor: &gatel_state::Supervisor) {
    let committed = gateway.store.current().expect("configuration committed");
    supervisor.reconcile(&committed.runtime.upstreams).await;
    gateway.retain_series();
    gateway.state.retain(&committed.plan.bindings);
}

/// Every round introduces identities that can never be reused by a later one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retired_generations_and_caches_are_released_under_identity_churn() {
    let gateway = Gateway::new("identity-churn");
    let supervisor = gatel_state::Supervisor::new(gateway.telemetry.clone());
    let backend = backend("churn").await;
    let port = free_port();
    let mut generations = Vec::new();
    let mut retired_caches = Vec::new();
    let mut baseline = None;
    for round in 0..120 {
        let config = format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "api-{round}" {{ endpoint "{backend}" id="endpoint-{round}"
    health "tcp" interval="1s" timeout="500ms"
}}
site "*" id="site-{round}" {{
    access-log output="file:access-{round}.log"
    route "/*" id="route-{round}" {{
        cache default-ttl="60s" max-size="4KiB" max-entry="1KiB"
        proxy upstream="api-{round}"
    }}
}}
"#
        );
        let outcome = gateway.apply(&config).await;
        assert!(
            matches!(
                outcome,
                gatel_plan::Outcome::Started { .. } | gatel_plan::Outcome::Reloaded { .. }
            ),
            "round {round}: {outcome:?}"
        );
        activate(&gateway, &supervisor).await;
        let address = gateway.address();
        for _ in 0..2 {
            let answer = get(address, "/cached").await;
            assert!(answer.starts_with("HTTP/1.1 200"), "round {round}: {answer}");
        }
        generations.push(std::sync::Arc::downgrade(&gateway.store.current().unwrap()));
        let stores = caches(&gateway).collect::<Vec<_>>();
        assert_eq!(stores.len(), 1, "only the active cache should be discoverable");
        retired_caches.push(std::sync::Arc::downgrade(&stores[0]));
        let sample = Sample::take(&gateway);
        if round == 19 {
            baseline = Some(sample);
        } else if let Some(baseline) = baseline {
            assert!(
                sample.grew_beyond(&baseline, 8).is_none(),
                "round {round}: baseline {baseline:?}, now {sample:?}"
            );
        }
    }
    // Current ownership remains live; previous values must actually be freed,
    // not merely disappear from a registry.
    generations.pop();
    retired_caches.pop();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let generations = generations.iter().filter(|weak| weak.strong_count() != 0).count();
            let caches = retired_caches
                .iter()
                .filter(|weak| weak.strong_count() != 0)
                .count();
            if generations == 0 && caches == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "retired ownership: {error}; generations {:?}; caches {:?}",
            generations
                .iter()
                .enumerate()
                .filter(|(_, weak)| weak.strong_count() != 0)
                .map(|(round, weak)| (round, weak.strong_count()))
                .collect::<Vec<_>>(),
            retired_caches
                .iter()
                .enumerate()
                .filter(|(_, weak)| weak.strong_count() != 0)
                .map(|(round, weak)| (round, weak.strong_count()))
                .collect::<Vec<_>>()
        )
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        gateway.server.shutdown().await;
        supervisor.shutdown().await;
    })
    .await
    .expect("identity churn drains listeners and workers");
}
