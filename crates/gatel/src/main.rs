#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod cli;

use std::sync::Arc;

use clap::Parser;
use gatel_core::config::{auto_config_from_env, parse_config};
use gatel_core::server::{self, AppState};
use gatel_core::tls::TlsManager;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    match cli.command {
        cli::Commands::Run {
            config: config_path,
        } => {
            // If the config file does not exist but environment variables are
            // set, use the auto-generated config instead of failing.
            let config_str = if !std::path::Path::new(&config_path).exists() {
                match auto_config_from_env() {
                    Some(auto_cfg) => {
                        info!("config file not found; using auto-config from environment");
                        auto_cfg
                    }
                    None => {
                        return Err(anyhow::anyhow!(
                            "config file '{}' not found and no GATEL_* environment \
                             variables are set",
                            config_path
                        ));
                    }
                }
            } else {
                std::fs::read_to_string(&config_path)
                    .map_err(|e| anyhow::anyhow!("failed to read config {config_path}: {e}"))?
            };
            let config = parse_config(&config_str)?;

            // Initialize tracing
            init_tracing(
                &config.global.log_level,
                &config.global.log_format,
                config.global.otlp_endpoint.as_deref(),
                config.global.otlp_service_name.as_deref(),
            );

            info!("gatel starting");
            let rt = gatel_core::runtime::info();
            info!(
                runtime = rt.name,
                io_uring = rt.io_uring,
                "runtime initialized"
            );
            info!(
                sites = config.sites.len(),
                http = %config.global.http_addr,
                "configuration loaded"
            );

            // Build TlsManager if TLS is configured.
            let tls_manager = if config.tls.is_some() {
                match TlsManager::build(&config).await {
                    Ok(mgr) => {
                        info!(
                            https = %config.global.https_addr,
                            "TLS configured"
                        );
                        Some(mgr)
                    }
                    Err(e) => {
                        error!("failed to initialize TLS: {e}");
                        return Err(anyhow::anyhow!("TLS initialization failed: {e}"));
                    }
                }
            } else {
                None
            };

            let state = AppState::with_config_path(config, tls_manager, config_path.clone());

            // Keep a reference for the signal handler.
            let signal_state = Arc::clone(&state);
            let config_path_owned = config_path.clone();

            // Spawn signal handler task.
            tokio::spawn(async move {
                signal_handler(signal_state, config_path_owned).await;
            });

            server::run(state).await?;
        }
        cli::Commands::Validate {
            config: config_path,
        } => {
            let config_str = std::fs::read_to_string(&config_path)
                .map_err(|e| anyhow::anyhow!("failed to read config {config_path}: {e}"))?;
            match parse_config(&config_str) {
                Ok(config) => {
                    println!(
                        "Configuration is valid ({} site(s), {} route(s))",
                        config.sites.len(),
                        config.sites.iter().map(|s| s.routes.len()).sum::<usize>()
                    );
                }
                Err(e) => {
                    eprintln!("Configuration error: {e}");
                    std::process::exit(1);
                }
            }
        }
        cli::Commands::Reload => {
            // Phase 5: send SIGHUP to the running process
            eprintln!("reload not yet implemented");
            std::process::exit(1);
        }
        cli::Commands::Serve {
            root,
            port,
            listen,
            browse,
        } => {
            init_tracing("info", "pretty", None, None);

            // Build a synthetic config
            let browse_str = if browse { " browse=true" } else { "" };
            let config_str = format!(
                r#"
global {{
    http "{listen}:{port}"
}}
site "*" {{
    route "/*" {{
        root "{root}"
        file-server{browse_str}
    }}
}}
"#
            );
            let config = parse_config(&config_str)?;
            info!("serving {} on {}:{}", root, listen, port);
            let rt = gatel_core::runtime::info();
            info!(
                runtime = rt.name,
                io_uring = rt.io_uring,
                "runtime initialized"
            );
            let state = AppState::new(config, None);
            server::run(state).await?;
        }
        cli::Commands::Completions { shell } => {
            cli::generate_completions(shell);
        }
        cli::Commands::ManPage => {
            cli::generate_man_page()?;
        }
    }

    Ok(())
}

