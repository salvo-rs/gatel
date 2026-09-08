//! Configured budgets publish with the generation and affect old connections.
mod support;
use std::sync::Arc;

use gatel_plan::Outcome;
use gatel_plan::blocking::{Budget, Limits};
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn head(stream: &mut tokio::net::TcpStream, status: u16) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        stream
            .write_all(
                b"HEAD / HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic YWRtaW46Zml4dHVyZQ==\r\n\r\n",
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        while !response.ends_with(b"\r\n\r\n") {
            assert!(response.len() < 65536);
            response.push(stream.read_u8().await.unwrap());
        }
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with(&format!("HTTP/1.1 {status}")), "{response}");
        if status == 503 {
            assert!(!response.to_ascii_lowercase().contains("www-authenticate:"));
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn configuration_commit_resizes_live_work_without_resetting_or_reviving_it() {
    let mut gateway = Gateway::new("blocking-config");
    let budget = Arc::new(Budget::new(Default::default()).unwrap());
    let executor = budget.executor();
    gateway.server = Arc::new(gatel_http::Server::with_blocking_executor(
        gateway.store.clone(),
        gateway.binds.clone(),
        executor.clone(),
    ));
    gateway.blocking_budget = Some(budget.clone());
    write_key(
        &gateway.directory.join("admin.hash"),
        &bcrypt::hash("fixture", 4).unwrap(),
    );
    let port = free_port();
    let config = |limits: &str, hash: &str| {
        format!(
            r#"
global {{ http "127.0.0.1:{port}"; {limits} }}
site "*" {{ route "/*" {{ basic-auth {{ user "admin" hash="{hash}"; }}; respond "ok"; }} }}
"#
        )
    };
    let initial = config("blocking jobs=2 memory=\"16KiB\";", "admin.hash");
    let started = gateway.apply(&initial).await;
    assert!(matches!(started, Outcome::Started { .. }), "{started:?}");
    assert_eq!(
        executor.snapshot().limits,
        Limits {
            jobs: 2,
            memory_bytes: 16384
        }
    );
    let mut old = tokio::net::TcpStream::connect(gateway.address()).await.unwrap();
    head(&mut old, 200).await;

    let worker = executor.clone();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let work = tokio::spawn(async move {
        worker
            .run(8192, move || {
                started.send(()).unwrap();
                released.recv().unwrap();
            })
            .await
            .unwrap();
    });
    ready.await.unwrap();
    let smaller = config("blocking jobs=1 memory=\"4KiB\";", "admin.hash");
    assert!(matches!(gateway.apply(&smaller).await, Outcome::Reloaded { .. }));
    assert_eq!(executor.snapshot().memory_bytes, 8192);
    assert_eq!(executor.snapshot().jobs, 1);
    head(&mut old, 503).await;
    let before = executor.snapshot();
    assert!(matches!(
        gateway.apply(&config("blocking jobs=0;", "admin.hash")).await,
        Outcome::Rejected(_)
    ));
    assert!(matches!(
        gateway
            .apply(&config("blocking jobs=20 memory=\"1GiB\";", "absent.hash"))
            .await,
        Outcome::PrepareFailed(_)
    ));
    assert_eq!(executor.snapshot(), before);
    assert_eq!(gateway.store.status().generation.number(), 2);
    assert!(matches!(gateway.apply(&smaller).await, Outcome::NoOp { .. }));
    release.send(()).unwrap();
    work.await.unwrap();
    head(&mut old, 503).await; // Credential itself exceeds the reduced limit.
    assert!(matches!(
        gateway.apply(&config("", "admin.hash")).await,
        Outcome::Reloaded { .. }
    ));
    assert_eq!(executor.snapshot().limits, Limits::default());
    head(&mut old, 200).await;
    budget.close();
    assert!(matches!(gateway.apply(&initial).await, Outcome::Reloaded { .. }));
    assert!(executor.snapshot().closed);
    head(&mut old, 503).await;
    drop(old);
    gateway.stop().await;
}

#[tokio::test]
async fn an_embedding_host_without_a_controller_rejects_the_knob() {
    let gateway = Gateway::new("blocking-unmanaged");
    let port = free_port();
    let outcome = gateway.apply(&format!(
        "global {{ http \"127.0.0.1:{port}\"; blocking jobs=2; }}\nsite \"*\" {{ route \"/\" {{ respond \"ok\" }} }}\n"
    )).await;
    assert!(
        matches!(outcome, Outcome::PrepareFailed(message) if message.contains("configurable blocking budget"))
    );
    assert!(gateway.store.current().is_none());
    gateway.stop().await;
}
