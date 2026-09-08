use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{FutureExt, Stream, TryStreamExt};
use gatel_cgi_protocol::head::Head;
use gatel_macros::FromNode;
use gatel_model::{Diagnostics, FromNode as _, Hasher, Node};
use gatel_plugin_runtime::{BuildContext, Directive, DirectiveConfig, DirectiveMeta, DirectiveState};
use http_body::Body as _;
use http_body_util::BodyExt;
use salvo_core::http::{Request, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Semaphore, mpsc};

use crate::backend::{Backend, BackendConfig, BackendFactory, Invocation, NetworkFactory};
use crate::mapping::Script;

#[derive(Debug, Clone, FromNode)]
struct Fields {
    #[gatel(arg)]
    backend: String,
    #[gatel(prop)]
    root: String,
    #[gatel(prop)]
    script: String,
    #[gatel(prop)]
    remote_root: Option<String>,
    #[gatel(prop, default = String::from("/index.php"))]
    script_name: String,
    #[gatel(prop, default = String::from("localhost"))]
    server_name: String,
    #[gatel(prop, default = 32)]
    concurrency: u32,
    #[gatel(prop)]
    process_limit: Option<u32>,
    #[gatel(prop, default = 16_777_216)]
    max_body: u64,
    #[gatel(prop, default = false)]
    buffer_unknown: bool,
    #[gatel(prop, default = 1_048_576)]
    max_buffer: u64,
    #[gatel(prop, default = 60)]
    timeout: u32,
    #[gatel(prop, default = 15)]
    idle_timeout: u32,
    #[gatel(prop, default = false)]
    pass_authorization: bool,
}

#[derive(Debug, Clone)]
struct Config {
    fields: Fields,
    factory: Arc<dyn BackendFactory>,
}
struct Capacity {
    http: Arc<Semaphore>,
    process: Arc<Semaphore>,
}
impl std::ops::Deref for Config {
    type Target = Fields;
    fn deref(&self) -> &Fields {
        &self.fields
    }
}
#[derive(Debug)]
pub enum ExchangeError {
    Timeout,
    Unavailable,
    Failed,
}
impl ExchangeError {
    fn status(self) -> StatusCode {
        match self {
            Self::Timeout => StatusCode::GATEWAY_TIMEOUT,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Failed => StatusCode::BAD_GATEWAY,
        }
    }
}
pub trait Transport: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Transport for T {}
pub struct ExchangeRequest {
    pub stream: Box<dyn Transport>,
    pub input: Box<dyn AsyncRead + Unpin + Send>,
    pub environment: gatel_cgi_protocol::environment::Environment,
    pub length: u64,
    pub head_request: bool,
    pub timeout: Duration,
    pub idle_timeout: Duration,
    pub max_request: u64,
}
pub trait Protocol: std::fmt::Debug + Send + Sync + 'static {
    fn meta(&self) -> &'static DirectiveMeta;
    fn run(&self, request: ExchangeRequest, output: Sink) -> Running;
}

