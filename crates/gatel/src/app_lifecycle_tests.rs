use std::future::Future;
use std::task::Poll;
use std::time::Duration;

use super::*;

struct Fixture {
    app: Arc<App>,
    directory: PathBuf,
    address: std::net::SocketAddr,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("gatel-app-lifecycle-{}-{sequence}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let app = Arc::new(App::new(directory.join("gatel.kdl")).unwrap());
        let address = app.binds.adopt(listener).unwrap();
        let fixture = Self {
            app,
            directory,
            address,
        };
        fixture.commit("old");
        fixture
    }

    fn commit(&self, name: &str) {
        std::fs::write(
            &self.app.root,
            format!(
                r#"
global {{ http "{}" }}
upstream "{name}" {{
    endpoint "127.0.0.1:1"
    health "tcp" interval="60s" timeout="1s"
}}
site "*" {{ route "/*" {{ proxy upstream="{name}" }} }}
"#,
                self.address
            ),
        )
        .unwrap();
        let outcome = self.app.attempt();
        assert!(!outcome.is_failure(), "{outcome:?}");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for name in ["ca.pem", "client.pem", "client.key", "ops.hash"] {
            let _ = std::fs::remove_file(self.directory.join(name));
        }
        let _ = std::fs::remove_file(&self.app.root);
        for name in ["acme-a", "acme-b"] {
            let namespaces = self.directory.join(name).join("accounts-v1");
            if let Ok(entries) = std::fs::read_dir(&namespaces) {
                for entry in entries.flatten() {
                    let _ = std::fs::remove_file(entry.path().join("account.lock"));
                    let _ = std::fs::remove_file(entry.path().join("registration-intent.key"));
                    let _ = std::fs::remove_dir(entry.path());
                }
            }
            let _ = std::fs::remove_dir(namespaces);
            let _ = std::fs::remove_dir(self.directory.join(name));
        }
        let _ = std::fs::remove_dir(&self.directory);
    }
}

