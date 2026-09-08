use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

#[derive(Debug)]
struct NeverRun;
impl Protocol for NeverRun {
    fn meta(&self) -> &'static DirectiveMeta {
        static META: DirectiveMeta = DirectiveMeta::terminal("test-cgi");
        &META
    }
    fn run(&self, _: ExchangeRequest, _: Sink) -> Running {
        panic!("incomplete upload must not reach exchange")
    }
}

struct PendingBody(Arc<AtomicBool>, bool);
#[test]
fn network_factory_rejects_process_limit_at_decode_and_prepare() {
    let factory = NetworkFactory(Arc::new(NeverRun));
    let origin = gatel_model::Origin::synthetic("network-limit");
    for limit in [0, 1, 64, 1024, 1025] {
        let node = Node::new("test-cgi", origin.clone())
            .with_arg(gatel_model::Value::string("127.0.0.1:9000", origin.clone()))
            .with_prop("root", gatel_model::Value::string(".", origin.clone()))
            .with_prop("script", gatel_model::Value::string("app", origin.clone()))
            .with_prop("process-limit", gatel_model::Value::int(limit, origin.clone()));
        assert!(
            directive(NeverRun)
                .decode(&node, &mut Diagnostics::new())
                .is_none()
        );
        let config = BackendConfig {
            concurrency: 1,
            process_limit: Some(limit as u32),
            target: "127.0.0.1:9000",
            config_root: std::path::Path::new("."),
            root: ".",
            script: "app",
            remote_root: None,
        };
        assert!(factory.validate(config).unwrap_err().contains("process-limit"));
        assert!(factory.prepare(config).is_err());
    }
}
impl Drop for PendingBody {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
impl http_body::Body for PendingBody {
    type Data = Bytes;
    type Error = salvo_core::BoxedError;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        if !self.1 {
            self.1 = true;
            return Poll::Ready(Some(Ok(http_body::Frame::data(Bytes::from_static(
                b"partial upload",
            )))));
        }
        Poll::Pending
    }
}
fn pending_request(dropped: Arc<AtomicBool>) -> Request {
    let mut request = Request::default();
    *request.body_mut() = salvo_core::http::ReqBody::Boxed {
        inner: Box::pin(PendingBody(dropped, false)),
        fuse_config: None,
    };
    request
}

#[tokio::test]
async fn cancellation_while_buffering_releases_input_and_admission_without_dial() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let slots = Arc::new(Semaphore::new(1));
    let terminal = Terminal {
        config: Config {
            fields: Fields {
                backend: address.to_string(),
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
                timeout: 60,
                idle_timeout: 15,
                pass_authorization: false,
            },
            factory: Arc::new(NetworkFactory(Arc::new(NeverRun))),
        },
        script: Script::resolve(root, ".", "Cargo.toml", "/app", None).unwrap(),
        backend: NetworkFactory(Arc::new(NeverRun))
            .prepare(BackendConfig {
                concurrency: 1,
                process_limit: None,
                target: &address.to_string(),
                config_root: root,
                root: ".",
                script: "Cargo.toml",
                remote_root: None,
            })
            .unwrap(),
        slots: slots.clone(),
    };
    let depot = Depot::new();
    for _ in 0..2 {
        let dropped = Arc::new(AtomicBool::new(false));
        let mut request = pending_request(dropped.clone());
        let mut running = Box::pin(terminal.execute(&mut request, &depot));
        let mut context = Context::from_waker(std::task::Waker::noop());
        assert!(running.as_mut().poll(&mut context).is_pending());
        assert_eq!(slots.available_permits(), 0);
        assert!(!dropped.load(Ordering::SeqCst));
        let mut competitor = pending_request(Arc::new(AtomicBool::new(false)));
        assert!(matches!(
            terminal.execute(&mut competitor, &depot).await,
            Err(StatusCode::SERVICE_UNAVAILABLE)
        ));
        drop(running);
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(slots.available_permits(), 1);
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(25), listener.accept())
            .await
            .is_err()
    );
    // A complete input reaches NeverRun, whose factory panics synchronously.
    // The adapter must translate it and still release the capacity/socket.
    let mut empty = Request::default();
    assert!(matches!(
        terminal.execute(&mut empty, &depot).await,
        Err(StatusCode::BAD_GATEWAY)
    ));
    assert_eq!(slots.available_permits(), 1);
    let (mut socket, _) = listener.accept().await.unwrap();
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), socket.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(bytes.is_empty());
}
