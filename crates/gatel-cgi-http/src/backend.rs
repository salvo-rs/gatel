//! Backend-independent invocation ownership. Preparing a backend must not start
//! request work; each returned future owns all work for exactly one invocation.
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use gatel_plugin_runtime::DirectiveMeta;
use tokio::io::AsyncRead;

use crate::{ExchangeError, ExchangeRequest, Protocol, Running, Sink, Transport};

/// Validated HTTP input, without an assumed socket or child process.
pub struct Invocation {
    pub input: Box<dyn AsyncRead + Unpin + Send>,
    pub environment: gatel_cgi_protocol::environment::Environment,
    pub length: u64,
    pub head_request: bool,
    /// Absolute budget includes backend startup, exchange and output backpressure.
    pub deadline: tokio::time::Instant,
    pub idle_timeout: Duration,
    pub max_request: u64,
}

/// Configuration-only inputs. No request headers can choose the executable or
/// destination. Paths are relative to config_root unless already absolute.
#[derive(Clone, Copy)]
pub struct BackendConfig<'a> {
    /// Process backends must count cleanup as well as HTTP transfer admission.
    pub concurrency: usize,
    /// Optional per-invocation root + descendant limit. Network backends reject it.
    pub process_limit: Option<u32>,
    pub target: &'a str,
    pub config_root: &'a Path,
    pub root: &'a str,
    pub script: &'a str,
    pub remote_root: Option<&'a str>,
}

/// Plugin extension point for network, process, or in-process CGI backends.
/// This is a trusted Rust interface, not an OS sandbox or stable binary ABI.
pub trait BackendFactory: std::fmt::Debug + Send + Sync + 'static {
    fn meta(&self) -> &'static DirectiveMeta;
    /// Syntax validation only: no I/O and no request work.
    fn validate(&self, config: BackendConfig<'_>) -> Result<(), String>;
    /// Resolve immutable configuration. Do not spawn, connect, or detach tasks.
    fn prepare(&self, config: BackendConfig<'_>) -> Result<Arc<dyn Backend>, String>;
    /// Process admission persists across compatible prepared generations.
    /// Network adapters need no additional pool and retain the default path.
    fn prepare_with_capacity(
        &self,
        config: BackendConfig<'_>,
        _capacity: Arc<tokio::sync::Semaphore>,
    ) -> Result<Arc<dyn Backend>, String> {
        self.prepare(config)
    }
}

pub trait Backend: Send + Sync + 'static {
    /// Inert generation-local workers sharing this prepared backend's resources.
    /// Called once by the host's unified directive preparation, not per route.
    fn background(&self) -> Result<Vec<gatel_plugin_runtime::BackgroundTask>, String> {
        Ok(Vec::new())
    }
    /// Startup must be deferred into the returned future. The future owns all
    /// per-request resources; dropping it cancels the invocation. Process
    /// implementations must additionally arrange bounded kill/reap supervision.
    /// Finish the sink only after protocol completion (including child status).
    fn run(&self, invocation: Invocation, output: Sink) -> Running;
}

#[derive(Debug)]
pub(crate) struct NetworkFactory(pub Arc<dyn Protocol>);
impl BackendFactory for NetworkFactory {
    fn meta(&self) -> &'static DirectiveMeta {
        self.0.meta()
    }
    fn validate(&self, config: BackendConfig<'_>) -> Result<(), String> {
        if config.process_limit.is_some() {
            return Err("network CGI does not support process-limit".into());
        }
        target(config.target, config.config_root).map(|_| ())
    }
    fn prepare(&self, config: BackendConfig<'_>) -> Result<Arc<dyn Backend>, String> {
        self.validate(config)?;
        Ok(Arc::new(NetworkBackend {
            target: target(config.target, config.config_root)?,
            protocol: self.0.clone(),
        }))
    }
}

#[derive(Clone)]
enum Target {
    Tcp(std::net::SocketAddr),
    #[cfg(unix)]
    Unix(std::path::PathBuf),
}
fn target(value: &str, root: &Path) -> Result<Target, String> {
    if let Some(path) = value.strip_prefix("unix:") {
        if path.is_empty() {
            return Err("empty Unix socket path".into());
        }
        #[cfg(unix)]
        {
            return Ok(Target::Unix(root.join(path)));
        }
        #[cfg(not(unix))]
        {
            let _ = (path, root);
            return Err("Unix CGI sockets are unsupported on this platform".into());
        }
    }
    value
        .parse()
        .map(Target::Tcp)
        .map_err(|_| "CGI backend must be an IP:port or unix:path".into())
}

struct NetworkBackend {
    target: Target,
    protocol: Arc<dyn Protocol>,
}
impl Backend for NetworkBackend {
    fn run(&self, request: Invocation, output: Sink) -> Running {
        let target = self.target.clone();
        let protocol = self.protocol.clone();
        Box::pin(async move {
            let dial_deadline = request
                .deadline
                .min(tokio::time::Instant::now() + Duration::from_secs(5));
            let stream: Box<dyn Transport> = match target {
                Target::Tcp(address) => Box::new(
                    tokio::time::timeout_at(dial_deadline, tokio::net::TcpStream::connect(address))
                        .await
                        .map_err(|_| ExchangeError::Timeout)?
                        .map_err(|_| ExchangeError::Failed)?,
                ),
                #[cfg(unix)]
                Target::Unix(path) => Box::new(
                    tokio::time::timeout_at(dial_deadline, tokio::net::UnixStream::connect(path))
                        .await
                        .map_err(|_| ExchangeError::Timeout)?
                        .map_err(|_| ExchangeError::Failed)?,
                ),
            };
            protocol
                .run(
                    ExchangeRequest {
                        stream,
                        input: request.input,
                        environment: request.environment,
                        length: request.length,
                        head_request: request.head_request,
                        timeout: request
                            .deadline
                            .saturating_duration_since(tokio::time::Instant::now()),
                        idle_timeout: request.idle_timeout,
                        max_request: request.max_request,
                    },
                    output,
                )
                .await
        })
    }
}