#[tokio::test]
async fn failed_account_candidate_does_not_replace_active_shutdown_ownership() {
    let fixture = Fixture::new();
    let app = &fixture.app;
    let config = |directory: &str, fail: bool| {
        let extra = if fail {
            "{ client-auth ca=\"absent.pem\"; }"
        } else {
            ""
        };
        format!(
            r#"
global {{ https "{}"; acme email="ops@example.com" agreed=#true ca="http://127.0.0.1:1/directory" directory="{directory}"; }}
site "example.com" {{ tls acme {extra}
route "/" {{ respond "ok"; }} }}
"#,
            fixture.address
        )
    };
    std::fs::write(&app.root, config("acme-a", false)).unwrap();
    assert!(matches!(app.attempt(), Outcome::Reloaded { .. }));
    tokio::time::timeout(Duration::from_secs(1), app.activate())
        .await
        .unwrap();
    let committed = app.store.current().unwrap();
    let active = committed.runtime.acme.as_ref().unwrap().0.clone();
    assert!(active.is_running().await);
    std::fs::write(&app.root, config("acme-b", true)).unwrap();
    assert!(matches!(app.attempt(), Outcome::PrepareFailed(_)));
    let cached = app.acme.lock().unwrap().as_ref().unwrap().1.clone();
    assert!(!Arc::ptr_eq(&active, &cached));
    let reused = app.acme_factory()(committed.plan.spec.global.acme.as_ref().unwrap()).unwrap();
    assert!(Arc::ptr_eq(&active, &reused));
    tokio::time::timeout(Duration::from_secs(1), app.shutdown())
        .await
        .unwrap();
    assert!(!active.is_running().await);
    assert!(!cached.is_running().await);
}

#[tokio::test]
async fn watcher_reload_refreshes_tls_files_without_changing_configuration_revisions() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for rotate_client in [false, true] {
            let fixture = Fixture::new();
            let app = &fixture.app;
            let first = rcgen::generate_simple_self_signed(vec!["api.internal".into()]).unwrap();
            let second = rcgen::generate_simple_self_signed(vec!["api.internal".into()]).unwrap();
            let ca = fixture.directory.join("ca.pem");
            let cert = fixture.directory.join("client.pem");
            let key = fixture.directory.join("client.key");
            std::fs::write(&ca, first.cert.pem()).unwrap();
            std::fs::write(&cert, first.cert.pem()).unwrap();
            std::fs::write(&key, first.signing_key.serialize_pem()).unwrap();
            let client = if rotate_client {
                r#"client-cert="file:client.pem" client-key="file:client.key""#
            } else {
                ""
            };
            std::fs::write(
                &app.root,
                format!(
                    r#"
global {{ http "{}" }}
upstream "api" {{
    endpoint "127.0.0.1:1"
    tls server-name="api.internal" ca="file:ca.pem" {client}
}}
site "*" {{ route "/*" {{ proxy upstream="api" }} }}
"#,
                    fixture.address
                ),
            )
            .unwrap();
            assert!(matches!(
                crate::watch::reload_once(app).await,
                Outcome::Reloaded { .. }
            ));
            let before = app.store.current().unwrap();
            let revs = app.store.status().revs;
            assert!(matches!(
                crate::watch::reload_once(app).await,
                Outcome::NoOp { .. }
            ));
            if rotate_client {
                std::fs::write(&cert, second.cert.pem()).unwrap();
                // Interrupted pair replacement must fail, not publish a
                // certificate with the previous key or advance the generation.
                assert!(matches!(
                    crate::watch::reload_once(app).await,
                    Outcome::PrepareFailed(_)
                ));
                assert!(Arc::ptr_eq(&before, &app.store.current().unwrap()));
                std::fs::write(&key, second.signing_key.serialize_pem()).unwrap();
            } else {
                std::fs::write(&ca, second.cert.pem()).unwrap();
            }
            assert!(matches!(
                crate::watch::reload_once(app).await,
                Outcome::Reloaded { .. }
            ));
            let after = app.store.current().unwrap();
            assert_eq!(app.store.status().revs, revs);
            assert_eq!(before.plan_hash(), after.plan_hash());
            assert!(!Arc::ptr_eq(
                &before.runtime.upstreams.iter().next().unwrap().pool,
                &after.runtime.upstreams.iter().next().unwrap().pool
            ));
            assert!(matches!(
                crate::watch::reload_once(app).await,
                Outcome::NoOp { .. }
            ));
            std::fs::write(&ca, "invalid certificate").unwrap();
            assert!(matches!(
                crate::watch::reload_once(app).await,
                Outcome::PrepareFailed(_)
            ));
            std::fs::remove_file(&ca).unwrap();
            assert!(matches!(
                crate::watch::reload_once(app).await,
                Outcome::PrepareFailed(_)
            ));
            assert!(Arc::ptr_eq(&after, &app.store.current().unwrap()));
            app.shutdown().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn queued_activation_reads_the_generation_after_acquiring_lifecycle_ownership() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let fixture = Fixture::new();
        let app = &fixture.app;
        let gate = app.lifecycle.lock().await;
        let mut activation = Box::pin(app.activate());
        std::future::poll_fn(|cx| {
            assert!(activation.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        fixture.commit("new");
        drop(gate);
        activation.await;
        let running = app.supervisor.running().await;
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].name(), "new");
        app.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn declared_secret_reads_rotate_basic_auth_without_editing_configuration() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn accepted(address: std::net::SocketAddr, token: &str) -> bool {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream.write_all(format!("GET / HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic {token}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(
            response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.1 401"),
            "{response}"
        );
        response.starts_with("HTTP/1.1 200")
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        let fixture = Fixture::new();
        let app = &fixture.app;
        let path = fixture.directory.join("ops.hash");
        std::fs::write(&path, bcrypt::hash("old", 4).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::fs::write(
            &app.root,
            format!(
                r#"
global {{ http "{}" }}
site "*" {{ route "/*" {{
    basic-auth {{ user "ops" hash="file:ops.hash" }}
    respond "authenticated"
}} }}
"#,
                fixture.address
            ),
        )
        .unwrap();
        assert!(matches!(
            crate::watch::reload_once(app).await,
            Outcome::Reloaded { .. }
        ));
        let revs = app.store.status().revs;
        assert!(accepted(fixture.address, "b3BzOm9sZA==").await);
        assert!(
            app.store
                .current()
                .unwrap()
                .runtime
                .secrets
                .read(
                    &gatel_model::SecretRef::parse("ops.hash").unwrap(),
                    &fixture.directory,
                )
                .unwrap_err()
                .contains("sealed")
        );
        assert!(!accepted(fixture.address, "b3BzOm5ldw==").await);
        std::fs::write(&path, bcrypt::hash("new", 4).unwrap()).unwrap();
        assert!(matches!(
            crate::watch::reload_once(app).await,
            Outcome::Reloaded { .. }
        ));
        assert_eq!(app.store.status().revs, revs);
        assert!(!accepted(fixture.address, "b3BzOm9sZA==").await);
        assert!(accepted(fixture.address, "b3BzOm5ldw==").await);
        assert!(matches!(
            crate::watch::reload_once(app).await,
            Outcome::NoOp { .. }
        ));
        let current = app.store.current().unwrap();
        std::fs::write(&path, "INVALID-SECRET-SENTINEL").unwrap();
        let failure = crate::watch::reload_once(app).await;
        assert!(matches!(failure, Outcome::PrepareFailed(_)));
        assert!(!format!("{failure:?}").contains("INVALID-SECRET-SENTINEL"));
        assert!(Arc::ptr_eq(&current, &app.store.current().unwrap()));
        assert!(accepted(fixture.address, "b3BzOm5ldw==").await);
        app.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn activation_and_shutdown_are_serialized_and_shutdown_is_terminal() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for shutdown_first in [false, true] {
            let fixture = Fixture::new();
            let app = &fixture.app;
            if shutdown_first {
                tokio::join!(app.shutdown(), app.activate());
            } else {
                tokio::join!(app.activate(), app.shutdown());
            }
            app.activate().await;
            app.shutdown().await;
            assert!(app.lifecycle.lock().await.closed);
            assert!(app.server.active_addresses().await.is_empty());
            assert!(app.supervisor.running().await.is_empty());
            assert!(app.supervisor.resolving().await.is_empty());
        }
    })
    .await
    .unwrap();
}
