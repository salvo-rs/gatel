//! Admin REST API server.
//!
//! Provides a lightweight HTTP API for inspecting and managing the running
//! proxy. Runs on a separate listener (configured via `global.admin_addr`)
//! and exposes endpoints for health, configuration, upstream status, metrics,
//! and hot-reload.

use std::net::SocketAddr;
use std::sync::Arc;

use http::{Request, Response, StatusCode};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::config::parse_config;
use crate::hoops::metrics::Metrics;
use crate::server::AppState;
use crate::{Body, ProxyError, full_body};

/// Start the admin HTTP server on the given address.
///
/// The server runs indefinitely and handles requests against the shared
/// application state. It should be spawned as a background task alongside
/// the main proxy listeners.
pub async fn start_admin_server(
    addr: SocketAddr,
    state: Arc<AppState>,
    metrics: Arc<Metrics>,
) -> Result<(), ProxyError> {
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "admin API server listening");

    loop {
        let (stream, _client_addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                error!("admin accept error: {e}");
                continue;
            }
        };

        let state = Arc::clone(&state);
        let metrics = Arc::clone(&metrics);

        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req: Request<Incoming>| {
                let state = Arc::clone(&state);
                let metrics = Arc::clone(&metrics);
                async move { handle_admin_request(req, &state, &metrics).await }
            });

            if let Err(e) =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .http1()
                    .timer(TokioTimer::new())
                    .serve_connection(io, service)
                    .await
            {
                error!("admin connection error: {e}");
            }
        });
    }
}