impl Config {
    fn backend_config<'a>(&'a self, config_root: &'a std::path::Path) -> BackendConfig<'a> {
        BackendConfig {
            concurrency: self.concurrency as usize,
            process_limit: self.process_limit,
            target: &self.backend,
            config_root,
            root: &self.root,
            script: &self.script,
            remote_root: self.remote_root.as_deref(),
        }
    }
}
impl DirectiveConfig for Config {
    fn hash_into(&self, hash: &mut Hasher) {
        // Shared adapter behavior is part of every protocol's plan identity,
        // including third-party protocols using this configuration wrapper.
        hash.update_str("gatel-cgi-http-config-v9");
        hash.update_bool(self.process_limit.is_some());
        hash.update_u64(self.process_limit.unwrap_or_default() as u64);
        for value in [
            &self.backend,
            &self.root,
            &self.script,
            &self.script_name,
            &self.server_name,
        ] {
            hash.update_str(value);
        }
        for value in [self.concurrency as u64, self.max_body, self.timeout as u64] {
            hash.update_u64(value);
        }
        hash.update_bool(self.pass_authorization);
        hash.update_u64(self.idle_timeout as u64);
        hash.update_bool(self.buffer_unknown);
        hash.update_u64(self.max_buffer);
        hash.update_opt_str(self.remote_root.as_deref());
    }
    fn state(&self) -> Option<DirectiveState> {
        let mut hash = Hasher::new();
        hash.update_str("gatel-cgi-capacity-v2");
        hash.update_u64(self.concurrency as u64);
        let concurrency = self.concurrency as usize;
        Some(DirectiveState {
            compatibility: hash.finish(),
            create: Box::new(move || {
                Arc::new(Capacity {
                    http: Arc::new(Semaphore::new(concurrency)),
                    process: Arc::new(Semaphore::new(concurrency)),
                })
            }),
        })
    }
    fn prepare(&self, cx: BuildContext<'_>) -> Result<gatel_plugin_runtime::PreparedDirective, String> {
        let script = Script::resolve(
            cx.root,
            &self.root,
            &self.script,
            &self.script_name,
            self.remote_root.as_deref(),
        )?;
        let capacity: Arc<Capacity> = cx.state("CGI capacity")?;
        let backend = self
            .factory
            .prepare_with_capacity(self.backend_config(cx.root), capacity.process.clone())?;
        let tasks = backend.background()?;
        Ok(gatel_plugin_runtime::PreparedDirective::new(Arc::new(Terminal {
            script,
            backend,
            config: self.clone(),
            slots: capacity.http.clone(),
        }))
        .with_tasks(tasks))
    }
    fn describe(&self) -> String {
        format!(
            "{} {} script={}",
            self.factory.meta().name,
            self.backend,
            self.script
        )
    }
}
struct CgiDirective(Arc<dyn BackendFactory>);
impl Directive for CgiDirective {
    fn meta(&self) -> &DirectiveMeta {
        self.0.meta()
    }
    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let config = Config {
            fields: Fields::from_node(node, diagnostics)?,
            factory: self.0.clone(),
        };
        if let Err(error) = Script::validate(&config.script, &config.script_name) {
            diagnostics.error(gatel_model::Code::InvalidValue, node.origin.clone(), error);
            return None;
        }
        if !(1..=4096).contains(&config.concurrency)
            || !(1..=3600).contains(&config.timeout)
            || !(1..=3600).contains(&config.idle_timeout)
            || config.max_body > 1024 * 1024 * 1024
            || config.max_buffer > 1024 * 1024 * 1024
            || config.server_name.is_empty()
            || config.server_name.chars().any(char::is_control)
        {
            diagnostics.error(
                gatel_model::Code::InvalidValue,
                node.origin.clone(),
                "invalid CGI budget or server-name",
            );
            return None;
        }
        if let Some(remote) = &config.remote_root
            && let Err(error) = Script::validate_remote(remote)
        {
            diagnostics.error(gatel_model::Code::InvalidValue, node.origin.clone(), error);
            return None;
        }
        if let Err(error) = self.0.validate(config.backend_config(std::path::Path::new("."))) {
            diagnostics.error(gatel_model::Code::InvalidValue, node.origin.clone(), error);
            return None;
        }
        Some(Arc::new(config))
    }
}
pub fn directive(protocol: impl Protocol) -> impl Directive {
    backend_directive(NetworkFactory(Arc::new(protocol)))
}
/// Build a CGI terminal without requiring a network transport.
pub fn backend_directive(factory: impl BackendFactory) -> impl Directive {
    CgiDirective(Arc::new(factory))
}

struct Terminal {
    config: Config,
    script: Script,
    backend: Arc<dyn Backend>,
    slots: Arc<Semaphore>,
}

