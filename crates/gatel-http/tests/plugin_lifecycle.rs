//! Managed plugin work exercised through real normalize/prepare/commit paths.
mod support;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gatel_model::{Diagnostics, Hasher, Node, ResourceId, ResourceKind};
use gatel_plan::{
    BackgroundTask, BuildContext, Bundle, Directive, DirectiveConfig, DirectiveMeta, Outcome, Phase,
    Registry, Scopes,
};
use salvo_core::http::{Request, Response};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

#[derive(Default)]
struct Counts {
    started: AtomicUsize,
    stopped: AtomicUsize,
    cleaned: AtomicUsize,
}
struct Guard(Arc<Counts>);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.stopped.fetch_add(1, Ordering::SeqCst);
    }
}

struct WorkerPlugin(Arc<Counts>);
struct Config {
    counts: Arc<Counts>,
    mode: String,
}
struct Pass;
#[async_trait]
impl Handler for Pass {
    async fn handle(&self, _: &mut Request, _: &mut Depot, _: &mut Response, _: &mut FlowCtrl) {}
}
impl Directive for WorkerPlugin {
    fn meta(&self) -> &DirectiveMeta {
        static META: DirectiveMeta = DirectiveMeta::middleware("worker", Phase::Observe, Scopes::BOTH);
        &META
    }
    fn decode(&self, node: &Node, _: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        Some(Arc::new(Config {
            counts: self.0.clone(),
            mode: node
                .args
                .first()
                .and_then(|value| value.as_str())
                .unwrap_or("ok")
                .into(),
        }))
    }
}
impl DirectiveConfig for Config {
    fn hash_into(&self, hash: &mut Hasher) {
        hash.update_str(&self.mode);
    }
    fn describe(&self) -> String {
        "worker fixture".into()
    }
    fn upstreams(&self) -> Vec<ResourceId> {
        vec![ResourceId::new(ResourceKind::Upstream, "auth")]
    }
    fn build(&self, cx: BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        check_grants(&cx)?;
        Ok(Arc::new(Pass))
    }
    fn background(&self, cx: BuildContext<'_>) -> Result<Vec<BackgroundTask>, String> {
        check_grants(&cx)?;
        let count = if self.mode == "invalid" { 2 } else { 1 };
        Ok((0..count)
            .map(|_| {
                let counts = self.counts.clone();
                if self.mode == "cooperative" {
                    return BackgroundTask::cooperative("tick", move |mut shutdown| async move {
                        let _guard = Guard(counts.clone());
                        counts.started.fetch_add(1, Ordering::SeqCst);
                        shutdown.requested().await;
                        // Cleanup genuinely suspends; abort cannot substitute for awaiting it.
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        counts.cleaned.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    });
                }
                BackgroundTask::new("tick", move || async move {
                    let _guard = Guard(counts.clone());
                    counts.started.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<()>().await;
                })
            })
            .collect())
    }
}

#[tokio::test]
async fn committed_cooperative_plugin_cleanup_is_awaited_after_rejected_candidate() {
    let counts = Arc::new(Counts::default());
    let mut gateway = support::Gateway::new("plugin-cleanup");
    gateway.registry = Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .bundle(Bundle::new("test-worker", "1").with(WorkerPlugin(counts.clone())))
        .build()
        .unwrap();
    let port = support::free_port();
    let source = |mode: &str| {
        format!(
            r#"
global {{ http "127.0.0.1:{port}"; }}
upstream "auth" {{ endpoint "127.0.0.1:1"; }}
upstream "admin" {{ endpoint "127.0.0.1:2"; }}
site "*" {{ worker "{mode}"; route "/" {{ respond "ok"; }}; }}
"#
        )
    };
    assert!(matches!(
        gateway.apply(&source("cooperative")).await,
        Outcome::Started { .. }
    ));
    wait_for(&counts.started, 1).await;
    assert!(matches!(
        gateway.apply(&source("invalid")).await,
        Outcome::PrepareFailed(_)
    ));
    assert_eq!(counts.started.load(Ordering::SeqCst), 1);
    assert_eq!(counts.cleaned.load(Ordering::SeqCst), 0);
    assert!(support::get(gateway.address(), "/").await.ends_with("ok"));
    let current = gateway.store.current().unwrap();
    current.runtime.plugin_tasks.shutdown().await;
    assert_eq!(counts.cleaned.load(Ordering::SeqCst), 1);
    assert_eq!(counts.stopped.load(Ordering::SeqCst), 1);
    assert_eq!(
        current.runtime.plugin_tasks.outcomes(),
        vec![("tick", gatel_http::plugin_tasks::TaskOutcome::Completed)]
    );
    gateway.stop().await;
}

