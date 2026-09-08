use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

#[tokio::test]
async fn exhausted_probe_budget_never_connects_to_the_next_address() {
    use tokio::io::AsyncReadExt;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let first = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let second = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        second.set_nonblocking(true).unwrap();
        let addresses = [first.local_addr().unwrap(), second.local_addr().unwrap()];
        let peer = tokio::spawn(async move {
            let (mut socket, _) = first.accept().await.unwrap();
            let mut bytes = Vec::new();
            socket.read_to_end(&mut bytes).await.unwrap();
            assert!(!bytes.is_empty());
        });
        let result = probe_any(
            &addresses,
            &gatel_plan::HealthKind::Http {
                path: "/".into(),
                status: 200,
                host: None,
            },
            gatel_plan::UpstreamProtocol::Http1,
            std::time::Duration::from_millis(200),
            None,
        )
        .await;
        assert!(result.unwrap_err().contains("budget exhausted"));
        assert!(matches!(second.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock));
        peer.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_fast_failed_address_still_allows_a_healthy_fallback() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut addresses = Vec::new();
        let mut peers = Vec::new();
        for status in [503, 200] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            addresses.push(listener.local_addr().unwrap());
            peers.push(tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    head.push(socket.read_u8().await.unwrap());
                }
                socket
                    .write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: 0\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
            }));
        }
        assert!(
            probe_any(
                &addresses,
                &gatel_plan::HealthKind::Http {
                    path: "/".into(),
                    status: 200,
                    host: None
                },
                gatel_plan::UpstreamProtocol::Http1,
                std::time::Duration::from_secs(2),
                None,
            )
            .await
            .is_ok()
        );
        for peer in peers {
            peer.await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn zero_probe_budget_does_not_start_transport() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    assert!(
        probe_any(
            &[listener.local_addr().unwrap()],
            &gatel_plan::HealthKind::TcpConnect,
            gatel_plan::UpstreamProtocol::Http1,
            std::time::Duration::ZERO,
            None,
        )
        .await
        .unwrap_err()
        .contains("budget exhausted")
    );
    assert!(matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock));
}

fn runtimes() -> Arc<UpstreamRuntimes> {
    let plan = super::tests::plan(&super::tests::config(
        "127.0.0.1:1".parse().unwrap(),
        r#""tcp" interval="60s" timeout="1s""#,
    ));
    Arc::new(
        UpstreamRuntimes::resolve(
            &plan,
            &crate::registry::StateRegistry::new(),
            std::path::Path::new("."),
        )
        .unwrap()
        .0,
    )
}

#[tokio::test]
async fn completed_workers_are_not_reported_live_and_reconcile_replaces_them() {
    let supervisor = Supervisor::new(Telemetry::new());
    let runtimes = runtimes();
    supervisor.reconcile(&runtimes).await;
    let previous = {
        let mut workers = supervisor.workers.lock().await;
        let worker = workers.values_mut().next().unwrap();
        let id = worker.handle.id();
        worker.handle.abort();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !worker.handle.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        id
    };
    assert!(supervisor.running().await.is_empty());
    supervisor.reconcile(&runtimes).await;
    assert_eq!(supervisor.running().await.len(), 1);
    assert_ne!(
        supervisor
            .workers
            .lock()
            .await
            .values()
            .next()
            .unwrap()
            .handle
            .id(),
        previous
    );
    supervisor.shutdown().await;
}

#[tokio::test]
async fn shutdown_is_terminal_even_when_reconcile_races_or_runs_later() {
    for shutdown_first in [false, true] {
        let supervisor = Supervisor::new(Telemetry::new());
        let runtimes = runtimes();
        if shutdown_first {
            tokio::join!(supervisor.shutdown(), supervisor.reconcile(&runtimes));
        } else {
            tokio::join!(supervisor.reconcile(&runtimes), supervisor.shutdown());
        }
        supervisor.reconcile(&runtimes).await;
        supervisor.shutdown().await;
        assert!(supervisor.running().await.is_empty());
        assert!(supervisor.resolving().await.is_empty());
        assert!(!supervisor.census.is_running().await);
        assert!(*supervisor.closed.lock().await);
    }
}

#[tokio::test]
async fn dropping_worker_ownership_aborts_the_task() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let guard = Dropped(dropped.clone());
    let (started, ready) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        let _guard = guard;
        started.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    ready.await.unwrap();
    let (stop, _receiver) = watch::channel(false);
    let abort = handle.abort_handle();
    drop(Worker {
        identity: Hasher::new().finish(),
        stop,
        handle,
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !abort.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn shutdown_interrupts_inflight_probe_without_reporting_failure() {
    use tokio::io::AsyncReadExt;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (started, ready) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 4096];
            assert!(socket.read(&mut buffer).await.unwrap() > 0);
            started.send(()).unwrap();
            let mut rest = Vec::new();
            let _ = socket.read_to_end(&mut rest).await;
        });
        let plan = super::tests::plan(&super::tests::config(
            address,
            r#""http" path="/" interval="60s" timeout="30s""#,
        ));
        let state = crate::registry::StateRegistry::new();
        let (runtimes, _) = UpstreamRuntimes::resolve(&plan, &state, std::path::Path::new(".")).unwrap();
        let endpoints = runtimes.iter().next().unwrap().endpoints.clone();
        let supervisor = Supervisor::new(Telemetry::new());
        supervisor.reconcile(&Arc::new(runtimes)).await;
        ready.await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), supervisor.shutdown())
            .await
            .expect("shutdown waited for the 30-second probe timeout");
        peer.await.unwrap();
        assert_eq!(endpoints.all()[0].health(), crate::endpoint::Health::Unknown);
        assert!(supervisor.running().await.is_empty());
    })
    .await
    .unwrap();
}

struct Dropped(Arc<AtomicUsize>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn cancelling_a_round_drops_all_children_before_parent_join_returns() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let (started, mut ready) = tokio::sync::mpsc::channel(2);
    let futures = (0..2)
        .map(|_| {
            let guard = Dropped(dropped.clone());
            let started = started.clone();
            async move {
                let _guard = guard;
                started.send(()).await.unwrap();
                std::future::pending::<()>().await;
            }
        })
        .collect();
    let parent = tokio::spawn(join_all(futures));
    ready.recv().await.unwrap();
    ready.recv().await.unwrap();
    parent.abort();
    assert!(parent.await.unwrap_err().is_cancelled());
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn concurrent_rounds_preserve_input_order_without_detached_tasks() {
    let (release, held) = tokio::sync::oneshot::channel();
    let futures: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = u8> + Send>>> = vec![
        Box::pin(async move {
            held.await.unwrap();
            1
        }),
        Box::pin(async move {
            release.send(()).unwrap();
            2
        }),
    ];
    assert_eq!(join_all(futures).await, [1, 2]);
}
