use std::path::PathBuf;
use std::time::Duration;

use gatel_model::{BootId, Hasher, SourceId, SourceRev, SourceSnapshot};

use super::*;

struct Fixture {
    app: Arc<App>,
    writer: Arc<Mutex<CheckpointWriter>>,
    snapshot: Snapshot,
    // Drop the app and writer handles before deleting their files on Windows.
    directory: Directory,
}

struct Directory(PathBuf);
impl std::ops::Deref for Directory {
    type Target = std::path::Path;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Fixture {
    fn new() -> Self {
        Self::with_sources([SourceId::file("main")])
    }

    fn with_sources(sources: impl IntoIterator<Item = SourceId>) -> Self {
        let directory = std::env::temp_dir().join(format!("gatel-checkpoint-submit-{}", BootId::generate()));
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        builder.create(&directory).unwrap();
        let app = Arc::new(App::with_sources(std::env::temp_dir(), sources).unwrap());
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let text = format!(
            "global {{ http \"{}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"checkpoint\"; }} }}",
            socket.local_addr().unwrap()
        );
        app.binds.adopt(socket).unwrap();
        let (writer, previous) = CheckpointWriter::open(&directory, "fixture", &app.registry).unwrap();
        assert!(previous.is_none());
        Self {
            app,
            writer: Arc::new(Mutex::new(writer)),
            directory: Directory(directory),
            snapshot: snapshot(&text),
        }
    }
}

#[path = "app_checkpoint_source_tests.rs"]
mod source_tests;

#[path = "app_checkpoint_frontend_tests.rs"]
mod frontend_tests;

#[path = "app_checkpoint_http_tests.rs"]
mod http_tests;

#[path = "app_checkpoint_session_tests.rs"]
mod session_tests;

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn snapshot(text: &str) -> Snapshot {
    let mut hash = Hasher::new();
    hash.update_str(text);
    Snapshot::new().with(SourceSnapshot::new(
        SourceId::file("main"),
        SourceRev(1),
        hash.finish(),
        gatel_kdl::lower(text, "fixture").unwrap(),
    ))
}

async fn assert_serving(app: &App) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addresses = app.server.active_addresses().await;
    assert_eq!(addresses.len(), 1);
    let mut client = tokio::net::TcpStream::connect(addresses[0]).await.unwrap();
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    client
        .take(16 * 1024)
        .read_to_string(&mut response)
        .await
        .unwrap();
    assert!(
        response.starts_with("HTTP/1.1 200") && response.ends_with("checkpoint"),
        "{response}"
    );
}

#[tokio::test]
async fn filesystem_save_failure_keeps_live_publication_but_blocks_further_writes() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let fixture = Fixture::new();
        let initial = fixture.app.store.status().version();
        let log = std::fs::read(fixture.directory.join("intent.log")).unwrap();
        // A non-regular checkpoint target is a portable real filesystem error,
        // without changing operator permissions or injecting a fake save result.
        std::fs::create_dir(fixture.directory.join("checkpoint.bin")).unwrap();
        let outcome = fixture
            .app
            .reload_checkpoint_if(initial.clone(), fixture.snapshot.clone(), fixture.writer.clone())
            .await;
        assert!(matches!(outcome, Err(SourceWriteError::PersistenceUnknown)));
        let published = fixture.app.store.status().version();
        assert_ne!(published, initial);
        assert_eq!(
            fixture.app.activated.load(Ordering::Acquire),
            published.generation.number()
        );
        assert_serving(&fixture.app).await;
        assert!(fixture.writer.lock().unwrap().ensure_ready().is_err());
        // Removing the obstruction does not authorize automatic continuation.
        std::fs::remove_dir(fixture.directory.join("checkpoint.bin")).unwrap();
        let result = fixture
            .app
            .reload_checkpoint_if(published.clone(), Snapshot::new(), fixture.writer.clone())
            .await;
        assert!(matches!(result, Err(SourceWriteError::PersistenceUnknown)));
        assert_eq!(fixture.app.store.status().version(), published);
        assert_eq!(std::fs::read(fixture.directory.join("intent.log")).unwrap(), log);
        assert!(!fixture.directory.join("checkpoint.bin").exists());
        assert_serving(&fixture.app).await;
        fixture.app.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn initial_rejection_cannot_acknowledge_a_recoverable_checkpoint() {
    let fixture = Fixture::new();
    let outcome = fixture
        .app
        .reload_checkpoint_if(
            fixture.app.store.status().version(),
            snapshot("invalid-directive;"),
            fixture.writer.clone(),
        )
        .await;
    assert!(matches!(outcome, Err(SourceWriteError::PersistenceUnknown)));
    assert!(fixture.app.store.current().is_none());
    assert!(
        fixture
            .app
            .store
            .observed_snapshot()
            .1
            .get(&SourceId::file("main"))
            .is_some()
    );
    assert!(fixture.writer.lock().unwrap().ensure_ready().is_err());
    assert!(!fixture.directory.join("checkpoint.bin").exists());
    fixture.app.shutdown().await;
}

