//! Source authority paired with the host's bounded preparation and activation.
use std::sync::{Arc, Weak};

use gatel_model::{Fingerprint, Node, SourceId, SourceSnapshot};
use gatel_plan::{Conflict, Outcome, SourceWriteError, Version};

use super::App;

#[path = "app_writer_text.rs"]
mod text;

/// Only trusted frontend code receives this handle. It cannot select a source,
/// revision, registry or preparation callback at submission time. Fingerprints
/// must still cover complete semantic input; this is not a remote credential.
pub struct AppSourceWriter {
    app: Weak<App>,
    source: SourceId,
    #[cfg(feature = "admin-api")]
    checkpoint: Option<Arc<std::sync::Mutex<crate::admin::CheckpointWriter>>>,
}

impl std::fmt::Debug for AppSourceWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppSourceWriter")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl App {
    /// Submit an already-observed trusted source. Unlike AppSourceWriter, this
    /// lower-level entry lets its caller choose identity and revision. The host
    /// must authorize both; composition and CAS run in bounded configuration work.
    #[allow(
        dead_code,
        reason = "Host integration point for a future control-plane adapter"
    )]
    pub async fn reload_source_if(
        self: &Arc<Self>,
        expected: Version,
        source: SourceSnapshot,
    ) -> Result<Outcome, Conflict> {
        self.reload_task(move |app| async move {
            app.configuration_work(move |app| {
                let (actual, mut snapshot) = app.store.observed_snapshot();
                if actual != expected {
                    return Err(Conflict { actual });
                }
                snapshot.insert(source);
                app.attempt_snapshot_if(&expected, &snapshot)
            })
            .await
            .unwrap_or_else(|| {
                let outcome = Outcome::Stale("configuration submissions are closed".into());
                outcome.record(&app.telemetry);
                Ok(outcome)
            })
        })
        .await
    }

    #[allow(
        dead_code,
        reason = "Trusted host issuance; network adapter is not yet exposed"
    )]
    pub fn source_writer(self: &Arc<Self>, source: SourceId) -> Option<AppSourceWriter> {
        if self.store.is_sealed() || !self.store.is_registered_source(&source) {
            return None;
        }
        Some(AppSourceWriter {
            app: Arc::downgrade(self),
            source,
            #[cfg(feature = "admin-api")]
            checkpoint: None,
        })
    }

    /// Bind both source authority and checkpoint policy at handle issuance.
    /// Host must recover this writer first and use the same policy for all
    /// participating sources. This does not install a global persistence mode.
    #[cfg(feature = "admin-api")]
    #[allow(dead_code, reason = "Source-scoped checkpoint integration entry")]
    pub(crate) fn checkpoint_source_writer(
        self: &Arc<Self>,
        source: SourceId,
        checkpoint: Arc<std::sync::Mutex<crate::admin::CheckpointWriter>>,
    ) -> Option<AppSourceWriter> {
        let mut writer = self.source_writer(source)?;
        writer.checkpoint = Some(checkpoint);
        Some(writer)
    }
}

impl AppSourceWriter {
    #[allow(dead_code, reason = "Trusted frontend integration point")]
    pub fn source(&self) -> &SourceId {
        &self.source
    }

    /// Accepted work owns its lifetime through activation even if the awaiting
    /// caller disappears. Admission overload/closure may return Stale; a missing
    /// or already-sealed owner returns Unavailable. Ordinary handles are memory
    /// only; checkpoint-bound handles finish saving before returning success.
    #[allow(dead_code, reason = "Trusted frontend integration point")]
    pub async fn reload_if(
        &self,
        expected: Version,
        content: Fingerprint,
        nodes: Vec<Node>,
    ) -> Result<Outcome, SourceWriteError> {
        self.reload_input_if(expected, move |_| Ok((content, nodes)))
            .await
    }

    /// Inline KDL only: host-computed identity, stable source origins, no imports.
    /// Parsing runs after admission and version validation in the blocking worker.
    /// Network adapters must bound incoming bytes before allocating this String.
    #[allow(dead_code, reason = "Bounded text entry for a future control-plane adapter")]
    pub async fn reload_kdl_if(&self, expected: Version, input: String) -> Result<Outcome, SourceWriteError> {
        self.reload_input_if(expected, move |source| text::lower(&input, source))
            .await
    }

