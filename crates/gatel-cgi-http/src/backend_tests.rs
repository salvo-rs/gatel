use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

#[derive(Debug, Clone, Copy)]
enum Mode {
    Complete,
    Pending,
    Panic,
    Fail,
    Unavailable,
    Timeout,
    Streaming,
}
#[derive(Debug)]
struct Factory;
impl BackendFactory for Factory {
    fn meta(&self) -> &'static DirectiveMeta {
        static META: DirectiveMeta = DirectiveMeta::terminal("local-cgi");
        &META
    }
    fn validate(&self, config: BackendConfig<'_>) -> Result<(), String> {
        if config.target == "local" {
            Ok(())
        } else {
            Err("expected local backend".into())
        }
    }
    fn prepare(&self, _: BackendConfig<'_>) -> Result<Arc<dyn Backend>, String> {
        panic!("decode must never prepare a backend")
    }
}

struct LocalBackend(Mode, Arc<AtomicUsize>);
impl Backend for LocalBackend {
    fn run(&self, invocation: Invocation, mut sink: Sink) -> Running {
        if matches!(self.0, Mode::Panic) {
            panic!("private backend failure");
        }
        let mode = self.0;
        let live = self.1.clone();
        Box::pin(async move {
            struct Active(Arc<AtomicUsize>);
            impl Drop for Active {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::SeqCst);
                }
            }
            live.fetch_add(1, Ordering::SeqCst);
            let _active = Active(live);
            let _input = invocation.input;
            if matches!(mode, Mode::Pending) {
                std::future::pending::<()>().await;
            }
            if matches!(mode, Mode::Fail) {
                return Err(ExchangeError::Failed);
            }
            if matches!(mode, Mode::Unavailable) {
                return Err(ExchangeError::Unavailable);
            }
            if matches!(mode, Mode::Timeout) {
                return Err(ExchangeError::Timeout);
            }
            if matches!(mode, Mode::Streaming) {
                sink.head(Head {
                    status: StatusCode::OK,
                    headers: http::HeaderMap::new(),
                    content_length: Some(2),
                })
                .await
                .map_err(|_| ExchangeError::Failed)?;
                sink.body(b"x").await.map_err(|_| ExchangeError::Failed)?;
                std::future::pending::<()>().await;
            }
            assert_eq!(invocation.length, 0);
            sink.head(Head {
                status: StatusCode::OK,
                headers: http::HeaderMap::new(),
                content_length: Some(0),
            })
            .await
            .map_err(|_| ExchangeError::Failed)?;
            sink.finish().await.map_err(|_| ExchangeError::Failed)
        })
    }
}

fn terminal(mode: Mode) -> (Terminal, Arc<AtomicUsize>) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let live = Arc::new(AtomicUsize::new(0));
    (
        Terminal {
            config: Config {
                fields: Fields {
                    backend: "local".into(),
                    root: ".".into(),
                    script: "Cargo.toml".into(),
                    remote_root: None,
                    script_name: "/app".into(),
                    server_name: "localhost".into(),
                    concurrency: 1,
                    process_limit: None,
                    max_body: 1024,
                    buffer_unknown: true,
                    max_buffer: 1024,
                    timeout: 1,
                    idle_timeout: 1,
                    pass_authorization: false,
                },
                factory: Arc::new(Factory),
            },
            script: Script::resolve(root, ".", "Cargo.toml", "/app", None).unwrap(),
            backend: Arc::new(LocalBackend(mode, live.clone())),
            slots: Arc::new(Semaphore::new(1)),
        },
        live,
    )
}

#[test]
fn custom_backend_validation_does_not_impose_network_syntax_or_prepare() {
    let origin = gatel_model::Origin::synthetic("local");
    let directive = backend_directive(Factory);
    for (target, valid) in [("local", true), ("127.0.0.1:9000", false)] {
        let node = Node::new("local-cgi", origin.clone())
            .with_arg(gatel_model::Value::string(target, origin.clone()))
            .with_prop("root", gatel_model::Value::string("missing-root", origin.clone()))
            .with_prop("script", gatel_model::Value::string("app", origin.clone()));
        assert_eq!(directive.decode(&node, &mut Diagnostics::new()).is_some(), valid);
    }
}

