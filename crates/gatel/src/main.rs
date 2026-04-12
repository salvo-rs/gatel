#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod cli;
#[cfg(windows)]
mod win_service;

use std::sync::Arc;

use clap::Parser;
use gatel_core::config::{GlobalConfig, auto_config_from_env, parse_config, parse_config_file};
use gatel_core::server::{self, AppState};
use gatel_core::tls::TlsManager;
use tracing::{error, info, warn};

fn config_has_tls(config: &gatel_core::config::AppConfig) -> bool {
    config.tls.is_some() || config.sites.iter().any(|site| site.tls.is_some())
}

fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    // Windows service dispatcher must run before the tokio runtime is created.
    #[cfg(windows)]
    {
        if let cli::Commands::Service {
            action: cli::ServiceAction::Run { ref config },
        } = cli.command
        {
            return win_service::dispatch(config.clone());
        }
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main(cli))
}

async fn async_main(cli: cli::Cli) -> anyhow::Result<()> {
    match cli.command {
        cli::Commands::Run {
            config: config_path,
        } => {
            // If the config file does not exist but environment variables are
            // set, use the auto-generated config instead of failing.  Otherwise
            // load the file via `parse_config_file` so `import "..."` directives
            // are resolved relative to the main config's directory.
            let config = if !std::path::Path::new(&config_path).exists() {
                match auto_config_from_env() {
                    Some(auto_cfg) => {
                        info!("config file not found; using auto-config from environment");
                        parse_config(&auto_cfg)?
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
                parse_config_file(&config_path)?
            };

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
            let tls_manager = if config_has_tls(&config) {
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
        } => match parse_config_file(&config_path) {
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
        },
        cli::Commands::Reload {
            config: config_path,
            address,
        } => {
            // Determine admin address (and optional auth token) from flag or config.
            let (admin_addr, auth_token) = if let Some(addr) = address {
                (addr, None)
            } else {
                let config = parse_config_file(&config_path)?;
                match config.global.admin_addr {
                    Some(addr) => (
                        addr.to_string(),
                        reload_auth_token(&config.global).map(str::to_owned),
                    ),
                    None => {
                        return Err(anyhow::anyhow!(
                            "admin API not configured in '{config_path}'; \
                             set 'admin' in the global block or use --address"
                        ));
                    }
                }
            };

            match admin_reload_request(&admin_addr, auth_token.as_deref()) {
                Ok(body) => {
                    println!("Configuration reloaded: {body}");
                }
                Err(e) => {
                    eprintln!("Reload failed: {e}");
                    std::process::exit(1);
                }
            }
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
        #[cfg(windows)]
        cli::Commands::Service { action } => {
            // Only Install and Uninstall reach here; Run is handled in main().
            match action {
                cli::ServiceAction::Install { config } => {
                    win_service::install_service(&config)?;
                }
                cli::ServiceAction::Uninstall => {
                    win_service::uninstall_service()?;
                }
                cli::ServiceAction::Run { .. } => unreachable!(),
            }
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
    gatel_core::sd_notify::sd_notify("STOPPING=1");
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
    gatel_core::sd_notify::sd_notify("RELOADING=1");

    let new_config = match parse_config_file(config_path) {
        Ok(c) => c,
        Err(e) => {
            error!("failed to reload config from '{config_path}': {e}");
            gatel_core::sd_notify::sd_notify("READY=1");
            return;
        }
    };

    if let Err(e) = state.reload(new_config).await {
        error!("failed to apply reloaded configuration: {e}");
    }
    gatel_core::sd_notify::sd_notify("READY=1");
}

/// Send a POST /config/reload request to the admin API.
fn admin_reload_request(addr: &str, auth_token: Option<&str>) -> Result<String, anyhow::Error> {
    use std::io::{Read, Write};

    let mut stream = std::net::TcpStream::connect(addr)
        .map_err(|e| anyhow::anyhow!("failed to connect to admin API at {addr}: {e}"))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;

    let auth_header = match auth_token {
        Some(token) => format!("Authorization: Bearer {token}\r\n"),
        None => String::new(),
    };

    let request = format!(
        "POST /config/reload HTTP/1.1\r\n\
         Host: {addr}\r\n\
         {auth_header}\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         \r\n"
    );
    stream.write_all(request.as_bytes())?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;

    // Extract status code and body from HTTP response.
    let status_line = response.lines().next().unwrap_or("");
    let status_ok = status_line.contains("200");

    // Extract JSON body (after the blank line separating headers from body).
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.trim())
        .unwrap_or(&response);

    if status_ok {
        Ok(body.to_string())
    } else {
        Err(anyhow::anyhow!("{body}"))
    }
}

fn reload_auth_token(global: &GlobalConfig) -> Option<&str> {
    global
        .admin_auth_token
        .as_deref()
        .or(global.admin_write_token.as_deref())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_auth_token_prefers_admin_auth_token() {
        let global = GlobalConfig {
            admin_auth_token: Some("admin-token".to_string()),
            admin_write_token: Some("write-token".to_string()),
            ..GlobalConfig::default()
        };

        assert_eq!(reload_auth_token(&global), Some("admin-token"));
    }

    #[test]
    fn reload_auth_token_falls_back_to_write_token() {
        let global = GlobalConfig {
            admin_write_token: Some("write-token".to_string()),
            ..GlobalConfig::default()
        };

        assert_eq!(reload_auth_token(&global), Some("write-token"));
    }
}
