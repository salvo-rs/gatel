//! Native Windows Service support.
//!
//! Implements the Windows Service Control Manager (SCM) protocol so that
//! gatel can run as a proper Windows service with graceful Start/Stop.

use std::ffi::OsString;
use std::sync::OnceLock;
use std::time::Duration;

use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

const SERVICE_NAME: &str = "gatel";

/// Config path passed from main() → service_main() via static.
static CONFIG_PATH: OnceLock<String> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

// ---------------------------------------------------------------------------
// Service dispatcher (called from main before tokio runtime)
// ---------------------------------------------------------------------------

/// Dispatch to the Windows Service Control Manager.
///
/// Must be called from the main thread **before** any tokio runtime is created.
/// Blocks until the service is stopped.
pub fn dispatch(config_path: String) -> anyhow::Result<()> {
    let _ = CONFIG_PATH.set(config_path);
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .map_err(|e| anyhow::anyhow!("service dispatcher failed: {e}"))
}

// ---------------------------------------------------------------------------
// Service entry point (called by SCM on a service thread)
// ---------------------------------------------------------------------------

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service() {
        // No console in service mode — write to a temp file for diagnostics.
        let _ = std::fs::write(
            std::env::temp_dir().join("gatel-service-error.log"),
            format!("{e:?}"),
        );
    }
}

fn run_service() -> anyhow::Result<()> {
    use std::sync::Arc;

    // Channel for the control handler to signal shutdown.
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    // Register the service control handler.
    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = shutdown_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
        .map_err(|e| anyhow::anyhow!("failed to register service control handler: {e}"))?;

    // Helper to report status to SCM.
    let report = |state: ServiceState, accept: ServiceControlAccept, wait: Duration| {
        let _ = status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: wait,
            process_id: None,
        });
    };

    report(
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
        Duration::from_secs(10),
    );

    let config_path = CONFIG_PATH.get().map(|s| s.as_str()).unwrap_or("gatel.kdl");

    // Build a dedicated tokio runtime for the service.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build tokio runtime: {e}"))?;

    rt.block_on(async {
        use gatel_core::config::parse_config;
        use gatel_core::server::{self, AppState};
        use gatel_core::tls::TlsManager;
        use tracing::{error, info};

        let config_str = std::fs::read_to_string(config_path)
            .map_err(|e| anyhow::anyhow!("failed to read config: {e}"))?;
        let config = parse_config(&config_str)?;

        super::init_tracing(
            &config.global.log_level,
            &config.global.log_format,
            config.global.otlp_endpoint.as_deref(),
            config.global.otlp_service_name.as_deref(),
        );

        info!("gatel starting as Windows service");

        let tls_manager = if config.tls.is_some() {
            match TlsManager::build(&config).await {
                Ok(mgr) => Some(mgr),
                Err(e) => {
                    error!("TLS initialization failed: {e}");
                    return Err(anyhow::anyhow!("TLS initialization failed: {e}"));
                }
            }
        } else {
            None
        };

        let state = AppState::with_config_path(config, tls_manager, config_path.to_string());

        // Report Running to SCM.
        report(
            ServiceState::Running,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            Duration::default(),
        );

        let shutdown_state = Arc::clone(&state);

        // Run until either the server exits or SCM sends Stop.
        tokio::select! {
            result = server::run(state) => {
                if let Err(e) = result {
                    error!("server error: {e}");
                }
            }
            _ = shutdown_rx.recv() => {
                info!("service stop requested, shutting down");
                report(
                    ServiceState::StopPending,
                    ServiceControlAccept::empty(),
                    Duration::from_secs(30),
                );
                shutdown_state.shutdown.shutdown();
                if let Some(ref tls_mgr) = shutdown_state.tls_manager {
                    tls_mgr.stop_maintenance();
                }
                let _ = shutdown_state.shutdown.drain().await;
                info!("graceful shutdown complete");
            }
        }

        report(
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
            Duration::default(),
        );

        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Install / Uninstall
// ---------------------------------------------------------------------------

/// Register gatel as a Windows service.
pub fn install_service(config_path: &str) -> anyhow::Result<()> {
    let config_abs = std::fs::canonicalize(config_path)
        .map_err(|e| anyhow::anyhow!("cannot resolve config path '{config_path}': {e}"))?;
    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("cannot determine executable path: {e}"))?;

    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|e| {
        anyhow::anyhow!("failed to connect to Service Manager (run as Administrator): {e}")
    })?;

    let service_info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from("Gatel Reverse Proxy"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: vec![
            OsString::from("service"),
            OsString::from("run"),
            OsString::from("--config"),
            config_abs.as_os_str().to_owned(),
        ],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };

    let service = manager
        .create_service(&service_info, ServiceAccess::CHANGE_CONFIG)
        .map_err(|e| anyhow::anyhow!("failed to create service: {e}"))?;

    service
        .set_description("Gatel - High-performance reverse proxy and web server")
        .map_err(|e| anyhow::anyhow!("failed to set description: {e}"))?;

    println!("Service '{SERVICE_NAME}' installed.");
    println!("  Config: {}", config_abs.display());
    println!("  Start:  sc start {SERVICE_NAME}");
    println!("  Stop:   sc stop {SERVICE_NAME}");
    Ok(())
}

/// Remove the gatel Windows service.
pub fn uninstall_service() -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| {
            anyhow::anyhow!("failed to connect to Service Manager (run as Administrator): {e}")
        })?;

    let service = manager
        .open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::DELETE)
        .map_err(|e| anyhow::anyhow!("failed to open service: {e}"))?;

    // Try to stop first (ignore errors if already stopped).
    let _ = service.stop();

    service
        .delete()
        .map_err(|e| anyhow::anyhow!("failed to delete service: {e}"))?;

    println!("Service '{SERVICE_NAME}' uninstalled.");
    Ok(())
}