    async fn reload_input_if(
        &self,
        expected: Version,
        input: impl FnOnce(&SourceId) -> Result<(Fingerprint, Vec<Node>), gatel_model::Diagnostics>
        + Send
        + 'static,
    ) -> Result<Outcome, SourceWriteError> {
        let app = self.app.upgrade().ok_or(SourceWriteError::Unavailable)?;
        if app.store.is_sealed() {
            return Err(SourceWriteError::Unavailable);
        }
        let source = self.source.clone();
        #[cfg(feature = "admin-api")]
        let checkpoint = self.checkpoint.clone();
        #[cfg(feature = "admin-api")]
        let base = expected.clone();
        let work = move |app: Arc<App>| async move {
            app.configuration_work(move |app| {
                let (actual, mut intent) = app.store.observed_snapshot();
                if actual != expected {
                    return Err(SourceWriteError::Conflict(Conflict { actual }));
                }
                #[cfg(feature = "admin-api")]
                if let Some(writer) = checkpoint {
                    writer
                        .lock()
                        .map_err(|_| SourceWriteError::PersistenceUnknown)?
                        .bind(actual.boot_id)
                        .map_err(|_| SourceWriteError::PersistenceUnknown)?;
                }
                let (content, nodes) = match input(&source) {
                    Ok(input) => input,
                    Err(diagnostics) => {
                        let outcome = app
                            .store
                            .reject_observation_if(&expected, diagnostics)
                            .map_err(SourceWriteError::Conflict)?;
                        outcome.record(&app.telemetry);
                        return Ok(outcome);
                    }
                };
                let observed = SourceSnapshot::observe(source.clone(), content, nodes, intent.get(&source))
                    .map_err(SourceWriteError::Observation)?;
                intent.insert(observed);
                app.attempt_snapshot_if(&expected, &intent)
                    .map_err(SourceWriteError::Conflict)
            })
            .await
            .unwrap_or_else(|| {
                let outcome = Outcome::Stale("configuration submissions are closed".into());
                outcome.record(&app.telemetry);
                Ok(outcome)
            })
        };
        #[cfg(feature = "admin-api")]
        if let Some(writer) = self.checkpoint.clone() {
            return app
                .reload_task_persisting(work, move |app, result| {
                    app.finish_checkpoint(&base, &writer, result)
                })
                .await;
        }
        app.reload_task(work).await
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{Hasher, ObservationError, SourceRev};

    use super::*;

    fn input(text: &str) -> (Fingerprint, Vec<Node>) {
        let mut hash = Hasher::new();
        hash.update_str(text);
        (
            hash.finish(),
            gatel_kdl::lower(text, "bound-host-writer").unwrap(),
        )
    }

    #[tokio::test]
    async fn writer_owns_activation_preserves_intent_and_rejects_identity_reuse() {
        let app = Arc::new(
            App::with_sources(std::env::temp_dir(), [SourceId::file("main"), SourceId::admin()]).unwrap(),
        );
        let writer = app.source_writer(SourceId::admin()).unwrap();
        assert_eq!(writer.source(), &SourceId::admin());
        assert!(app.source_writer(SourceId::file("unregistered")).is_none());
        assert!(app.store.observed_snapshot().1.sources().next().is_none());
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        app.binds.adopt(socket).unwrap();
        let (file_content, file_nodes) = input(&format!(
            "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"writer-live\"; }} }}"
        ));
        let file = SourceSnapshot::observe(SourceId::file("main"), file_content, file_nodes, None).unwrap();
        assert!(matches!(
            app.reload_source_if(app.store.status().version(), file).await,
            Ok(Outcome::Started { .. })
        ));
        let initial = app.store.status().version();
        let (content, nodes) = input("upstream \"runtime/shared\" { endpoint \"127.0.0.1:1\"; }");
        let outcome = writer
            .reload_if(initial.clone(), content, nodes.clone())
            .await
            .unwrap();
        assert!(matches!(outcome, Outcome::Reloaded { .. }), "{outcome:?}");
        let current = app.store.current().unwrap();
        assert_eq!(
            app.activated.load(std::sync::atomic::Ordering::Acquire),
            current.number.number()
        );
        let version = app.store.status().version();
        let (_, changed) = input("upstream \"runtime/shared\" { endpoint \"127.0.0.1:2\"; }");
        assert!(matches!(
            writer.reload_if(version.clone(), content, changed).await,
            Err(SourceWriteError::Observation(ObservationError::ContentMismatch))
        ));
        assert_eq!(app.store.status().version(), version);
        assert!(Arc::ptr_eq(&current, &app.store.current().unwrap()));
        assert!(matches!(
            writer.reload_if(initial, content, nodes.clone()).await,
            Err(SourceWriteError::Conflict(_))
        ));
        assert!(matches!(
            writer.reload_if(version.clone(), content, nodes).await,
            Ok(Outcome::NoOp { .. })
        ));
        assert_eq!(
            app.store
                .observed_snapshot()
                .1
                .get(&SourceId::admin())
                .unwrap()
                .rev,
            SourceRev(1)
        );
        let (empty, nodes) = input("");
        assert!(matches!(
            writer.reload_if(version, empty, nodes).await,
            Ok(Outcome::Reloaded { .. })
        ));
        assert!(app.store.current().unwrap().plan.upstreams.is_empty());
        assert_eq!(
            app.store
                .observed_snapshot()
                .1
                .get(&SourceId::file("main"))
                .unwrap()
                .rev,
            SourceRev(1)
        );
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.read_to_string(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(response.ends_with("writer-live"));
        let inline = "upstream \"runtime/inline\" { endpoint \"127.0.0.1:2\"; }";
        assert!(matches!(
            writer
                .reload_kdl_if(app.store.status().version(), inline.into())
                .await,
            Ok(Outcome::Reloaded { .. })
        ));
        let version = app.store.status().version();
        let committed = app.store.current().unwrap();
        for bad in [
            "site {".to_string(),
            "import \"never-read.kdl\";".to_string(),
            " ".repeat(1024 * 1024 + 1),
        ] {
            assert!(matches!(
                writer.reload_kdl_if(version.clone(), bad).await,
                Ok(Outcome::Rejected(_))
            ));
            assert_eq!(app.store.status().version(), version);
            assert!(Arc::ptr_eq(&committed, &app.store.current().unwrap()));
            assert_eq!(app.store.status().last_failure.unwrap().stage, "observe");
            assert!(matches!(
                writer.reload_kdl_if(version.clone(), inline.into()).await,
                Ok(Outcome::NoOp { .. })
            ));
            assert!(app.store.status().last_failure.is_none());
        }
        let mut stale = version.clone();
        stale.boot_id = gatel_model::BootId::generate();
        assert!(matches!(
            writer.reload_kdl_if(stale, "site {".into()).await,
            Err(SourceWriteError::Conflict(_))
        ));
        assert!(app.store.status().last_failure.is_none());
        app.shutdown().await;
        assert!(app.source_writer(SourceId::admin()).is_none());
        assert!(matches!(
            writer
                .reload_if(app.store.status().version(), empty, vec![])
                .await,
            Err(SourceWriteError::Unavailable)
        ));
    }

    #[tokio::test]
    async fn handle_does_not_keep_its_owner_alive() {
        let app = Arc::new(App::with_sources(std::env::temp_dir(), [SourceId::admin()]).unwrap());
        let writer = app.source_writer(SourceId::admin()).unwrap();
        let version = app.store.status().version();
        let weak = Arc::downgrade(&app);
        drop(app);
        assert!(weak.upgrade().is_none());
        assert!(matches!(
            writer.reload_if(version, Hasher::new().finish(), vec![]).await,
            Err(SourceWriteError::Unavailable)
        ));
    }

    #[tokio::test]
    async fn cancelled_waiters_keep_bounded_admission_until_work_finishes() {
        use std::future::Future;
        use std::task::{Context, Waker};
        let app = Arc::new(App::with_sources(std::env::temp_dir(), [SourceId::admin()]).unwrap());
        let writer = app.source_writer(SourceId::admin()).unwrap();
        let version = app.store.status().version();
        let execution = app.configuration_slot.clone().acquire_owned().await.unwrap();
        let (content, nodes) = input("not-a-supported-root-node;");
        for _ in 0..super::super::MAX_RELOAD_TASKS {
            let mut caller = Box::pin(writer.reload_if(version.clone(), content, nodes.clone()));
            assert!(
                caller
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            drop(caller);
        }
        assert_eq!(app.reload_slots.available_permits(), 0);
        assert!(matches!(writer.reload_if(version.clone(), content, nodes).await,
            Ok(Outcome::Stale(reason)) if reason.contains("capacity exhausted")));
        assert_eq!(app.store.status().version(), version);
        drop(writer);
        drop(execution);
        tokio::time::timeout(std::time::Duration::from_secs(5), app.submissions.drained())
            .await
            .unwrap();
        assert_eq!(
            app.reload_slots.available_permits(),
            super::super::MAX_RELOAD_TASKS
        );
        let (actual, intent) = app.store.observed_snapshot();
        assert_ne!(actual, version);
        assert_eq!(intent.get(&SourceId::admin()).unwrap().rev, SourceRev(1));
        assert!(app.store.status().last_failure.is_some());
        assert!(app.previous.lock().unwrap().is_none());
        app.shutdown().await;
    }
}
