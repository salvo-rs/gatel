use http_body_util::{BodyExt, Empty};

use super::*;
use crate::admin::AdminApi;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

async fn status(api: &AdminApi) -> serde_json::Value {
    let response = api
        .handle(
            http::Request::builder()
                .uri("/v1/config/status")
                .header(http::header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Empty::<bytes::Bytes>::new())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), http::StatusCode::OK);
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn status_reports_publication_before_activation_without_waiting_for_reload() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let text = format!(
            "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"first\"; }} }}"
        );
        let snapshot = Snapshot::new()
            .with(
                SourceSnapshot::observe(
                    SourceId::file("main"),
                    gatel_model::Hasher::new().finish(),
                    gatel_kdl::lower(&text, "fixture").unwrap(),
                    None,
                )
                .unwrap(),
            )
            .with(
                SourceSnapshot::observe(
                    SourceId::admin(),
                    gatel_model::Hasher::new().finish(),
                    vec![],
                    None,
                )
                .unwrap(),
            );
        let app = Arc::new(App::from_snapshot(std::env::temp_dir(), snapshot).unwrap());
        app.binds.adopt(socket).unwrap();
        let api = AdminApi::new(&app, zeroize::Zeroizing::new(TOKEN.into())).unwrap();
        let activation = app.lifecycle.lock().await;
        let owner = app.clone();
        let first = tokio::spawn(async move { owner.reload().await });
        while app.store.current().is_none() {
            tokio::task::yield_now().await;
        }
        let pending = status(&api).await;
        assert_eq!(pending["generation"], "1");
        assert_eq!(pending["last_activated_generation"], "0");
        assert_eq!(pending["published"], true);
        assert_eq!(pending["health"]["ready"], false);
        assert!(!first.is_finished());
        drop(activation);
        assert!(matches!(first.await.unwrap(), Outcome::Started { .. }));
        assert_eq!(status(&api).await["last_activated_generation"], "1");

        // During a later reload the old runtime remains ready, but the two
        // counters reveal that the newly published configuration is not active.
        let activation = app.lifecycle.lock().await;
        let writer = app.source_writer(SourceId::admin()).unwrap();
        let expected = app.configuration_status().version();
        let next = tokio::spawn(async move {
            writer
                .reload_kdl_if(
                    expected,
                    "upstream \"runtime/new\" { endpoint \"127.0.0.1:2\"; }".into(),
                )
                .await
        });
        while app.store.status().generation.number() != 2 {
            tokio::task::yield_now().await;
        }
        let pending = status(&api).await;
        assert_eq!(pending["generation"], "2");
        assert_eq!(pending["last_activated_generation"], "1");
        assert_eq!(pending["health"]["ready"], true);
        assert!(!next.is_finished());
        drop(activation);
        assert!(matches!(next.await.unwrap(), Ok(Outcome::Reloaded { .. })));
        let active = status(&api).await;
        assert_eq!(active["generation"], "2");
        assert_eq!(active["last_activated_generation"], "2");
        app.shutdown().await;
    })
    .await
    .unwrap();
}
