//! Budget scrape data comes from the injected executor, not a global guess.
mod support;
use std::sync::Arc;

use gatel_plan::Outcome;
use gatel_plan::blocking::Budget;
use support::*;

async fn login(gateway: &Gateway, password: &str, status: u16) {
    let response = request(gateway.address(), &format!(
        "GET / HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic {password}\r\nConnection: close\r\n\r\n"
    )).await;
    assert!(response.starts_with(&format!("HTTP/1.1 {status}")), "{response}");
}
async fn scrape(gateway: &Gateway) -> String {
    let response = get(gateway.address(), "/metrics").await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    response
}

#[tokio::test]
async fn metrics_explain_saturation_and_preserve_counts_across_reload() {
    let mut gateway = Gateway::new("blocking-metrics");
    let budget = Arc::new(Budget::new(Default::default()).unwrap());
    let executor = budget.executor();
    gateway.server = Arc::new(gatel_http::Server::with_blocking_executor(
        gateway.store.clone(),
        gateway.binds.clone(),
        executor.clone(),
    ));
    gateway.blocking_budget = Some(budget.clone());
    let hash = bcrypt::hash("fixture", 4).unwrap();
    write_key(&gateway.directory.join("admin.hash"), &hash);
    let port = free_port();
    let config = |jobs| {
        format!(
            r#"
global {{ http "127.0.0.1:{port}"; blocking jobs={jobs} memory="8KiB"; }}
site "*" {{
 route "/metrics" {{ metrics; }}
 route "/" {{ basic-auth {{ user "admin" hash="admin.hash"; }}; respond "ok"; }}
}}
"#
        )
    };
    assert!(matches!(gateway.apply(&config(2)).await, Outcome::Started { .. }));
    let worker = executor.clone();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let work = tokio::spawn(async move {
        worker
            .run(4096, move || {
                started.send(()).unwrap();
                released.recv().unwrap();
            })
            .await
            .unwrap();
    });
    ready.await.unwrap();
    login(&gateway, "YWRtaW46Zml4dHVyZQ==", 503).await;
    let first = scrape(&gateway).await;
    for metric in [
        "gatel_blocking_jobs 1\n",
        "gatel_blocking_running_jobs 1\n",
        "gatel_blocking_memory_bytes 4096\n",
        "gatel_blocking_rejections_total{reason=\"memory\"} 1\n",
    ] {
        assert!(first.contains(metric), "{metric}: {first}");
    }
    assert!(matches!(
        gateway.apply(&config(1)).await,
        Outcome::Reloaded { .. }
    ));
    login(&gateway, "YWRtaW46Zml4dHVyZQ==", 503).await;
    let second = scrape(&gateway).await;
    assert!(second.contains("gatel_blocking_jobs_limit 1\n"));
    assert!(second.contains("gatel_blocking_rejections_total{reason=\"jobs\"} 1\n"));
    assert!(second.contains("gatel_blocking_rejections_total{reason=\"memory\"} 1\n"));
    release.send(()).unwrap();
    work.await.unwrap();
    login(&gateway, "YWRtaW46d3Jvbmc=", 401).await;
    login(&gateway, "YWRtaW46Zml4dHVyZQ==", 200).await;
    budget.close();
    login(&gateway, "YWRtaW46Zml4dHVyZQ==", 503).await;
    let final_scrape = scrape(&gateway).await;
    for metric in [
        "gatel_blocking_jobs 0\n",
        "gatel_blocking_running_jobs 0\n",
        "gatel_blocking_memory_bytes 0\n",
        "gatel_blocking_closed 1\n",
        "gatel_blocking_admissions_total 3\n",
        "gatel_blocking_completions_total{outcome=\"completed\"} 3\n",
        "gatel_blocking_rejections_total{reason=\"closed\"} 1\n",
    ] {
        assert!(final_scrape.contains(metric), "{metric}: {final_scrape}");
    }
    assert!(!final_scrape.contains(&hash));
    assert!(!final_scrape.contains("admin"));
    assert!(!final_scrape.contains("fixture"));
    gateway.stop().await;
}