#[tokio::test]
async fn recovery_conflict_does_not_claim_writer_and_prepare_failure_can_retry() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let fixture = Fixture::new();
        fixture
            .app
            .reload_checkpoint_if(
                fixture.app.store.status().version(),
                fixture.snapshot.clone(),
                fixture.writer.clone(),
            )
            .await
            .unwrap();
        drop(fixture.writer);
        let before = std::fs::read(fixture.directory.join("checkpoint.bin")).unwrap();
        let (writer, snapshots) =
            CheckpointWriter::open(&fixture.directory, "fixture", &fixture.app.registry).unwrap();
        let writer = Arc::new(Mutex::new(writer));
        let (serving, observed) = snapshots.unwrap();
        let recovered = Arc::new(App::with_sources(std::env::temp_dir(), [SourceId::file("main")]).unwrap());
        let intruder = Arc::new(App::with_sources(std::env::temp_dir(), [SourceId::file("main")]).unwrap());
        let expected = recovered.store.status().version();
        assert!(matches!(
            intruder
                .recover_checkpoint_if(
                    expected.clone(),
                    serving.clone(),
                    observed.clone(),
                    writer.clone()
                )
                .await,
            Err(CheckpointRecoveryError::Restore(
                gatel_plan::RestoreIntentError::Conflict(_)
            ))
        ));
        assert!(intruder.store.current().is_none());
        // The original App still owns the listening address. Correct recovery
        // should reach resource preparation, not be blocked by the stale caller.
        let outcome = recovered
            .recover_checkpoint_if(expected, serving.clone(), observed.clone(), writer.clone())
            .await
            .unwrap();
        assert!(matches!(outcome, Outcome::PrepareFailed(_)), "{outcome:?}");
        assert!(recovered.store.current().is_none());
        assert_eq!(recovered.store.observed_snapshot().1.revs(), observed.revs());
        assert!(writer.lock().unwrap().ensure_ready().is_err());
        assert_eq!(
            std::fs::read(fixture.directory.join("checkpoint.bin")).unwrap(),
            before
        );
        fixture.app.shutdown().await;
        let outcome = recovered
            .recover_checkpoint_if(
                recovered.store.status().version(),
                serving,
                observed,
                writer.clone(),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, Outcome::Started { .. }), "{outcome:?}");
        writer.lock().unwrap().ensure_ready().unwrap();
        assert_serving(&recovered).await;
        assert_eq!(
            std::fs::read(fixture.directory.join("checkpoint.bin")).unwrap(),
            before
        );
        recovered.shutdown().await;
        intruder.shutdown().await;
        drop(writer);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn checkpoint_writer_cannot_be_shared_between_host_lifetimes() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let fixture = Fixture::new();
        let owner = fixture.app.store.status().version().boot_id;
        fixture.writer.lock().unwrap().bind(owner).unwrap();
        let stranger = Arc::new(App::with_sources(std::env::temp_dir(), [SourceId::file("main")]).unwrap());
        let before = stranger.store.status().version();
        let result = stranger
            .reload_checkpoint_if(before.clone(), fixture.snapshot.clone(), fixture.writer.clone())
            .await;
        assert!(matches!(result, Err(SourceWriteError::PersistenceUnknown)));
        assert_eq!(stranger.store.status().version(), before);
        assert!(stranger.store.current().is_none());
        assert!(
            fixture
                .writer
                .lock()
                .unwrap()
                .save(&stranger.registry, &stranger.store.capture_configuration())
                .is_err()
        );
        assert!(stranger.store.observed_snapshot().1.sources().next().is_none());
        assert!(!fixture.directory.join("checkpoint.bin").exists());
        // Rejected cross-host access must not disable the correct owner.
        fixture.writer.lock().unwrap().bind(owner).unwrap();
        assert!(matches!(
            fixture
                .app
                .reload_checkpoint_if(
                    fixture.app.store.status().version(),
                    fixture.snapshot.clone(),
                    fixture.writer.clone()
                )
                .await,
            Ok(Outcome::Started { .. })
        ));
        assert_serving(&fixture.app).await;
        stranger.shutdown().await;
        fixture.app.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn noop_persists_new_observation_without_rewriting_serving_provenance() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let fixture = Fixture::new();
        let started = fixture
            .app
            .reload_checkpoint_if(
                fixture.app.store.status().version(),
                fixture.snapshot.clone(),
                fixture.writer.clone(),
            )
            .await
            .unwrap();
        assert!(matches!(started, Outcome::Started { .. }));
        let generation = fixture.app.store.current().unwrap().number;
        let mut revised = fixture.snapshot.get(&SourceId::file("main")).unwrap().clone();
        revised.rev = SourceRev(2);
        let outcome = fixture
            .app
            .reload_checkpoint_if(
                fixture.app.store.status().version(),
                Snapshot::new().with(revised),
                fixture.writer.clone(),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, Outcome::NoOp { generation: actual } if actual == generation));
        fixture.app.shutdown().await;
        drop(fixture.writer);
        let (reader, snapshots) =
            CheckpointWriter::open(&fixture.directory, "fixture", &fixture.app.registry).unwrap();
        let (serving, observed) = snapshots.unwrap();
        assert_eq!(serving.get(&SourceId::file("main")).unwrap().rev, SourceRev(1));
        assert_eq!(observed.get(&SourceId::file("main")).unwrap().rev, SourceRev(2));
        drop(reader);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn closed_activation_never_checkpoints_an_unactivated_publication() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let fixture = Fixture::new();
        let mut lifecycle = fixture.app.lifecycle.lock().await;
        let app = fixture.app.clone();
        let writer = fixture.writer.clone();
        let snapshot = fixture.snapshot.clone();
        let waiter = tokio::spawn(async move {
            app.reload_checkpoint_if(app.store.status().version(), snapshot, writer)
                .await
        });
        while fixture.app.store.current().is_none() {
            tokio::task::yield_now().await;
        }
        assert_eq!(fixture.app.activated.load(Ordering::Acquire), 0);
        // Inject the lifecycle state established by shutdown while holding its
        // mutex. This is a deterministic activation guard test, not a claim of
        // reproducing the scheduler order of every real shutdown interleaving.
        lifecycle.closed = true;
        fixture.app.store.seal();
        drop(lifecycle);
        assert!(matches!(
            waiter.await.unwrap(),
            Err(SourceWriteError::PersistenceUnknown)
        ));
        assert!(fixture.writer.lock().unwrap().ensure_ready().is_err());
        assert!(!fixture.directory.join("checkpoint.bin").exists());
        assert!(fixture.app.server.active_addresses().await.is_empty());
        fixture.app.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancelled_checkpoint_waiter_still_saves_after_activation() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let fixture = Fixture::new();
        let lifecycle = fixture.app.lifecycle.lock().await;
        let app = fixture.app.clone();
        let writer = fixture.writer.clone();
        let snapshot = fixture.snapshot.clone();
        let waiter = tokio::spawn(async move {
            app.reload_checkpoint_if(app.store.status().version(), snapshot, writer)
                .await
        });
        while fixture.app.store.current().is_none() {
            tokio::task::yield_now().await;
        }
        assert!(!fixture.directory.join("checkpoint.bin").exists());
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        drop(lifecycle);
        fixture.app.submissions.drained().await;
        assert!(fixture.directory.join("checkpoint.bin").is_file());
        fixture.writer.lock().unwrap().ensure_ready().unwrap();
        assert_eq!(
            fixture.app.activated.load(Ordering::Acquire),
            fixture.app.store.current().unwrap().number.number()
        );
        fixture.app.shutdown().await;
        drop(fixture.writer);
        let (reader, snapshots) =
            CheckpointWriter::open(&fixture.directory, "fixture", &fixture.app.registry).unwrap();
        let (serving, observed) = snapshots.unwrap();
        assert_eq!(serving.revs(), fixture.snapshot.revs());
        assert_eq!(observed.revs(), fixture.snapshot.revs());
        assert_eq!(
            gatel_model::snapshot_codec::encode(&serving).unwrap(),
            gatel_model::snapshot_codec::encode(&fixture.snapshot).unwrap()
        );
        let recovered = Arc::new(App::with_sources(std::env::temp_dir(), [SourceId::file("main")]).unwrap());
        let reader = Arc::new(Mutex::new(reader));
        let initial = recovered.store.status().version();
        assert!(matches!(
            recovered
                .recover_checkpoint_if(initial.clone(), Snapshot::new(), observed.clone(), reader.clone())
                .await,
            Err(CheckpointRecoveryError::WriterUnavailable)
        ));
        assert_eq!(recovered.store.status().version(), initial);
        assert!(recovered.store.current().is_none());
        let lifecycle = recovered.lifecycle.lock().await;
        let owner = recovered.clone();
        let target = reader.clone();
        let recovery = tokio::spawn(async move {
            owner
                .recover_checkpoint_if(initial, serving, observed, target)
                .await
        });
        while recovered.store.current().is_none() {
            tokio::task::yield_now().await;
        }
        assert!(reader.lock().unwrap().ensure_ready().is_err());
        assert!(recovered.reload_execution.try_lock().is_err());
        recovery.abort();
        assert!(recovery.await.unwrap_err().is_cancelled());
        drop(lifecycle);
        recovered.submissions.drained().await;
        assert_ne!(
            recovered.store.status().version().boot_id,
            fixture.app.store.status().version().boot_id
        );
        assert_serving(&recovered).await;
        reader.lock().unwrap().ensure_ready().unwrap();
        assert!(matches!(
            recovered
                .reload_checkpoint_if(
                    recovered.store.status().version(),
                    fixture.snapshot.clone(),
                    reader.clone()
                )
                .await,
            Ok(Outcome::NoOp { .. })
        ));
        recovered.shutdown().await;
        drop(reader);
    })
    .await
    .unwrap();
}