enum Message {
    Head(Head),
    Body(Bytes),
}
const EXPECT_HEAD: u8 = 0;
const WRITING_BODY: u8 = 1;
const FINISHED: u8 = 2;
const FAILED: u8 = 3;
const OUTPUT_CHUNK: usize = 8192;
type Completion = Arc<std::sync::atomic::AtomicU8>;
pub struct Sink {
    sender: mpsc::Sender<Message>,
    completion: Completion,
}
impl Sink {
    fn begin(&self, expected: u8) -> io::Result<()> {
        // Poison first: a cancelled callback must not leave success behind.
        let phase = self.completion.swap(FAILED, std::sync::atomic::Ordering::AcqRel);
        if phase != expected {
            return Err(io::Error::other("invalid CGI output lifecycle"));
        }
        Ok(())
    }
    pub async fn head(&mut self, head: Head) -> io::Result<()> {
        self.begin(EXPECT_HEAD)?;
        self.sender
            .send(Message::Head(head))
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        self.completion
            .store(WRITING_BODY, std::sync::atomic::Ordering::Release);
        Ok(())
    }
    pub async fn body(&mut self, body: &[u8]) -> io::Result<()> {
        self.begin(WRITING_BODY)?;
        for chunk in body.chunks(OUTPUT_CHUNK) {
            // Reserve before copying: a slow reader cannot make the adapter
            // accumulate an extra large pending allocation outside the queue.
            let slot = self
                .sender
                .reserve()
                .await
                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
            slot.send(Message::Body(Bytes::copy_from_slice(chunk)));
        }
        self.completion
            .store(WRITING_BODY, std::sync::atomic::Ordering::Release);
        Ok(())
    }
    pub async fn finish(&mut self) -> io::Result<()> {
        self.begin(WRITING_BODY)?;
        self.completion
            .store(FINISHED, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}
pub type Running = Pin<Box<dyn std::future::Future<Output = Result<(), ExchangeError>> + Send>>;
async fn checked_transfer(transfer: Running, completion: Completion) -> Result<(), ExchangeError> {
    std::panic::AssertUnwindSafe(transfer)
        .catch_unwind()
        .await
        .map_err(|_| ExchangeError::Failed)??;
    if completion.load(std::sync::atomic::Ordering::Acquire) != FINISHED {
        return Err(ExchangeError::Failed);
    }
    Ok(())
}
struct ResponseStream {
    running: Option<Running>,
    receiver: mpsc::Receiver<Message>,
    error: Option<io::Error>,
}
impl Stream for ResponseStream {
    type Item = io::Result<Bytes>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.receiver.poll_recv(cx) {
                Poll::Ready(Some(Message::Body(bytes))) => return Poll::Ready(Some(Ok(bytes))),
                Poll::Ready(Some(Message::Head(_))) => {
                    return Poll::Ready(Some(Err(io::Error::other("duplicate CGI response head"))));
                }
                _ => {}
            }
            if let Some(running) = &mut self.running {
                match running.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(result) => {
                        self.running = None;
                        self.error = result.err().map(|e| io::Error::other(format!("{e:?}")));
                        continue;
                    }
                }
            }
            return Poll::Ready(self.error.take().map(Err));
        }
    }
}

