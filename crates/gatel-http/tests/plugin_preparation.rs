//! Handler and task share one prepared resource, independent of route fan-out.
mod support;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use gatel_model::{Diagnostics, Hasher, Node};
use gatel_plan::{
    BackgroundTask, BuildContext, Bundle, Directive, DirectiveConfig, DirectiveMeta, Outcome, Phase,
    PreparedDirective, Registry, Scopes,
};
use salvo_core::http::{Request, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

#[derive(Default)]
struct Counts {
    built: AtomicUsize,
    started: AtomicUsize,
    dropped: AtomicUsize,
}
struct Shared {
    id: usize,
    ready: AtomicBool,
    counts: Arc<Counts>,
}
impl Drop for Shared {
    fn drop(&mut self) {
        self.counts.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
struct Plugin {
    terminal: bool,
    counts: Arc<Counts>,
}
struct Config {
    terminal: bool,
    mode: String,
    counts: Arc<Counts>,
}
struct Endpoint {
    terminal: bool,
    shared: Arc<Shared>,
}
impl Directive for Plugin {
    fn meta(&self) -> &DirectiveMeta {
        static COMMON: DirectiveMeta = DirectiveMeta::middleware("shared", Phase::Observe, Scopes::BOTH);
        static LOCAL: DirectiveMeta = DirectiveMeta::terminal("local");
        if self.terminal { &LOCAL } else { &COMMON }
    }
    fn decode(&self, node: &Node, _: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        Some(Arc::new(Config {
            terminal: self.terminal,
            counts: self.counts.clone(),
            mode: node.args.first().and_then(|v| v.as_str()).unwrap_or("ok").into(),
        }))
    }
}
impl DirectiveConfig for Config {
    fn hash_into(&self, hash: &mut Hasher) {
        hash.update_bool(self.terminal);
        hash.update_str(&self.mode);
    }
    fn describe(&self) -> String {
        "unified preparation fixture".into()
    }
    // No build/background implementations: the host must use the unified hook.
    fn prepare(&self, _: BuildContext<'_>) -> Result<PreparedDirective, String> {
        let shared = Arc::new(Shared {
            id: self.counts.built.fetch_add(1, Ordering::SeqCst),
            ready: AtomicBool::new(false),
            counts: self.counts.clone(),
        });
        if self.mode == "reject" {
            return Err("candidate resource rejected".into());
        }
        let worker = shared.clone();
        let task = BackgroundTask::cooperative("owner", move |mut shutdown| async move {
            worker.ready.store(true, Ordering::SeqCst);
            worker.counts.started.fetch_add(1, Ordering::SeqCst);
            shutdown.requested().await;
            Ok(())
        });
        Ok(PreparedDirective::new(Arc::new(Endpoint {
            terminal: self.terminal,
            shared,
        }))
        .with_tasks(vec![task]))
    }
}
#[async_trait]
impl Handler for Endpoint {
    async fn handle(&self, _: &mut Request, _: &mut Depot, response: &mut Response, _: &mut FlowCtrl) {
        assert!(
            self.shared.ready.load(Ordering::SeqCst),
            "handler and worker did not share preparation"
        );
        response.headers_mut().insert(
            if self.terminal { "x-local" } else { "x-shared" },
            self.shared.id.to_string().parse().unwrap(),
        );
        if self.terminal {
            response.status_code(StatusCode::OK);
            response.render("ok");
        }
    }
}
async fn wait(counter: &AtomicUsize, value: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while counter.load(Ordering::SeqCst) != value {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
fn header(response: &str, name: &str) -> String {
    response
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.trim().to_owned())
        })
        .unwrap()
}

#[tokio::test]
async fn unified_resources_are_shared_by_owner_and_failed_prepare_releases_them() {
    let counts = Arc::new(Counts::default());
    let mut gateway = support::Gateway::new("unified-prepare");
    gateway.registry = Registry::builder()
        .bundle(
            Bundle::new("fixture", "1")
                .with(Plugin {
                    terminal: false,
                    counts: counts.clone(),
                })
                .with(Plugin {
                    terminal: true,
                    counts: counts.clone(),
                }),
        )
        .build()
        .unwrap();
    let a = support::free_port();
    let b = support::free_port();
    let source = |mode: &str| {
        format!(
            r#"
global {{ http "a" "127.0.0.1:{a}"; http "b" "127.0.0.1:{b}"; }}
site "*" {{ shared; route "/a" {{ local "{mode}"; }}; route "/b" {{ local; }}; }}
"#
        )
    };
    let initial = gateway.apply(&source("ok")).await;
    assert!(matches!(initial, Outcome::Started { .. }), "{initial:?}");
    wait(&counts.started, 3).await;
    assert_eq!(counts.built.load(Ordering::SeqCst), 3);
    let first = support::get(([127, 0, 0, 1], a).into(), "/a").await;
    let other_route = support::get(([127, 0, 0, 1], a).into(), "/b").await;
    let other_listener = support::get(([127, 0, 0, 1], b).into(), "/a").await;
    let fallback = support::get(([127, 0, 0, 1], b).into(), "/missing").await;
    assert!(first.starts_with("HTTP/1.1 200"));
    assert_eq!(header(&first, "x-shared"), header(&other_route, "x-shared"));
    assert_eq!(header(&first, "x-shared"), header(&fallback, "x-shared"));
    assert_eq!(header(&first, "x-local"), header(&other_listener, "x-local"));
    assert_ne!(header(&first, "x-local"), header(&other_route, "x-local"));
    assert!(matches!(gateway.apply(&source("ok")).await, Outcome::NoOp { .. }));
    assert_eq!(counts.built.load(Ordering::SeqCst), 3);
    assert!(matches!(
        gateway.apply(&source("reject")).await,
        Outcome::PrepareFailed(_)
    ));
    assert_eq!(counts.started.load(Ordering::SeqCst), 3);
    assert_eq!(
        counts.dropped.load(Ordering::SeqCst),
        counts.built.load(Ordering::SeqCst) - 3
    );
    let retained = support::get(([127, 0, 0, 1], a).into(), "/a").await;
    assert_eq!(header(&first, "x-local"), header(&retained, "x-local"));
    gateway.stop().await;
    drop(gateway);
    wait(&counts.dropped, counts.built.load(Ordering::SeqCst)).await;
}
