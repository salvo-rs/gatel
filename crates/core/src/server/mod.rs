pub mod graceful;
#[cfg(feature = "http3")]
pub mod h3_server;
pub mod http_server;
pub mod proxy_protocol;

use std::sync::Arc;

use arc_swap::ArcSwap;
pub use graceful::GracefulShutdown;
pub use proxy_protocol::{PrefixedStream, ProxyProtocolHeader};
use tokio::net::TcpListener;
use tracing::{debug, error, info, warn};

use crate::config::AppConfig;
use crate::hoops::metrics::Metrics;
use crate::plugin::ModuleRegistry;
use crate::salvo_service;
use crate::tls::TlsManager;

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
    /// Module registry for plugin-provided middleware and handlers.
    pub modules: Arc<ModuleRegistry>,
}

impl AppState {
    pub fn new(config: AppConfig, tls_manager: Option<TlsManager>) -> Arc<Self> {
        let grace_period = config.global.grace_period;
        let service = salvo_service::build_service(&config, &crate::plugin::ModuleRegistry::new());
        Arc::new(Self {
            config: ArcSwap::from_pointee(config),
            service: ArcSwap::new(Arc::new(service)),
            tls_manager,
            shutdown: GracefulShutdown::new(grace_period),
            config_path: None,
            metrics: Arc::new(Metrics::new()),
            modules: Arc::new(ModuleRegistry::new()),
        })
    }

    /// Create a new `AppState` with a config file path for hot-reload support.
    pub fn with_config_path(
        config: AppConfig,
        tls_manager: Option<TlsManager>,
        path: String,
    ) -> Arc<Self> {
        let grace_period = config.global.grace_period;
        let service = salvo_service::build_service(&config, &crate::plugin::ModuleRegistry::new());
        Arc::new(Self {
            config: ArcSwap::from_pointee(config),
            service: ArcSwap::new(Arc::new(service)),
            tls_manager,
            shutdown: GracefulShutdown::new(grace_period),
            config_path: Some(path),
            metrics: Arc::new(Metrics::new()),
            modules: Arc::new(ModuleRegistry::new()),
        })
    }

    /// Create a new `AppState` with a pre-populated module registry.
    pub fn with_modules(
        config: AppConfig,
        tls_manager: Option<TlsManager>,
        modules: ModuleRegistry,
    ) -> Arc<Self> {
        let grace_period = config.global.grace_period;
        let modules = Arc::new(modules);
        let service = salvo_service::build_service(&config, &modules);
        Arc::new(Self {
            config: ArcSwap::from_pointee(config),
            service: ArcSwap::new(Arc::new(service)),
            tls_manager,
            shutdown: GracefulShutdown::new(grace_period),
            config_path: None,
            metrics: Arc::new(Metrics::new()),
            modules,
        })
    }

    /// Hot-reload: atomically swap in a new config and rebuild the router.
    pub async fn reload(&self, new_config: AppConfig) {
        // Reload TLS configuration if a TlsManager is present.
        if let Some(ref tls_mgr) = self.tls_manager
            && let Err(e) = tls_mgr.reload(&new_config).await
        {
            error!("failed to reload TLS configuration: {e}");
            // Continue with the rest of the reload; the old TLS config
            // remains active via ArcSwap.
        }

        let new_service = salvo_service::build_service(&new_config, &self.modules);
        self.service.store(Arc::new(new_service));
        self.config.store(Arc::new(new_config));
        info!("configuration reloaded");
    }
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