/// Handle OS signals for shutdown and reload.
///
/// - SIGTERM / SIGINT / Ctrl+C: initiate graceful shutdown.
/// - SIGHUP (Unix only): hot-reload configuration.
async fn signal_handler(state: Arc<AppState>, #[allow(unused)] config_path: String) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
        let mut sighup = signal(SignalKind::hangup()).expect("failed to register SIGHUP handler");

        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("received SIGINT, initiating graceful shutdown");
                    initiate_shutdown(&state).await;
                    return;
                }
                _ = sigterm.recv() => {
                    info!("received SIGTERM, initiating graceful shutdown");
                    initiate_shutdown(&state).await;
                    return;
                }
                _ = sighup.recv() => {
                    info!("received SIGHUP, reloading configuration");
                    reload_config(&state, &config_path).await;
                }
            }
        }
    }

    #[cfg(not(unix))]
    {
        // On Windows, only Ctrl+C is available.
        if let Err(e) = tokio::signal::ctrl_c().await {
            error!("failed to listen for Ctrl+C: {e}");
            return;
        }
        info!("received Ctrl+C, initiating graceful shutdown");
        initiate_shutdown(&state).await;
    }
}

/// Initiate graceful shutdown: signal the accept loops to stop, then
/// drain active connections.
async fn initiate_shutdown(state: &AppState) {
    state.shutdown.shutdown();

    // Stop TLS maintenance loop.
    if let Some(ref tls_mgr) = state.tls_manager {
        tls_mgr.stop_maintenance();
    }

    let drained = state.shutdown.drain().await;
    if drained {
        info!("graceful shutdown complete");
    } else {
        warn!("graceful shutdown timed out, exiting");
    }

    // Exit the process after shutdown.
    std::process::exit(0);
}

/// Hot-reload configuration from disk.
#[allow(dead_code)]
async fn reload_config(state: &AppState, config_path: &str) {
    let config_str = match std::fs::read_to_string(config_path) {
        Ok(s) => s,
        Err(e) => {
            error!("failed to read config file for reload: {e}");
            return;
        }
    };

    let new_config = match parse_config(&config_str) {
        Ok(c) => c,
        Err(e) => {
            error!("invalid configuration on reload: {e}");
            return;
        }
    };

    state.reload(new_config).await;
}

fn init_tracing(
    level: &str,
    format: &str,
    #[allow(unused_variables)] otlp_endpoint: Option<&str>,
    #[allow(unused_variables)] otlp_service_name: Option<&str>,
) {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    #[cfg(feature = "otlp")]
    if let Some(endpoint) = otlp_endpoint {
        use opentelemetry_otlp::WithExportConfig;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;

        let service = otlp_service_name.unwrap_or("gatel");

        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()
            .expect("failed to build OTLP exporter");

        let resource = opentelemetry_sdk::Resource::builder()
            .with_attribute(opentelemetry::KeyValue::new(
                "service.name",
                service.to_string(),
            ))
            .build();

        let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_resource(resource)
            .with_batch_exporter(exporter)
            .build();

        use opentelemetry::trace::TracerProvider as _;
        opentelemetry::global::set_tracer_provider(tracer_provider.clone());

        match format {
            "json" => {
                let telemetry_layer =
                    tracing_opentelemetry::layer().with_tracer(tracer_provider.tracer("gatel"));
                tracing_subscriber::registry()
                    .with(filter)
                    .with(tracing_subscriber::fmt::layer().json())
                    .with(telemetry_layer)
                    .init();
            }
            _ => {
                let telemetry_layer =
                    tracing_opentelemetry::layer().with_tracer(tracer_provider.tracer("gatel"));
                tracing_subscriber::registry()
                    .with(filter)
                    .with(tracing_subscriber::fmt::layer())
                    .with(telemetry_layer)
                    .init();
            }
        }
        return;
    }

    // Non-OTLP path (or OTLP feature not enabled).
    match format {
        "json" => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .init();
        }
        _ => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
    }
}