#[tokio::test]
async fn reload_retires_tasks_without_retaining_old_runtime_and_server_joins_cleanup() {
    let counts = Arc::new(Counts::default());
    let mut gateway = support::Gateway::new("plugin-retirement");
    gateway.registry = Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .bundle(Bundle::new("test-worker", "1").with(WorkerPlugin(counts.clone())))
        .build()
        .unwrap();
    let port = support::free_port();
    let source = |body: &str| {
        format!(
            r#"
global {{ http "127.0.0.1:{port}"; }}
upstream "auth" {{ endpoint "127.0.0.1:1"; }}
upstream "admin" {{ endpoint "127.0.0.1:2"; }}
site "*" {{ worker "cooperative"; route "/" {{ respond "{body}"; }}; }}
"#
        )
    };
    assert!(matches!(
        gateway.apply(&source("one")).await,
        Outcome::Started { .. }
    ));
    wait_for(&counts.started, 1).await;
    let old = gateway.store.current().unwrap();
    let weak = Arc::downgrade(&old.runtime);
    assert!(matches!(
        gateway.apply(&source("two")).await,
        Outcome::Reloaded { .. }
    ));
    wait_for(&counts.started, 2).await;
    assert_eq!(counts.cleaned.load(Ordering::SeqCst), 0);
    drop(old);
    assert!(
        weak.upgrade().is_none(),
        "retirement must not retain the old runtime"
    );
    wait_for(&counts.cleaned, 1).await;
    assert_eq!(counts.stopped.load(Ordering::SeqCst), 1);
    assert!(support::get(gateway.address(), "/").await.ends_with("two"));
    // The current generation stays in the store, but host shutdown still drains it.
    gateway.stop().await;
    assert_eq!(counts.cleaned.load(Ordering::SeqCst), 2);
    assert_eq!(counts.stopped.load(Ordering::SeqCst), 2);
    assert_eq!(gateway.server.plugin_retirements().tracked_generations(), 0);
}

#[tokio::test]
async fn retirement_capacity_rejects_reload_and_recovers_after_old_generation_release() {
    let counts = Arc::new(Counts::default());
    let mut gateway = support::Gateway::new("retirement-capacity");
    gateway.registry = Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .bundle(Bundle::new("test-worker", "1").with(WorkerPlugin(counts.clone())))
        .build()
        .unwrap();
    let port = support::free_port();
    let source = |body: usize| {
        format!(
            r#"
global {{ http "127.0.0.1:{port}"; }}
upstream "auth" {{ endpoint "127.0.0.1:1"; }}
upstream "admin" {{ endpoint "127.0.0.1:2"; }}
site "*" {{ worker "ok"; route "/" {{ respond "version-{body}"; }}; }}
"#
        )
    };
    let limit = gatel_http::plugin_retirement::Retirements::LIMIT;
    let mut pinned = Vec::new();
    for generation in 0..limit {
        let outcome = gateway.apply(&source(generation)).await;
        assert!(
            matches!(outcome, Outcome::Started { .. } | Outcome::Reloaded { .. }),
            "{outcome:?}"
        );
        pinned.push(gateway.store.current().unwrap());
    }
    wait_for(&counts.started, limit).await;
    let rejected = gateway.apply(&source(limit)).await;
    assert!(matches!(rejected, Outcome::PrepareFailed(_)), "{rejected:?}");
    assert_eq!(counts.started.load(Ordering::SeqCst), limit);
    assert!(
        support::get(gateway.address(), "/")
            .await
            .ends_with(&format!("version-{}", limit - 1))
    );
    drop(pinned.remove(0));
    wait_for(&counts.stopped, 1).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while gateway.server.plugin_retirements().tracked_generations() == limit {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        gateway.apply(&source(limit)).await,
        Outcome::Reloaded { .. }
    ));
    wait_for(&counts.started, limit + 1).await;
    assert!(
        support::get(gateway.address(), "/")
            .await
            .ends_with(&format!("version-{limit}"))
    );
    gateway.stop().await;
    assert_eq!(counts.stopped.load(Ordering::SeqCst), limit + 1);
    drop(pinned);
}