impl Terminal {
    async fn execute(
        &self,
        request: &mut Request,
        depot: &Depot,
    ) -> Result<(Head, ResponseStream), StatusCode> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(self.config.timeout as u64);
        let length = match request.headers().get("content-length") {
            Some(value) => Some(
                value
                    .to_str()
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or(StatusCode::BAD_REQUEST)?,
            ),
            None if request.headers().contains_key("transfer-encoding")
                || !request.body().is_end_stream() =>
            {
                if !self.config.buffer_unknown {
                    return Err(StatusCode::LENGTH_REQUIRED);
                }
                None
            }
            None => Some(0),
        };
        if length.is_some_and(|length| length > self.config.max_body) {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        let remote = depot
            .get::<std::net::IpAddr>("gatel.client_ip")
            .map(|ip| ip.to_string())
            .unwrap_or_default();
        let tls = depot.get::<bool>("gatel.tls").copied().unwrap_or(false);
        let protocol = format!("{:?}", request.version());
        let mut env = self
            .script
            .environment(crate::mapping::RequestInfo {
                uri: request.uri(),
                headers: request.headers(),
                method: request.method().as_str(),
                length: length.unwrap_or(0),
                remote: &remote,
                server: &self.config.server_name,
                port: request
                    .local_addr()
                    .clone()
                    .into_std()
                    .map(|addr| addr.port())
                    .unwrap_or(if tls { 443 } else { 80 }),
                protocol: &protocol,
                tls,
                pass_authorization: self.config.pass_authorization,
            })
            .map_err(|_| StatusCode::BAD_REQUEST)?;
        // Reject invalid metadata before consuming or waiting for body bytes.
        // Capacity bounds buffering as well as the subsequent backend exchange.
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let mut input: Box<dyn AsyncRead + Unpin + Send> = Box::new(tokio_util::io::StreamReader::new(
            request.take_body().into_data_stream().map_err(|error| {
                if let Some(limit) = gatel_plugin_runtime::BodyTooLarge::find(&error) {
                    io::Error::other(limit)
                } else {
                    io::Error::other("request body failure")
                }
            }),
        ));
        let length = match length {
            Some(length) => length,
            None => {
                let body = tokio::time::timeout_at(
                    deadline,
                    crate::input::buffer(
                        &mut input,
                        self.config.max_buffer.min(self.config.max_body),
                        Duration::from_secs(self.config.idle_timeout as u64),
                    ),
                )
                .await
                .map_err(|_| StatusCode::REQUEST_TIMEOUT)?
                .map_err(|error| match error {
                    crate::input::Error::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
                    crate::input::Error::Read => StatusCode::BAD_REQUEST,
                    crate::input::Error::Timeout => StatusCode::REQUEST_TIMEOUT,
                })?;
                let length = body.len() as u64;
                input = Box::new(std::io::Cursor::new(body));
                length
            }
        };
        env.replace_existing("CONTENT_LENGTH", length.to_string().as_bytes())
            .map_err(|_| StatusCode::BAD_REQUEST)?;
        let head_request = request.method() == http::Method::HEAD;
        let (sender, mut receiver) = mpsc::channel(1);
        let completion = Arc::new(std::sync::atomic::AtomicU8::new(EXPECT_HEAD));
        let exchange_request = Invocation {
            input,
            environment: env,
            length,
            head_request,
            deadline,
            idle_timeout: Duration::from_secs(self.config.idle_timeout as u64),
            max_request: self.config.max_body,
        };
        let output = Sink {
            sender,
            completion: completion.clone(),
        };
        let backend = self.backend.clone();
        // Defer factory invocation into the guarded future too. Factory and
        // polling panics use the same failure path, never backend panic text.
        let transfer: Running = Box::pin(async move { backend.run(exchange_request, output).await });
        let mut running: Running = Box::pin(async move {
            let _permit = permit;
            tokio::time::timeout_at(deadline, checked_transfer(transfer, completion))
                .await
                .unwrap_or(Err(ExchangeError::Timeout))
        });
        let head = tokio::select! {
            biased;
            value = receiver.recv() => match value {
                Some(Message::Head(head)) => head,
                _ => return Err(running.await.err().map(ExchangeError::status).unwrap_or(StatusCode::BAD_GATEWAY)),
            },
            result = &mut running => {
                result.map_err(ExchangeError::status)?;
                let Some(Message::Head(head)) = receiver.recv().await else { return Err(StatusCode::BAD_GATEWAY); };
                return Ok((head, ResponseStream { running: None, receiver, error: None }));
            },
        };
        if head_request || head.content_length == Some(0) || matches!(head.status.as_u16(), 204 | 304) {
            running.await.map_err(ExchangeError::status)?;
            return Ok((
                head,
                ResponseStream {
                    running: None,
                    receiver,
                    error: None,
                },
            ));
        }
        Ok((
            head,
            ResponseStream {
                running: Some(running),
                receiver,
                error: None,
            },
        ))
    }
}
#[async_trait]
impl Handler for Terminal {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        match self.execute(request, depot).await {
            Ok((head, stream)) => {
                response.status_code(head.status);
                for name in head.headers.keys() {
                    response.headers_mut().remove(name);
                }
                for (name, value) in &head.headers {
                    response.headers_mut().append(name.clone(), value.clone());
                }
                response.stream(stream);
            }
            Err(status) => {
                depot.insert("gatel.generated", true);
                response.status_code(status);
                if request.method() != http::Method::HEAD {
                    let _ = response.write_body("CGI request failed\n");
                }
            }
        }
        ctrl.skip_rest();
    }
}

#[cfg(test)]
#[path = "output_tests.rs"]
mod output_tests;

#[cfg(test)]
#[path = "input_tests.rs"]
mod input_tests;

#[cfg(test)]
#[path = "backend_tests.rs"]
mod backend_tests;