/// Route an admin request to the appropriate handler.
async fn handle_admin_request(
    req: Request<Incoming>,
    state: &AppState,
    metrics: &Metrics,
) -> Result<Response<Body>, std::convert::Infallible> {
    // Enforce bearer token auth if configured.
    let config = state.config.load();
    if let Some(ref token) = config.global.admin_auth_token
        && !check_bearer_auth(&req, token)
    {
        return Ok(json_response(
            StatusCode::UNAUTHORIZED,
            r#"{"error":"unauthorized"}"#,
        ));
    }

    let path = req.uri().path().to_string();
    let method = req.method().clone();

    let result = match (method.as_str(), path.as_str()) {
        ("GET", "/config") => handle_get_config(state),
        ("POST", "/config/reload") => handle_config_reload(state).await,
        ("POST", "/config") => handle_post_config(req, state).await,
        ("POST", "/config/test") => handle_test_config(req).await,
        ("POST", "/stop") => handle_stop(state),
        ("GET", "/health") => handle_health(),
        ("GET", "/upstreams") => handle_upstreams(state),
        ("GET", "/metrics") => handle_metrics(metrics),
        ("GET", "/runtime") => handle_runtime(),
        _ => json_response(StatusCode::NOT_FOUND, r#"{"error":"not found"}"#),
    };

    Ok(result)
}

// ---------------------------------------------------------------------------
// Endpoint handlers
// ---------------------------------------------------------------------------

/// `GET /config` — return the current configuration as JSON.
fn handle_get_config(state: &AppState) -> Response<Body> {
    let config = state.config.load();
    match serde_json::to_string_pretty(&**config) {
        Ok(json) => Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(full_body(json))
            .unwrap(),
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!(r#"{{"error":"serialization failed: {e}"}}"#),
        ),
    }
}

/// `POST /config/reload` — trigger a hot-reload from the config file.
///
/// Currently delegates to `AppState::reload` using the path stored in the
/// existing config. In practice, the caller (main binary) would re-read the
/// config file and call `state.reload(new_config)`. Here we just signal
/// success to confirm the endpoint is wired up; the actual file re-read is
/// done by the caller that owns the config path.
async fn handle_config_reload(state: &AppState) -> Response<Body> {
    // Read the config path from global state — the admin API re-reads the
    // file via `parse_config_file` so any `import` directives are resolved.
    if let Some(ref config_path) = state.config_path {
        match crate::config::parse_config_file(config_path) {
            Ok(new_config) => {
                state.reload(new_config).await;
                json_response(StatusCode::OK, r#"{"status":"reloaded"}"#)
            }
            Err(e) => json_response(
                StatusCode::BAD_REQUEST,
                &format!(r#"{{"error":"config reload failed: {e}"}}"#),
            ),
        }
    } else {
        json_response(
            StatusCode::OK,
            r#"{"status":"reload_requested","note":"no config path set; reload must be triggered externally"}"#,
        )
    }
}

/// `GET /health` — simple health check endpoint.
fn handle_health() -> Response<Body> {
    json_response(StatusCode::OK, r#"{"status":"healthy"}"#)
}

/// `GET /upstreams` — return upstream backend status.
///
/// Iterates over all sites and their proxy routes to collect upstream info
/// from the current configuration. This shows the configured addresses,
/// health status, and active connections from the config perspective.
fn handle_upstreams(state: &AppState) -> Response<Body> {
    let config = state.config.load();
    let mut upstreams = Vec::new();

    for site in &config.sites {
        for route in &site.routes {
            if let crate::config::HandlerConfig::Proxy(ref proxy_cfg) = route.handler {
                for upstream in &proxy_cfg.upstreams {
                    upstreams.push(serde_json::json!({
                        "site": site.host,
                        "route": route.path,
                        "address": upstream.addr,
                        "weight": upstream.weight,
                    }));
                }
            }
        }
    }

    let json = serde_json::to_string_pretty(&upstreams).unwrap_or_else(|_| "[]".to_string());
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(full_body(json))
        .unwrap()
}

/// `GET /runtime` — return runtime information.
fn handle_runtime() -> Response<Body> {
    let rt = crate::runtime::info();
    let json = format!(
        r#"{{"runtime":"{}","io_uring":{},"send_tasks":{}}}"#,
        rt.name, rt.io_uring, rt.send_tasks
    );
    json_response(StatusCode::OK, &json)
}

/// `GET /metrics` — return Prometheus-format metrics.
fn handle_metrics(metrics: &Metrics) -> Response<Body> {
    let output = metrics.render_prometheus();
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
        .body(full_body(output))
        .unwrap()
}

/// `POST /config` — apply a new KDL configuration from the request body.
async fn handle_post_config(req: Request<Incoming>, state: &AppState) -> Response<Body> {
    let body_bytes = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &format!(r#"{{"error":"failed to read body: {e}"}}"#),
            );
        }
    };

    let config_str = match std::str::from_utf8(&body_bytes) {
        Ok(s) => s,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &format!(r#"{{"error":"invalid UTF-8: {e}"}}"#),
            );
        }
    };

    match parse_config(config_str) {
        Ok(new_config) => {
            let sites = new_config.sites.len();
            let routes: usize = new_config.sites.iter().map(|s| s.routes.len()).sum();
            state.reload(new_config).await;
            info!("config applied via admin API");
            json_response(
                StatusCode::OK,
                &format!(r#"{{"status":"applied","sites":{sites},"routes":{routes}}}"#),
            )
        }
        Err(e) => json_response(
            StatusCode::BAD_REQUEST,
            &format!(r#"{{"error":"config parse failed: {e}"}}"#),
        ),
    }
}

/// `POST /config/test` — validate a KDL configuration without applying it.
async fn handle_test_config(req: Request<Incoming>) -> Response<Body> {
    let body_bytes = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &format!(r#"{{"error":"failed to read body: {e}"}}"#),
            );
        }
    };

    let config_str = match std::str::from_utf8(&body_bytes) {
        Ok(s) => s,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &format!(r#"{{"error":"invalid UTF-8: {e}"}}"#),
            );
        }
    };

    match parse_config(config_str) {
        Ok(config) => {
            let sites = config.sites.len();
            let routes: usize = config.sites.iter().map(|s| s.routes.len()).sum();
            json_response(
                StatusCode::OK,
                &format!(r#"{{"status":"valid","sites":{sites},"routes":{routes}}}"#),
            )
        }
        Err(e) => json_response(StatusCode::BAD_REQUEST, &format!(r#"{{"error":"{e}"}}"#)),
    }
}

/// `POST /stop` — initiate graceful shutdown.
fn handle_stop(state: &AppState) -> Response<Body> {
    warn!("shutdown requested via admin API");
    state.shutdown.shutdown();
    json_response(StatusCode::OK, r#"{"status":"stopping"}"#)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a simple JSON response with the given status and body string.
fn json_response(status: StatusCode, body: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(full_body(body.to_string()))
        .unwrap()
}

/// Check for a valid `Authorization: Bearer <token>` header.
fn check_bearer_auth(req: &Request<Incoming>, expected_token: &str) -> bool {
    req.headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.starts_with("Bearer ")
                && crate::crypto::constant_time_eq(&v.as_bytes()[7..], expected_token.as_bytes())
        })
        .unwrap_or(false)
}