fn check_grants(cx: &BuildContext<'_>) -> Result<(), String> {
    let auth = ResourceId::new(ResourceKind::Upstream, "auth");
    let admin = ResourceId::new(ResourceKind::Upstream, "admin");
    cx.upstream(&auth)?;
    if cx.upstreams.has(&admin) || cx.upstream(&admin).is_ok() {
        return Err("host exposed an undeclared backend".into());
    }
    Ok(())
}

async fn wait_for(counter: &AtomicUsize, count: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while counter.load(Ordering::SeqCst) != count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn tasks_are_deduplicated_transactional_and_pinned_to_the_generation() {
    let counts = Arc::new(Counts::default());
    let mut gateway = support::Gateway::new("plugin-tasks");
    gateway.registry = Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .bundle(Bundle::new("test-worker", "1").with(WorkerPlugin(counts.clone())))
        .build()
        .unwrap();
    let a = support::free_port();
    let b = support::free_port();
    let source = |mode: &str, body: &str| {
        format!(
            r#"
global {{ http "a" "127.0.0.1:{a}"; http "b" "127.0.0.1:{b}"; }}
upstream "auth" {{ endpoint "127.0.0.1:1"; }}
upstream "admin" {{ endpoint "127.0.0.1:2"; }}
site "*" {{
 worker "{mode}"
 route "/" {{ respond "{body}"; }}
 route "/other" {{ respond "other"; }}
}}
"#
        )
    };
    let initial = gateway.apply(&source("ok", "one")).await;
    assert!(matches!(initial, Outcome::Started { .. }), "{initial:?}");
    wait_for(&counts.started, 1).await; // Not one per route or listener.
    gateway.server.sync().await;
    assert!(matches!(
        gateway.apply(&source("ok", "one")).await,
        Outcome::NoOp { .. }
    ));
    assert_eq!(counts.started.load(Ordering::SeqCst), 1);

    // Duplicate task names fail prepare after factories have been collected.
    assert!(matches!(
        gateway.apply(&source("invalid", "bad")).await,
        Outcome::PrepareFailed(_)
    ));
    assert_eq!(counts.started.load(Ordering::SeqCst), 1);
    assert!(support::get(gateway.address(), "/").await.ends_with("one"));

    // A real connection holds precisely this committed Arc for its lifetime.
    let pinned = gateway.store.current().unwrap();
    assert!(matches!(
        gateway.apply(&source("ok", "two")).await,
        Outcome::Reloaded { .. }
    ));
    wait_for(&counts.started, 2).await;
    assert_eq!(counts.stopped.load(Ordering::SeqCst), 0);
    drop(pinned);
    wait_for(&counts.stopped, 1).await;
    gateway
        .store
        .current()
        .unwrap()
        .runtime
        .plugin_tasks
        .shutdown()
        .await;
    assert_eq!(counts.stopped.load(Ordering::SeqCst), 2);
    gateway.stop().await;
}
