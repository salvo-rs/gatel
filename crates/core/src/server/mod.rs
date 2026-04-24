pub mod graceful;
#[cfg(feature = "http3")]
pub mod h3_server;
pub mod http_server;
pub mod proxy_protocol;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use arc_swap::ArcSwap;
pub use graceful::GracefulShutdown;
pub use proxy_protocol::{PrefixedStream, ProxyProtocolHeader};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::config::AppConfig;
use crate::hoops::metrics::Metrics;
use crate::plugin::ModuleRegistry;
use crate::proxy::activity::BackendActivityTracker;
use crate::runtime_services::{
    RuntimeHealthCheck, RuntimeServiceError, RuntimeServiceRegistry, RuntimeState,
    RuntimeTargetState, merge_runtime_config, route_health_check, runtime_target_activity_key,
};
use crate::salvo_service;
use crate::tls::TlsManager;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeStateError {
    #[error(transparent)]
    Validation(#[from] RuntimeServiceError),

    #[error("failed to persist runtime state: {0}")]
    Persist(String),

    #[error("failed to restore runtime state: {0}")]
    Restore(String),

    #[error("failed to apply runtime state: {0}")]
    Apply(String),
}

#[derive(Default)]
struct RuntimeTaskState {
    health_checks: Mutex<HashMap<String, JoinHandle<()>>>,
    drains: Mutex<HashMap<String, JoinHandle<()>>>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct RuntimeStateStatus {
    pub path: Option<String>,
    pub revision: u64,
    pub services: usize,
    pub available: bool,
    pub corrupted: bool,
    pub last_error: Option<String>,
}

/// Shared application state, hot-swappable via ArcSwap.
pub struct AppState {
    pub config: ArcSwap<AppConfig>,
    pub service: ArcSwap<salvo::Service>,
    pub tls_manager: Option<TlsManager>,
    pub shutdown: GracefulShutdown,
    /// Optional path to the config file, used for hot-reload from the admin API.
    pub config_path: Option<String>,
    /// Shared metrics store.
    pub metrics: Arc<Metrics>,
    /// Active runtime backend request/tunnel tracker used for draining.
    pub backend_activity: Arc<BackendActivityTracker>,
    /// Module registry for plugin-provided middleware and handlers.
    pub modules: Arc<ModuleRegistry>,
    /// Runtime-managed services independent from the static config file.
    pub runtime_services: Arc<RuntimeServiceRegistry>,
    runtime_state_write_lock: AsyncMutex<()>,
    runtime_tasks: Arc<RuntimeTaskState>,
    runtime_state_status: Arc<RwLock<RuntimeStateStatus>>,
}

impl AppState {
    pub fn new(config: AppConfig, tls_manager: Option<TlsManager>) -> Arc<Self> {
        let grace_period = config.global.grace_period;
        let metrics = Arc::new(Metrics::new());
        let backend_activity = Arc::new(BackendActivityTracker::default());
        let service = salvo_service::build_service(
            &config,
            &crate::plugin::ModuleRegistry::new(),
            Arc::clone(&metrics),
            Arc::clone(&backend_activity),
        );
        Arc::new(Self {
            config: ArcSwap::from_pointee(config),
            service: ArcSwap::new(Arc::new(service)),
            tls_manager,
            shutdown: GracefulShutdown::new(grace_period),
            config_path: None,
            metrics,
            backend_activity,
            modules: Arc::new(ModuleRegistry::new()),
            runtime_services: Arc::new(RuntimeServiceRegistry::default()),
            runtime_state_write_lock: AsyncMutex::new(()),
            runtime_tasks: Arc::new(RuntimeTaskState::default()),
            runtime_state_status: Arc::new(RwLock::new(RuntimeStateStatus::default())),
        })
    }

    /// Create a new `AppState` with a config file path for hot-reload support.
    pub fn with_config_path(
        config: AppConfig,
        tls_manager: Option<TlsManager>,
        path: String,
    ) -> Arc<Self> {
        let grace_period = config.global.grace_period;
        let metrics = Arc::new(Metrics::new());
        let backend_activity = Arc::new(BackendActivityTracker::default());
        let service = salvo_service::build_service(
            &config,
            &crate::plugin::ModuleRegistry::new(),
            Arc::clone(&metrics),
            Arc::clone(&backend_activity),
        );
        let status_path = path.clone();
        Arc::new(Self {
            config: ArcSwap::from_pointee(config),
            service: ArcSwap::new(Arc::new(service)),
            tls_manager,
            shutdown: GracefulShutdown::new(grace_period),
            config_path: Some(path),
            metrics,
            backend_activity,
            modules: Arc::new(ModuleRegistry::new()),
            runtime_services: Arc::new(RuntimeServiceRegistry::default()),
            runtime_state_write_lock: AsyncMutex::new(()),
            runtime_tasks: Arc::new(RuntimeTaskState::default()),
            runtime_state_status: Arc::new(RwLock::new(RuntimeStateStatus {
                path: Some(status_path),
                ..RuntimeStateStatus::default()
            })),
        })
    }

    /// Create a new `AppState` with a pre-populated module registry.
    pub fn with_modules(
        config: AppConfig,
        tls_manager: Option<TlsManager>,
        modules: ModuleRegistry,
    ) -> Arc<Self> {
        let grace_period = config.global.grace_period;
        let metrics = Arc::new(Metrics::new());
        let backend_activity = Arc::new(BackendActivityTracker::default());
        let modules = Arc::new(modules);
        let service = salvo_service::build_service(
            &config,
            &modules,
            Arc::clone(&metrics),
            Arc::clone(&backend_activity),
        );
        Arc::new(Self {
            config: ArcSwap::from_pointee(config),
            service: ArcSwap::new(Arc::new(service)),
            tls_manager,
            shutdown: GracefulShutdown::new(grace_period),
            config_path: None,
            metrics,
            backend_activity,
            modules,
            runtime_services: Arc::new(RuntimeServiceRegistry::default()),
            runtime_state_write_lock: AsyncMutex::new(()),
            runtime_tasks: Arc::new(RuntimeTaskState::default()),
            runtime_state_status: Arc::new(RwLock::new(RuntimeStateStatus::default())),
        })
    }

    /// Hot-reload: atomically swap in a new config and rebuild the router.
    pub async fn reload(&self, new_config: AppConfig) -> Result<(), RuntimeStateError> {
        let runtime_snapshot = self.runtime_services.snapshot();
        let merged_config = merge_runtime_config(&new_config, &runtime_snapshot)?;
        let new_service = salvo_service::build_service(
            &merged_config,
            &self.modules,
            Arc::clone(&self.metrics),
            Arc::clone(&self.backend_activity),
        );
        self.reload_tls_config(&merged_config).await?;

        self.service.store(Arc::new(new_service));
        self.config.store(Arc::new(new_config));
        info!("configuration reloaded");
        Ok(())
    }

    pub fn runtime_state_path(&self) -> Option<PathBuf> {
        self.config_path.as_ref().map(|config_path| {
            let path = Path::new(config_path);
            let file_name = path
                .file_name()
                .and_then(|file_name| file_name.to_str())
                .unwrap_or("gatel");
            path.with_file_name(format!("{file_name}.runtime.json"))
        })
    }

    pub fn runtime_state_status(&self) -> RuntimeStateStatus {
        self.runtime_state_status
            .read()
            .expect("runtime state status poisoned")
            .clone()
    }

    pub async fn restore_runtime_state(self: &Arc<Self>) -> Result<(), RuntimeStateError> {
        let Some(path) = self.runtime_state_path() else {
            self.set_runtime_state_status(None, false, false, None);
            return Ok(());
        };
        if !path.exists() {
            self.set_runtime_state_status(None, false, false, None);
            return Ok(());
        }

        let payload = std::fs::read_to_string(&path).map_err(|error| {
            let message = error.to_string();
            self.set_runtime_state_status(None, false, false, Some(message.clone()));
            RuntimeStateError::Restore(message)
        })?;
        let snapshot: RuntimeState = serde_json::from_str(&payload).map_err(|error| {
            let message = error.to_string();
            self.set_runtime_state_status(None, false, true, Some(message.clone()));
            RuntimeStateError::Restore(message)
        })?;
        snapshot.ensure_supported_version().map_err(|error| {
            let message = error.to_string();
            self.set_runtime_state_status(None, false, true, Some(message.clone()));
            RuntimeStateError::Validation(error)
        })?;
        if let Err(error) = self.apply_runtime_snapshot(snapshot.clone(), false).await {
            self.set_runtime_state_status(Some(&snapshot), false, false, Some(error.to_string()));
            return Err(error);
        }
        info!(path = %path.display(), "restored runtime service state");
        Ok(())
    }

    pub async fn mutate_runtime_state<T, F>(
        self: &Arc<Self>,
        mutator: F,
    ) -> Result<T, RuntimeStateError>
    where
        F: FnOnce(&mut RuntimeState) -> Result<T, RuntimeServiceError>,
    {
        let _guard = self.runtime_state_write_lock.lock().await;
        let mut snapshot = self.runtime_services.snapshot();
        let result = mutator(&mut snapshot)?;
        self.commit_runtime_snapshot(snapshot).await?;
        Ok(result)
    }

    async fn commit_runtime_snapshot(
        self: &Arc<Self>,
        snapshot: RuntimeState,
    ) -> Result<(), RuntimeStateError> {
        let previous_snapshot = self.runtime_services.snapshot();
        self.persist_runtime_state(&snapshot)?;
        if let Err(error) = self.apply_runtime_snapshot(snapshot.clone(), false).await {
            if let Err(rollback_error) = self.persist_runtime_state(&previous_snapshot) {
                error!(
                    error = %rollback_error,
                    "failed to roll back persisted runtime state after apply failure"
                );
            }
            return Err(error);
        }
        Ok(())
    }

    async fn apply_runtime_snapshot(
        self: &Arc<Self>,
        snapshot: RuntimeState,
        persist: bool,
    ) -> Result<(), RuntimeStateError> {
        let base_config = self.config.load();
        let merged = merge_runtime_config(&base_config, &snapshot)?;
        let new_service = salvo_service::build_service(
            &merged,
            &self.modules,
            Arc::clone(&self.metrics),
            Arc::clone(&self.backend_activity),
        );
        if persist {
            self.persist_runtime_state(&snapshot)?;
        }
        self.reload_tls_config(&merged).await?;
        self.runtime_services.replace(snapshot.clone());
        self.service.store(Arc::new(new_service));
        self.ensure_runtime_tasks();
        self.set_runtime_state_status(Some(&snapshot), true, false, None);
        Ok(())
    }

    fn persist_runtime_state(&self, snapshot: &RuntimeState) -> Result<(), RuntimeStateError> {
        let Some(path) = self.runtime_state_path() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| RuntimeStateError::Persist(error.to_string()))?;
        }

        let payload = serde_json::to_string_pretty(snapshot)
            .map_err(|error| RuntimeStateError::Persist(error.to_string()))?;
        let temp_path = path.with_extension("runtime.tmp");
        std::fs::write(&temp_path, payload)
            .map_err(|error| RuntimeStateError::Persist(error.to_string()))?;
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        std::fs::rename(&temp_path, &path)
            .map_err(|error| RuntimeStateError::Persist(error.to_string()))?;
        Ok(())
    }

    fn set_runtime_state_status(
        &self,
        snapshot: Option<&RuntimeState>,
        available: bool,
        corrupted: bool,
        last_error: Option<String>,
    ) {
        let mut status = self
            .runtime_state_status
            .write()
            .expect("runtime state status poisoned");
        status.path = self
            .runtime_state_path()
            .map(|path| path.display().to_string());
        status.revision = snapshot.map(|snapshot| snapshot.revision).unwrap_or(0);
        status.services = snapshot
            .map(|snapshot| snapshot.services.len())
            .unwrap_or(0);
        status.available = available;
        status.corrupted = corrupted;
        status.last_error = last_error;
    }

    async fn reload_tls_config(&self, config: &AppConfig) -> Result<(), RuntimeStateError> {
        if let Some(ref tls_mgr) = self.tls_manager {
            tls_mgr.reload(config).await.map_err(|error| {
                RuntimeStateError::Apply(format!("failed to reload TLS configuration: {error}"))
            })?;
            return Ok(());
        }
        if config_requires_tls_manager(config) {
            return Err(RuntimeStateError::Apply(
                "TLS changes require the HTTPS listener and TLS manager to be configured at startup"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn ensure_runtime_tasks(self: &Arc<Self>) {
        let snapshot = self.runtime_services.snapshot();
        for service in snapshot.list_services() {
            for route in service.routes {
                for group in route.target_groups {
                    for target in group.targets {
                        match target.state {
                            RuntimeTargetState::Warming => self.schedule_health_check(
                                service.id.clone(),
                                route.id.clone(),
                                group.id.clone(),
                                target.id.clone(),
                            ),
                            RuntimeTargetState::Draining => self.schedule_drain(
                                service.id.clone(),
                                route.id.clone(),
                                group.id.clone(),
                                target.id.clone(),
                                target.drain_timeout,
                            ),
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    fn schedule_health_check(
        self: &Arc<Self>,
        service_id: String,
        route_id: String,
        group_id: String,
        target_id: String,
    ) {
        let key = runtime_target_activity_key(&service_id, &route_id, &group_id, &target_id);
        let mut tasks = self
            .runtime_tasks
            .health_checks
            .lock()
            .expect("runtime task state poisoned");
        if let Some(handle) = tasks.get(&key)
            && !handle.is_finished()
        {
            return;
        }

        let state = Arc::clone(self);
        let cleanup_key = key.clone();
        let handle = tokio::spawn(async move {
            Arc::clone(&state)
                .run_target_health_check(service_id, route_id, group_id, target_id)
                .await;
            state
                .runtime_tasks
                .health_checks
                .lock()
                .expect("runtime task state poisoned")
                .remove(&cleanup_key);
        });
        tasks.insert(key, handle);
    }

    fn schedule_drain(
        self: &Arc<Self>,
        service_id: String,
        route_id: String,
        group_id: String,
        target_id: String,
        drain_timeout: std::time::Duration,
    ) {
        let key = runtime_target_activity_key(&service_id, &route_id, &group_id, &target_id);
        let mut tasks = self
            .runtime_tasks
            .drains
            .lock()
            .expect("runtime task state poisoned");
        if let Some(handle) = tasks.get(&key)
            && !handle.is_finished()
        {
            return;
        }

        let state = Arc::clone(self);
        let cleanup_key = key.clone();
        let activity_key = key.clone();
        let handle = tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + drain_timeout;
            loop {
                let snapshot = state.runtime_services.snapshot();
                let draining = matches!(
                    snapshot.get_target(&service_id, &route_id, &group_id, &target_id),
                    Ok(target) if target.state == RuntimeTargetState::Draining
                );
                if !draining {
                    break;
                }

                let active = state.backend_activity.active(&activity_key);
                let timed_out = tokio::time::Instant::now() >= deadline;
                if active == 0 || timed_out {
                    if timed_out && active > 0 {
                        warn!(
                            service = %service_id,
                            route = %route_id,
                            group = %group_id,
                            target = %target_id,
                            active,
                            "drain timeout expired with active long-lived connections"
                        );
                    }
                    if let Err(error) = state
                        .mutate_runtime_state(|runtime| {
                            runtime.remove_target(
                                &service_id,
                                &route_id,
                                &group_id,
                                &target_id,
                                None,
                            )
                        })
                        .await
                    {
                        warn!(
                            service = %service_id,
                            route = %route_id,
                            group = %group_id,
                            target = %target_id,
                            error = %error,
                            "failed to finalize drained runtime target removal"
                        );
                    }
                    break;
                }

                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            state
                .runtime_tasks
                .drains
                .lock()
                .expect("runtime task state poisoned")
                .remove(&cleanup_key);
        });
        tasks.insert(key, handle);
    }

    async fn run_target_health_check(
        self: Arc<Self>,
        service_id: String,
        route_id: String,
        group_id: String,
        target_id: String,
    ) {
        let mut consecutive_successes = 0u32;
        let mut consecutive_failures = 0u32;

        loop {
            let snapshot = self.runtime_services.snapshot();
            let target = match snapshot.get_target(&service_id, &route_id, &group_id, &target_id) {
                Ok(target) => target,
                Err(_) => return,
            };
            if target.state != RuntimeTargetState::Warming {
                return;
            }

            let health_check = match route_health_check(&snapshot, &service_id, &route_id) {
                Ok(health_check) => health_check,
                Err(_) => return,
            };

            match probe_runtime_target(&target.addr, &health_check).await {
                Ok(()) => {
                    consecutive_successes = consecutive_successes.saturating_add(1);
                    consecutive_failures = 0;
                    if consecutive_successes >= health_check.success_threshold.max(1) {
                        if let Err(error) = self
                            .mutate_runtime_state(|runtime| {
                                runtime.set_target_state(
                                    &service_id,
                                    &route_id,
                                    &group_id,
                                    &target_id,
                                    RuntimeTargetState::Active,
                                    None,
                                )
                            })
                            .await
                        {
                            warn!(
                                service = %service_id,
                                route = %route_id,
                                group = %group_id,
                                target = %target_id,
                                error = %error,
                                "failed to promote warming runtime target"
                            );
                        }
                        return;
                    }
                }
                Err(error_message) => {
                    consecutive_successes = 0;
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    if consecutive_failures >= health_check.failure_threshold.max(1) {
                        if let Err(error) = self
                            .mutate_runtime_state(|runtime| {
                                runtime.set_target_state(
                                    &service_id,
                                    &route_id,
                                    &group_id,
                                    &target_id,
                                    RuntimeTargetState::Failed,
                                    Some(error_message.clone()),
                                )
                            })
                            .await
                        {
                            warn!(
                                service = %service_id,
                                route = %route_id,
                                group = %group_id,
                                target = %target_id,
                                error = %error,
                                "failed to mark warming runtime target as failed"
                            );
                        }
                        return;
                    }
                }
            }

            tokio::time::sleep(health_check.interval).await;
        }
    }
}

fn config_requires_tls_manager(config: &AppConfig) -> bool {
    config.tls.is_some() || config.sites.iter().any(|site| site.tls.is_some())
}

async fn probe_runtime_target(
    target_addr: &str,
    health_check: &RuntimeHealthCheck,
) -> Result<(), String> {
    let url = runtime_health_url(target_addr, health_check)?;
    let client = reqwest::Client::builder()
        .timeout(health_check.timeout)
        .build()
        .map_err(|error| error.to_string())?;

    let mut request = client.get(url);
    if let Some(host) = &health_check.host {
        request = request.header(reqwest::header::HOST, host);
    }

    let response = request.send().await.map_err(|error| error.to_string())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("unexpected status {}", response.status()))
    }
}

fn runtime_health_url(
    target_addr: &str,
    health_check: &RuntimeHealthCheck,
) -> Result<reqwest::Url, String> {
    let base = if target_addr.starts_with("http://") || target_addr.starts_with("https://") {
        target_addr.to_string()
    } else {
        format!("http://{target_addr}")
    };
    let mut url = reqwest::Url::parse(&base).map_err(|error| error.to_string())?;
    if let Some(port) = health_check.port {
        url.set_port(Some(port))
            .map_err(|_| "invalid health check port".to_string())?;
    }
    url.set_path(&health_check.path);
    url.set_query(None);
    Ok(url)
}

/// Start the HTTP (and optionally HTTPS) listener(s) and serve requests.
///
/// When `state.tls_manager` is `Some`, an HTTPS listener is started on the
/// configured `https_addr` alongside the HTTP listener. The HTTP listener
/// doubles as the ACME HTTP-01 challenge responder.
///
/// When `proxy_protocol` is enabled in the global config, the PROXY protocol
/// header is parsed from each connection before serving. The real client
/// address from the header is used instead of the TCP peer address.
pub async fn run(state: Arc<AppState>) -> Result<(), crate::ProxyError> {
    state
        .restore_runtime_state()
        .await
        .map_err(|error| crate::ProxyError::Internal(error.to_string()))?;
    let config = state.config.load();

    // Start the admin API server if configured.
    if let Some(admin_addr) = config.global.admin_addr {
        let admin_state = Arc::clone(&state);
        let admin_metrics = Arc::clone(&state.metrics);
        tokio::spawn(async move {
            if let Err(e) =
                crate::admin::start_admin_server(admin_addr, admin_state, admin_metrics).await
            {
                error!("admin server error: {e}");
            }
        });
    }

    // Start L4 stream proxy listeners if configured.
    if let Some(ref stream_config) = config.stream {
        match crate::stream::start_stream_listeners(stream_config).await {
            Ok(handles) => {
                info!(count = handles.len(), "stream proxy listeners started");
            }
            Err(e) => {
                error!("failed to start stream proxy listeners: {e}");
            }
        }
    }

    let http_addr = config.global.http_addr;
    let proxy_protocol_enabled = config.global.proxy_protocol;

    if proxy_protocol_enabled {
        info!("PROXY protocol support enabled");
    }

    let http_listener = TcpListener::bind(http_addr).await?;
    info!(%http_addr, "listening for HTTP connections");

    // If TLS is configured, start the HTTPS listener concurrently.
    let has_tls = state.tls_manager.is_some();

    if has_tls {
        let https_addr = config.global.https_addr;
        let https_listener = TcpListener::bind(https_addr).await?;
        info!(%https_addr, "listening for HTTPS connections");

        // Start HTTP/3 (QUIC) listener if the feature is enabled and config
        // has http3 turned on.
        #[cfg(feature = "http3")]
        if config.global.http3 {
            let h3_state = Arc::clone(&state);
            let h3_tls = state
                .tls_manager
                .as_ref()
                .expect("TLS manager must exist when has_tls is true")
                .server_config();
            let h3_addr = https_addr;
            tokio::spawn(async move {
                if let Err(e) = h3_server::run_h3_server(h3_addr, h3_tls, h3_state).await {
                    error!("HTTP/3 server error: {e}");
                }
            });
        }

        // Notify systemd that the service is ready (Type=notify).
        crate::sd_notify::sd_notify("READY=1");

        let http_state = Arc::clone(&state);
        let https_state = Arc::clone(&state);

        tokio::select! {
            result = accept_http_loop(http_listener, http_state, proxy_protocol_enabled) => {
                if let Err(e) = result {
                    error!("HTTP listener error: {e}");
                }
            }
            result = accept_https_loop(https_listener, https_state, proxy_protocol_enabled) => {
                if let Err(e) = result {
                    error!("HTTPS listener error: {e}");
                }
            }
        }
    } else {
        // Notify systemd that the service is ready (Type=notify).
        crate::sd_notify::sd_notify("READY=1");

        accept_http_loop(http_listener, state, proxy_protocol_enabled).await?;
    }

    Ok(())
}

/// Accept loop for plain HTTP connections.
async fn accept_http_loop(
    listener: TcpListener,
    state: Arc<AppState>,
    proxy_protocol: bool,
) -> Result<(), crate::ProxyError> {
    let local_addr = listener.local_addr()?;
    loop {
        if state.shutdown.is_shutdown() {
            info!("HTTP accept loop stopping (shutdown)");
            break;
        }

        let (mut stream, peer_addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                error!("HTTP accept error: {e}");
                continue;
            }
        };

        // Apply TCP socket options from global config.
        {
            let cfg = state.config.load();
            if cfg.global.tcp_nodelay {
                stream.set_nodelay(true).ok();
            }
        }

        let state = Arc::clone(&state);
        let _conn_guard = state.shutdown.track_conn();
        tokio::spawn(async move {
            let _guard = _conn_guard;

            if proxy_protocol {
                // Parse PROXY protocol header to get the real client address.
                match proxy_protocol::parse_proxy_protocol(&mut stream).await {
                    Ok((header, prefix)) => {
                        let client_addr = header.as_ref().map(|h| h.src_addr).unwrap_or(peer_addr);
                        let prefixed = PrefixedStream::new(prefix, stream);
                        if let Err(e) =
                            http_server::serve_io(prefixed, local_addr, client_addr, state, false)
                                .await
                        {
                            debug!(client = %client_addr, "HTTP connection error: {e}");
                        }
                    }
                    Err(e) => {
                        debug!(client = %peer_addr, "PROXY protocol parse error: {e}");
                    }
                }
            } else {
                if let Err(e) =
                    http_server::serve_connection(stream, local_addr, peer_addr, state).await
                {
                    debug!(client = %peer_addr, "HTTP connection error: {e}");
                }
            }
        });
    }

    Ok(())
}

/// Accept loop for TLS-wrapped HTTPS connections.
async fn accept_https_loop(
    listener: TcpListener,
    state: Arc<AppState>,
    proxy_protocol: bool,
) -> Result<(), crate::ProxyError> {
    let local_addr = listener.local_addr()?;
    loop {
        if state.shutdown.is_shutdown() {
            info!("HTTPS accept loop stopping (shutdown)");
            break;
        }

        let (mut stream, peer_addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                error!("HTTPS accept error: {e}");
                continue;
            }
        };

        // Apply TCP socket options from global config.
        {
            let cfg = state.config.load();
            if cfg.global.tcp_nodelay {
                stream.set_nodelay(true).ok();
            }
        }

        // Get a TlsAcceptor from the TlsManager. We call acceptor() each
        // iteration so that hot-reloaded TLS configs are picked up.
        let acceptor = match state.tls_manager {
            Some(ref tls_mgr) => tls_mgr.acceptor(),
            None => {
                // Should not happen since we only enter this loop when TLS is
                // configured, but handle gracefully.
                warn!("HTTPS accept loop running without TLS manager");
                continue;
            }
        };

        let state = Arc::clone(&state);
        let _conn_guard = state.shutdown.track_conn();
        tokio::spawn(async move {
            let _guard = _conn_guard;

            if proxy_protocol {
                // Parse PROXY protocol header first, then perform TLS handshake.
                match proxy_protocol::parse_proxy_protocol(&mut stream).await {
                    Ok((header, prefix)) => {
                        let client_addr = header.as_ref().map(|h| h.src_addr).unwrap_or(peer_addr);
                        let prefixed = PrefixedStream::new(prefix, stream);

                        // Perform TLS handshake on the PrefixedStream.
                        let tls_stream = match acceptor.accept(prefixed).await {
                            Ok(tls) => tls,
                            Err(e) => {
                                debug!(client = %client_addr, "TLS handshake failed: {e}");
                                return;
                            }
                        };

                        if let Err(e) =
                            http_server::serve_io(tls_stream, local_addr, client_addr, state, true)
                                .await
                        {
                            debug!(client = %client_addr, "HTTPS connection error: {e}");
                        }
                    }
                    Err(e) => {
                        debug!(client = %peer_addr, "PROXY protocol parse error: {e}");
                    }
                }
            } else {
                // No PROXY protocol — standard TLS handshake.
                let tls_stream = match acceptor.accept(stream).await {
                    Ok(tls) => tls,
                    Err(e) => {
                        debug!(client = %peer_addr, "TLS handshake failed: {e}");
                        return;
                    }
                };

                if let Err(e) =
                    http_server::serve_tls_connection(tls_stream, local_addr, peer_addr, state)
                        .await
                {
                    debug!(client = %peer_addr, "HTTPS connection error: {e}");
                }
            }
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use http::header::{COOKIE, HOST};
    use reqwest::StatusCode;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Notify;

    use super::*;
    use crate::config::LbPolicy;
    use crate::router::matcher::RequestMatcher;
    use crate::runtime_services::{
        RuntimeHealthCheck, RuntimeRoute, RuntimeService, RuntimeTarget, RuntimeTargetGroup,
    };

    fn runtime_service(
        target_addr: String,
        state: RuntimeTargetState,
        drain_timeout: Duration,
    ) -> RuntimeService {
        RuntimeService {
            id: "api".to_string(),
            revision: 0,
            listeners: Vec::new(),
            routes: vec![RuntimeRoute {
                id: "api".to_string(),
                hosts: vec!["example.com".to_string()],
                path_prefix: "/api".to_string(),
                matchers: Vec::new(),
                strip_path_prefix: true,
                lb: crate::config::LbPolicy::WeightedRoundRobin,
                lb_header: None,
                lb_cookie: None,
                response: None,
                timeout_seconds: None,
                target_groups: vec![RuntimeTargetGroup {
                    id: "primary".to_string(),
                    weight: 100,
                    targets: vec![RuntimeTarget {
                        id: "app-1".to_string(),
                        addr: target_addr,
                        weight: 100,
                        state,
                        drain_timeout,
                        last_error: None,
                    }],
                }],
                health_check: RuntimeHealthCheck {
                    interval: Duration::from_millis(25),
                    timeout: Duration::from_secs(1),
                    success_threshold: 1,
                    failure_threshold: 2,
                    ..RuntimeHealthCheck::default()
                },
            }],
            tls: None,
        }
    }

    fn active_target(id: &str, addr: String) -> RuntimeTarget {
        RuntimeTarget {
            id: id.to_string(),
            addr,
            weight: 100,
            state: RuntimeTargetState::Active,
            drain_timeout: Duration::from_secs(1),
            last_error: None,
        }
    }

    async fn spawn_backend(body: &str) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response_body = Arc::new(body.to_string());
        let handle = tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(connection) => connection,
                    Err(_) => break,
                };
                let response_body = Arc::clone(&response_body);
                tokio::spawn(async move {
                    let mut buffer = [0u8; 2048];
                    let _ = stream.read(&mut buffer).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        (format!("127.0.0.1:{}", addr.port()), handle)
    }

    async fn spawn_streaming_backend(
        first_chunk: &str,
        second_chunk: &str,
    ) -> (String, Arc<Notify>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let release = Arc::new(Notify::new());
        let first_chunk = Arc::new(first_chunk.to_string());
        let second_chunk = Arc::new(second_chunk.to_string());
        let handle = {
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                loop {
                    let (mut stream, _) = match listener.accept().await {
                        Ok(connection) => connection,
                        Err(_) => break,
                    };
                    let release = Arc::clone(&release);
                    let first_chunk = Arc::clone(&first_chunk);
                    let second_chunk = Arc::clone(&second_chunk);
                    tokio::spawn(async move {
                        let mut buffer = [0u8; 2048];
                        let _ = stream.read(&mut buffer).await;
                        let response_head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                        let first = format!("{:X}\r\n{}\r\n", first_chunk.len(), first_chunk);
                        let second = format!("{:X}\r\n{}\r\n", second_chunk.len(), second_chunk);
                        let _ = stream.write_all(response_head.as_bytes()).await;
                        let _ = stream.write_all(first.as_bytes()).await;
                        release.notified().await;
                        let _ = stream.write_all(second.as_bytes()).await;
                        let _ = stream.write_all(b"0\r\n\r\n").await;
                    });
                }
            })
        };
        (format!("127.0.0.1:{}", addr.port()), release, handle)
    }

    async fn spawn_websocket_echo_backend() -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request_head = read_http_head(&mut stream).await;
            let request_head = String::from_utf8_lossy(&request_head);
            assert!(request_head.contains("Connection: Upgrade\r\n"));
            assert!(request_head.contains("Upgrade: websocket\r\n"));
            assert!(!request_head.to_ascii_lowercase().contains("x-smuggled"));
            assert!(!request_head.contains("Connection: keep-alive"));

            let response = concat!(
                "HTTP/1.1 101 Switching Protocols\r\n",
                "Upgrade: websocket\r\n",
                "Connection: Upgrade\r\n",
                "Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n",
                "\r\n",
            );
            stream.write_all(response.as_bytes()).await.unwrap();

            let text = read_masked_text_frame(&mut stream).await;
            write_unmasked_text_frame(&mut stream, &text).await;
        });
        (format!("127.0.0.1:{}", addr.port()), handle)
    }

    async fn read_http_head(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut response = Vec::new();
        let mut buffer = [0; 1024];
        loop {
            let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buffer))
                .await
                .unwrap()
                .unwrap();
            assert_ne!(read, 0, "connection closed before HTTP headers completed");
            response.extend_from_slice(&buffer[..read]);
            if response.windows(4).any(|window| window == b"\r\n\r\n") {
                return response;
            }
        }
    }

    async fn write_masked_text_frame(stream: &mut tokio::net::TcpStream, text: &str) {
        let mask = [1, 2, 3, 4];
        let payload = text.as_bytes();
        assert!(payload.len() < 126);
        let mut frame = Vec::with_capacity(6 + payload.len());
        frame.push(0x81);
        frame.push(0x80 | payload.len() as u8);
        frame.extend_from_slice(&mask);
        for (index, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[index % mask.len()]);
        }
        stream.write_all(&frame).await.unwrap();
    }

    async fn read_masked_text_frame(stream: &mut tokio::net::TcpStream) -> String {
        let mut head = [0; 2];
        stream.read_exact(&mut head).await.unwrap();
        assert_eq!(head[0], 0x81);
        assert_eq!(head[1] & 0x80, 0x80);
        let len = (head[1] & 0x7f) as usize;
        assert!(len < 126);
        let mut mask = [0; 4];
        stream.read_exact(&mut mask).await.unwrap();
        let mut payload = vec![0; len];
        stream.read_exact(&mut payload).await.unwrap();
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % mask.len()];
        }
        String::from_utf8(payload).unwrap()
    }

    async fn write_unmasked_text_frame(stream: &mut tokio::net::TcpStream, text: &str) {
        let payload = text.as_bytes();
        assert!(payload.len() < 126);
        stream
            .write_all(&[0x81, payload.len() as u8])
            .await
            .unwrap();
        stream.write_all(payload).await.unwrap();
    }

    async fn read_unmasked_text_frame(stream: &mut tokio::net::TcpStream) -> String {
        let mut head = [0; 2];
        stream.read_exact(&mut head).await.unwrap();
        assert_eq!(head[0], 0x81);
        assert_eq!(head[1] & 0x80, 0);
        let len = (head[1] & 0x7f) as usize;
        assert!(len < 126);
        let mut payload = vec![0; len];
        stream.read_exact(&mut payload).await.unwrap();
        String::from_utf8(payload).unwrap()
    }

    async fn free_http_addr() -> std::net::SocketAddr {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        addr
    }

    async fn spawn_proxy() -> (
        Arc<AppState>,
        std::net::SocketAddr,
        tokio::task::JoinHandle<()>,
    ) {
        let http_addr = free_http_addr().await;
        let mut config = AppConfig::default();
        config.global.http_addr = http_addr;

        let state = AppState::new(config, None);
        let run_state = Arc::clone(&state);
        let handle = tokio::spawn(async move {
            let _ = run(run_state).await;
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}/", http_addr.port());
        for _ in 0..40 {
            if client.get(&url).send().await.is_ok() {
                return (state, http_addr, handle);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("proxy listener did not start");
    }

    async fn proxy_get(
        client: &reqwest::Client,
        http_addr: std::net::SocketAddr,
        path: &str,
        host: &str,
        headers: &[(&http::header::HeaderName, &str)],
    ) -> (StatusCode, String) {
        let mut request = client
            .get(format!("http://127.0.0.1:{}{}", http_addr.port(), path))
            .header(HOST, host);
        for (name, value) in headers {
            request = request.header((*name).clone(), *value);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        (status, body)
    }

    fn split_runtime_service(
        weighted_primary: String,
        weighted_secondary: String,
        header_default: String,
        header_canary: String,
        cookie_default: String,
        cookie_canary: String,
    ) -> RuntimeService {
        RuntimeService {
            id: "split".to_string(),
            revision: 0,
            listeners: Vec::new(),
            routes: vec![
                RuntimeRoute {
                    id: "weighted".to_string(),
                    hosts: Vec::new(),
                    path_prefix: "/weighted".to_string(),
                    matchers: Vec::new(),
                    strip_path_prefix: true,
                    lb: LbPolicy::WeightedRoundRobin,
                    lb_header: None,
                    lb_cookie: None,
                    response: None,
                    timeout_seconds: None,
                    target_groups: vec![
                        RuntimeTargetGroup {
                            id: "primary".to_string(),
                            weight: 1,
                            targets: vec![active_target("weighted-primary", weighted_primary)],
                        },
                        RuntimeTargetGroup {
                            id: "secondary".to_string(),
                            weight: 2,
                            targets: vec![active_target("weighted-secondary", weighted_secondary)],
                        },
                    ],
                    health_check: RuntimeHealthCheck::default(),
                },
                RuntimeRoute {
                    id: "header-canary".to_string(),
                    hosts: Vec::new(),
                    path_prefix: "/header".to_string(),
                    matchers: vec![RequestMatcher::Header {
                        name: "x-deploy".to_string(),
                        pattern: "canary".to_string(),
                    }],
                    strip_path_prefix: true,
                    lb: LbPolicy::WeightedRoundRobin,
                    lb_header: None,
                    lb_cookie: None,
                    response: None,
                    timeout_seconds: None,
                    target_groups: vec![RuntimeTargetGroup {
                        id: "header-canary".to_string(),
                        weight: 1,
                        targets: vec![active_target("header-canary", header_canary)],
                    }],
                    health_check: RuntimeHealthCheck::default(),
                },
                RuntimeRoute {
                    id: "header-default".to_string(),
                    hosts: Vec::new(),
                    path_prefix: "/header".to_string(),
                    matchers: Vec::new(),
                    strip_path_prefix: true,
                    lb: LbPolicy::WeightedRoundRobin,
                    lb_header: None,
                    lb_cookie: None,
                    response: None,
                    timeout_seconds: None,
                    target_groups: vec![RuntimeTargetGroup {
                        id: "header-default".to_string(),
                        weight: 1,
                        targets: vec![active_target("header-default", header_default)],
                    }],
                    health_check: RuntimeHealthCheck::default(),
                },
                RuntimeRoute {
                    id: "cookie-canary".to_string(),
                    hosts: Vec::new(),
                    path_prefix: "/cookie".to_string(),
                    matchers: vec![RequestMatcher::Cookie {
                        name: "deploy".to_string(),
                        pattern: "canary".to_string(),
                    }],
                    strip_path_prefix: true,
                    lb: LbPolicy::WeightedRoundRobin,
                    lb_header: None,
                    lb_cookie: None,
                    response: None,
                    timeout_seconds: None,
                    target_groups: vec![RuntimeTargetGroup {
                        id: "cookie-canary".to_string(),
                        weight: 1,
                        targets: vec![active_target("cookie-canary", cookie_canary)],
                    }],
                    health_check: RuntimeHealthCheck::default(),
                },
                RuntimeRoute {
                    id: "cookie-default".to_string(),
                    hosts: Vec::new(),
                    path_prefix: "/cookie".to_string(),
                    matchers: Vec::new(),
                    strip_path_prefix: true,
                    lb: LbPolicy::WeightedRoundRobin,
                    lb_header: None,
                    lb_cookie: None,
                    response: None,
                    timeout_seconds: None,
                    target_groups: vec![RuntimeTargetGroup {
                        id: "cookie-default".to_string(),
                        weight: 1,
                        targets: vec![active_target("cookie-default", cookie_default)],
                    }],
                    health_check: RuntimeHealthCheck::default(),
                },
            ],
            tls: None,
        }
    }

    #[tokio::test]
    async fn persists_and_restores_runtime_state() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("gatel.kdl");
        let state = AppState::with_config_path(
            AppConfig::default(),
            None,
            config_path.to_string_lossy().to_string(),
        );

        state
            .mutate_runtime_state(|runtime| {
                runtime.upsert_service(
                    runtime_service(
                        "127.0.0.1:3000".to_string(),
                        RuntimeTargetState::Active,
                        Duration::from_secs(1),
                    ),
                    None,
                )
            })
            .await
            .unwrap();

        let runtime_state_path = state.runtime_state_path().unwrap();
        assert!(runtime_state_path.exists());

        let restored = AppState::with_config_path(
            AppConfig::default(),
            None,
            config_path.to_string_lossy().to_string(),
        );
        restored.restore_runtime_state().await.unwrap();

        assert!(restored.runtime_services.get("api").is_some());
    }

    #[tokio::test]
    async fn reports_corrupted_runtime_state_on_restore() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("gatel.kdl");
        let state = AppState::with_config_path(
            AppConfig::default(),
            None,
            config_path.to_string_lossy().to_string(),
        );
        let runtime_state_path = state.runtime_state_path().unwrap();
        std::fs::write(&runtime_state_path, "{not-valid-json").unwrap();

        let error = state.restore_runtime_state().await.unwrap_err();
        assert!(matches!(error, RuntimeStateError::Restore(_)));

        let status = state.runtime_state_status();
        assert!(status.corrupted);
        assert!(!status.available);
        assert!(status.last_error.is_some());
    }

    #[tokio::test]
    async fn warming_target_promotes_to_active_after_health_check() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(connection) => connection,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    let mut buffer = [0u8; 1024];
                    let _ = stream.read(&mut buffer).await;
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .await;
                });
            }
        });

        let state = AppState::new(AppConfig::default(), None);
        state
            .mutate_runtime_state(|runtime| {
                runtime.upsert_service(
                    runtime_service(
                        format!("127.0.0.1:{}", addr.port()),
                        RuntimeTargetState::Warming,
                        Duration::from_secs(1),
                    ),
                    None,
                )
            })
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = state.runtime_services.snapshot();
                let target = snapshot
                    .get_target("api", "api", "primary", "app-1")
                    .expect("target exists");
                if target.state == RuntimeTargetState::Active {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn health_gated_activation_updates_live_proxy_requests() {
        let (backend_addr, backend_handle) = spawn_backend("warming-live").await;
        let (state, http_addr, proxy_handle) = spawn_proxy().await;
        let client = reqwest::Client::new();

        let mut service = runtime_service(
            backend_addr,
            RuntimeTargetState::Warming,
            Duration::from_secs(1),
        );
        service.routes[0].hosts.clear();
        service.routes[0].health_check.interval = Duration::from_millis(150);
        service.routes[0].health_check.success_threshold = 2;

        state
            .mutate_runtime_state(|runtime| runtime.upsert_service(service, None))
            .await
            .unwrap();

        let (status, _) = proxy_get(&client, http_addr, "/api", "example.com", &[]).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let (status, body) =
                    proxy_get(&client, http_addr, "/api", "example.com", &[]).await;
                if status == StatusCode::OK {
                    assert_eq!(body, "warming-live");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();

        state.shutdown.shutdown();
        proxy_handle.abort();
        backend_handle.abort();
    }

    #[tokio::test]
    async fn websocket_proxy_tunnels_split_connection_header_upgrade() {
        let (backend_addr, backend_handle) = spawn_websocket_echo_backend().await;
        let (state, http_addr, proxy_handle) = spawn_proxy().await;

        let mut service = runtime_service(
            backend_addr,
            RuntimeTargetState::Active,
            Duration::from_secs(1),
        );
        service.routes[0].hosts.clear();
        state
            .mutate_runtime_state(|runtime| runtime.upsert_service(service, None))
            .await
            .unwrap();

        let mut stream = tokio::net::TcpStream::connect(http_addr).await.unwrap();
        let request = concat!(
            "GET /api/ws HTTP/1.1\r\n",
            "Host: example.com\r\n",
            "Connection: keep-alive, x-smuggled\r\n",
            "Connection: Upgrade\r\n",
            "Upgrade: websocket\r\n",
            "X-Smuggled: should-not-forward\r\n",
            "Sec-WebSocket-Version: 13\r\n",
            "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
            "\r\n",
        );
        stream.write_all(request.as_bytes()).await.unwrap();

        let response_head = read_http_head(&mut stream).await;
        let response_head = String::from_utf8_lossy(&response_head);
        assert!(
            response_head.starts_with("HTTP/1.1 101"),
            "unexpected websocket handshake response: {response_head}"
        );

        write_masked_text_frame(&mut stream, "proxied websocket").await;
        let echoed = read_unmasked_text_frame(&mut stream).await;
        assert_eq!(echoed, "proxied websocket");

        state.shutdown.shutdown();
        proxy_handle.abort();
        backend_handle.await.unwrap();
    }

    #[tokio::test]
    async fn draining_target_is_removed_after_timeout() {
        let state = AppState::new(AppConfig::default(), None);
        let service = state
            .mutate_runtime_state(|runtime| {
                runtime.upsert_service(
                    runtime_service(
                        "127.0.0.1:3000".to_string(),
                        RuntimeTargetState::Active,
                        Duration::from_millis(50),
                    ),
                    None,
                )
            })
            .await
            .unwrap();

        state
            .mutate_runtime_state(|runtime| {
                runtime.patch_target(
                    "api",
                    "api",
                    "primary",
                    "app-1",
                    crate::runtime_services::RuntimeTargetPatch {
                        state: Some(RuntimeTargetState::Draining),
                        ..crate::runtime_services::RuntimeTargetPatch::default()
                    },
                    Some(service.revision),
                )
            })
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if state
                    .runtime_services
                    .snapshot()
                    .get_target("api", "api", "primary", "app-1")
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn draining_target_stops_new_requests_and_is_removed() {
        let (backend_addr, backend_handle) = spawn_backend("drain-live").await;
        let (state, http_addr, proxy_handle) = spawn_proxy().await;
        let client = reqwest::Client::new();

        let service = runtime_service(
            backend_addr,
            RuntimeTargetState::Active,
            Duration::from_millis(100),
        );
        let mut service = service;
        service.routes[0].hosts.clear();
        let service = state
            .mutate_runtime_state(|runtime| runtime.upsert_service(service, None))
            .await
            .unwrap();

        let (status, body) = proxy_get(&client, http_addr, "/api", "example.com", &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "drain-live");

        state
            .mutate_runtime_state(|runtime| {
                runtime.patch_target(
                    "api",
                    "api",
                    "primary",
                    "app-1",
                    crate::runtime_services::RuntimeTargetPatch {
                        state: Some(RuntimeTargetState::Draining),
                        ..crate::runtime_services::RuntimeTargetPatch::default()
                    },
                    Some(service.revision),
                )
            })
            .await
            .unwrap();

        let (status, _) = proxy_get(&client, http_addr, "/api", "example.com", &[]).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if state
                    .runtime_services
                    .snapshot()
                    .get_target("api", "api", "primary", "app-1")
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();

        state.shutdown.shutdown();
        proxy_handle.abort();
        backend_handle.abort();
    }

    #[tokio::test]
    async fn draining_target_waits_for_stream_completion_before_removal() {
        let (backend_addr, release_stream, backend_handle) =
            spawn_streaming_backend("hello", "-world").await;
        let (state, http_addr, proxy_handle) = spawn_proxy().await;
        let client = reqwest::Client::new();

        let service = runtime_service(
            backend_addr,
            RuntimeTargetState::Active,
            Duration::from_secs(2),
        );
        let service = state
            .mutate_runtime_state(|runtime| runtime.upsert_service(service.clone(), None))
            .await
            .unwrap();

        let response = client
            .get(format!("http://{http_addr}/api"))
            .header(HOST, "example.com")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        state
            .mutate_runtime_state(|runtime| {
                runtime.patch_target(
                    "api",
                    "api",
                    "primary",
                    "app-1",
                    crate::runtime_services::RuntimeTargetPatch {
                        state: Some(RuntimeTargetState::Draining),
                        ..crate::runtime_services::RuntimeTargetPatch::default()
                    },
                    Some(service.revision),
                )
            })
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            state
                .runtime_services
                .snapshot()
                .get_target("api", "api", "primary", "app-1")
                .is_ok()
        );

        release_stream.notify_waiters();
        assert_eq!(response.text().await.unwrap(), "hello-world");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if state
                    .runtime_services
                    .snapshot()
                    .get_target("api", "api", "primary", "app-1")
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();

        state.shutdown.shutdown();
        proxy_handle.abort();
        backend_handle.abort();
    }

    #[tokio::test]
    async fn draining_target_times_out_but_keeps_existing_stream_alive() {
        let (backend_addr, release_stream, backend_handle) =
            spawn_streaming_backend("hello", "-late").await;
        let (state, http_addr, proxy_handle) = spawn_proxy().await;
        let client = reqwest::Client::new();

        let service = runtime_service(
            backend_addr,
            RuntimeTargetState::Active,
            Duration::from_millis(150),
        );
        let service = state
            .mutate_runtime_state(|runtime| runtime.upsert_service(service.clone(), None))
            .await
            .unwrap();

        let response = client
            .get(format!("http://{http_addr}/api"))
            .header(HOST, "example.com")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        state
            .mutate_runtime_state(|runtime| {
                runtime.patch_target(
                    "api",
                    "api",
                    "primary",
                    "app-1",
                    crate::runtime_services::RuntimeTargetPatch {
                        state: Some(RuntimeTargetState::Draining),
                        ..crate::runtime_services::RuntimeTargetPatch::default()
                    },
                    Some(service.revision),
                )
            })
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if state
                    .runtime_services
                    .snapshot()
                    .get_target("api", "api", "primary", "app-1")
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();

        release_stream.notify_waiters();
        assert_eq!(response.text().await.unwrap(), "hello-late");

        state.shutdown.shutdown();
        proxy_handle.abort();
        backend_handle.abort();
    }

    #[tokio::test]
    async fn weighted_split_and_cookie_header_matching_route_live_requests() {
        let (weighted_primary_addr, weighted_primary_handle) =
            spawn_backend("weighted-primary").await;
        let (weighted_secondary_addr, weighted_secondary_handle) =
            spawn_backend("weighted-secondary").await;
        let (header_default_addr, header_default_handle) = spawn_backend("header-default").await;
        let (header_canary_addr, header_canary_handle) = spawn_backend("header-canary").await;
        let (cookie_default_addr, cookie_default_handle) = spawn_backend("cookie-default").await;
        let (cookie_canary_addr, cookie_canary_handle) = spawn_backend("cookie-canary").await;
        let (state, http_addr, proxy_handle) = spawn_proxy().await;
        let client = reqwest::Client::new();

        state
            .mutate_runtime_state(|runtime| {
                runtime.upsert_service(
                    split_runtime_service(
                        weighted_primary_addr,
                        weighted_secondary_addr,
                        header_default_addr,
                        header_canary_addr,
                        cookie_default_addr,
                        cookie_canary_addr,
                    ),
                    None,
                )
            })
            .await
            .unwrap();

        let mut weighted_primary_hits = 0;
        let mut weighted_secondary_hits = 0;
        for _ in 0..6 {
            let (status, body) =
                proxy_get(&client, http_addr, "/weighted", "example.com", &[]).await;
            assert_eq!(status, StatusCode::OK);
            match body.as_str() {
                "weighted-primary" => weighted_primary_hits += 1,
                "weighted-secondary" => weighted_secondary_hits += 1,
                other => panic!("unexpected weighted response: {other}"),
            }
        }
        assert_eq!(weighted_primary_hits, 2);
        assert_eq!(weighted_secondary_hits, 4);

        let (status, body) = proxy_get(
            &client,
            http_addr,
            "/header",
            "example.com",
            &[(&http::header::HeaderName::from_static("x-deploy"), "canary")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "header-canary");

        let (status, body) = proxy_get(&client, http_addr, "/header", "example.com", &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "header-default");

        let (status, body) = proxy_get(
            &client,
            http_addr,
            "/cookie",
            "example.com",
            &[(&COOKIE, "deploy=canary")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "cookie-canary");

        let (status, body) = proxy_get(&client, http_addr, "/cookie", "example.com", &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "cookie-default");

        state.shutdown.shutdown();
        proxy_handle.abort();
        weighted_primary_handle.abort();
        weighted_secondary_handle.abort();
        header_default_handle.abort();
        header_canary_handle.abort();
        cookie_default_handle.abort();
        cookie_canary_handle.abort();
    }

    #[tokio::test]
    async fn mutate_runtime_state_serializes_concurrent_writers() {
        let state = AppState::new(AppConfig::default(), None);
        let guard = state.runtime_state_write_lock.lock().await;
        let started = Arc::new(Notify::new());

        let state_for_task = Arc::clone(&state);
        let started_for_task = Arc::clone(&started);
        let pending_mutation = tokio::spawn(async move {
            started_for_task.notify_one();
            state_for_task
                .mutate_runtime_state(|runtime| {
                    runtime.upsert_service(
                        runtime_service(
                            "127.0.0.1:3000".to_string(),
                            RuntimeTargetState::Active,
                            Duration::from_secs(1),
                        ),
                        None,
                    )
                })
                .await
        });

        started.notified().await;
        tokio::task::yield_now().await;
        assert!(state.runtime_services.get("api").is_none());

        drop(guard);

        let service = tokio::time::timeout(Duration::from_secs(1), pending_mutation)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(service.revision, 1);
        assert_eq!(state.runtime_services.get("api").unwrap().revision, 1);
    }
}