#[test]
fn process_limit_changes_plan_identity_but_preserves_capacity() {
    let (terminal, _) = terminal(Mode::Complete);
    let mut config = terminal.config;
    let capacity = config.state().unwrap().compatibility;
    let mut identities = Vec::new();
    for limit in [None, Some(1), Some(64), Some(1024)] {
        config.fields.process_limit = limit;
        assert_eq!(
            config.backend_config(std::path::Path::new(".")).process_limit,
            limit
        );
        let mut hash = Hasher::new();
        config.hash_into(&mut hash);
        let identity = hash.finish();
        assert!(!identities.contains(&identity));
        identities.push(identity);
        assert_eq!(config.state().unwrap().compatibility, capacity);
    }
}

#[tokio::test]
async fn closed_output_preserves_pre_header_backend_error_status() {
    for (mode, expected) in [
        (Mode::Unavailable, StatusCode::SERVICE_UNAVAILABLE),
        (Mode::Timeout, StatusCode::GATEWAY_TIMEOUT),
        (Mode::Fail, StatusCode::BAD_GATEWAY),
    ] {
        let (terminal, live) = terminal(mode);
        let error = terminal
            .execute(&mut Request::default(), &Depot::new())
            .await
            .err()
            .unwrap();
        assert_eq!(error, expected);
        assert_eq!(live.load(Ordering::SeqCst), 0);
        assert_eq!(terminal.slots.available_permits(), 1);
    }
}

#[tokio::test]
async fn non_network_backend_completion_failure_and_factory_panic_release_capacity() {
    for mode in [Mode::Complete, Mode::Fail, Mode::Panic] {
        let (terminal, live) = terminal(mode);
        let result = terminal.execute(&mut Request::default(), &Depot::new()).await;
        if matches!(mode, Mode::Complete) {
            assert_eq!(result.unwrap().0.status, StatusCode::OK);
        } else {
            assert!(matches!(result, Err(StatusCode::BAD_GATEWAY)));
        }
        assert_eq!(terminal.slots.available_permits(), 1);
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn backend_startup_timeout_and_cancellation_drop_work_and_release_capacity() {
    use std::future::Future;
    let (terminal, live) = terminal(Mode::Pending);
    let mut request = Request::default();
    let depot = Depot::new();
    let mut pending = Box::pin(terminal.execute(&mut request, &depot));
    let mut context = Context::from_waker(std::task::Waker::noop());
    assert!(pending.as_mut().poll(&mut context).is_pending());
    assert_eq!(live.load(Ordering::SeqCst), 1);
    assert_eq!(terminal.slots.available_permits(), 0);
    assert!(matches!(
        terminal.execute(&mut Request::default(), &depot).await,
        Err(StatusCode::SERVICE_UNAVAILABLE)
    ));
    drop(pending);
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert_eq!(terminal.slots.available_permits(), 1);
    assert!(matches!(
        terminal.execute(&mut Request::default(), &depot).await,
        Err(StatusCode::GATEWAY_TIMEOUT)
    ));
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert_eq!(terminal.slots.available_permits(), 1);
}

#[tokio::test]
async fn response_stream_owns_non_network_work_after_headers() {
    use futures_util::StreamExt;
    let (terminal, live) = terminal(Mode::Streaming);
    let (head, mut stream) = terminal
        .execute(&mut Request::default(), &Depot::new())
        .await
        .unwrap();
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(stream.next().await.unwrap().unwrap(), Bytes::from_static(b"x"));
    assert_eq!(live.load(Ordering::SeqCst), 1);
    assert_eq!(terminal.slots.available_permits(), 0);
    drop(stream);
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert_eq!(terminal.slots.available_permits(), 1);
}
