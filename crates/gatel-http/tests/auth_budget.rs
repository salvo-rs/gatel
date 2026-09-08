//! Host-injected budget through prepare, reload and real HTTP authentication.
mod support;
use std::sync::Arc;

use gatel_plan::blocking::{Budget, Limits};
use support::*;

async fn authenticated(gateway: &Gateway, status: u16) -> String {
    let response = request(gateway.address(),
        "GET / HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic YWRtaW46Zml4dHVyZQ==\r\nConnection: close\r\n\r\n").await;
    assert!(response.starts_with(&format!("HTTP/1.1 {status}")), "{response}");
    if status == 503 {
        assert!(!response.to_ascii_lowercase().contains("www-authenticate:"));
    }
    response
}

#[tokio::test]
async fn injected_memory_budget_survives_reload_and_failed_prepare() {
    let mut gateway = Gateway::new("auth-injected-budget");
    let budget = Budget::new(Limits {
        jobs: 2,
        memory_bytes: 8192,
    })
    .unwrap();
    let executor = budget.executor();
    gateway.server = Arc::new(gatel_http::Server::with_blocking_executor(
        gateway.store.clone(),
        gateway.binds.clone(),
        executor.clone(),
    ));
    write_key(
        &gateway.directory.join("admin.hash"),
        &bcrypt::hash("fixture", 4).unwrap(),
    );
    let port = free_port();
    let config = |body: &str, hash: &str| {
        format!(
            r#"
global {{ http "127.0.0.1:{port}"; }}
site "*" {{ route "/*" {{
 basic-auth {{ user "admin" hash="{hash}"; }}
 respond "{body}";
}} }}"#
        )
    };
    assert!(matches!(
        gateway.apply(&config("one", "admin.hash")).await,
        gatel_plan::Outcome::Started { .. }
    ));
    authenticated(&gateway, 200).await;

    let worker = executor.clone();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let running = tokio::spawn(async move {
        worker
            .run(8192, move || {
                started.send(()).unwrap();
                released.recv().unwrap();
            })
            .await
            .unwrap();
    });
    ready.await.unwrap();
    // One job slot remains, but the shared memory reservation is exhausted.
    authenticated(&gateway, 503).await;
    gateway.apply(&config("two", "admin.hash")).await;
    assert_eq!(gateway.store.status().generation.number(), 2);
    authenticated(&gateway, 503).await;
    assert!(matches!(
        gateway.apply(&config("bad", "absent.hash")).await,
        gatel_plan::Outcome::PrepareFailed(_)
    ));
    assert_eq!(gateway.store.status().generation.number(), 2);
    assert_eq!(executor.snapshot().jobs, 1);
    assert_eq!(executor.snapshot().memory_bytes, 8192);
    budget
        .reconfigure(Limits {
            jobs: 1,
            memory_bytes: 4096,
        })
        .unwrap();
    assert_eq!(executor.snapshot().memory_bytes, 8192);
    release.send(()).unwrap();
    running.await.unwrap();
    // Even without outstanding work, this credential cannot fit the new limit.
    authenticated(&gateway, 503).await;
    budget
        .reconfigure(Limits {
            jobs: 1,
            memory_bytes: 8192,
        })
        .unwrap();
    assert!(authenticated(&gateway, 200).await.ends_with("two"));
    assert_eq!(executor.snapshot().jobs, 0);
    assert_eq!(executor.snapshot().memory_bytes, 0);
    budget.close();
    authenticated(&gateway, 503).await;
    gateway.stop().await;
}
