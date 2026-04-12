//! Admin REST API server.
//!
//! Provides a lightweight HTTP API for inspecting and managing the running
//! proxy. Runs on a separate listener (configured via `global.admin_addr`)
//! and exposes endpoints for health, configuration, upstream status, metrics,
//! and hot-reload.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;

use http::{Request, Response, StatusCode};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::config::{GlobalConfig, parse_config};
use crate::hoops::metrics::Metrics;
use crate::runtime_services::{
    RuntimeListener, RuntimeListenerPatch, RuntimeRoute, RuntimeRoutePatch, RuntimeService,
    RuntimeServiceError, RuntimeServicePatch, RuntimeState, RuntimeTarget, RuntimeTargetPatch,
    RuntimeTargetRef, RuntimeTargetState, runtime_target_activity_key,
};
use crate::server::{AppState, RuntimeStateError};
use crate::{Body, ProxyError, full_body};

#[derive(serde::Serialize)]
struct RuntimeTargetView {
    service_id: String,
    route_id: String,
    group_id: String,
    activity: usize,
    #[serde(flatten)]
    target: RuntimeTarget,
}

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
                async move { handle_admin_request(req, state, &metrics).await }
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
    state: Arc<AppState>,
    metrics: &Metrics,
) -> Result<Response<Body>, std::convert::Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let config = state.config.load();
    if let Err(response) =
        authorize_admin_request(&req, &config.global, &method, segments.as_slice())
    {
        return Ok(*response);
    }

    let result = match segments.as_slice() {
        ["config"] if method == http::Method::GET => handle_get_config(&state),
        ["config", "reload"] if method == http::Method::POST => handle_config_reload(&state).await,
        ["config"] if method == http::Method::POST => handle_post_config(req, &state).await,
        ["config", "test"] if method == http::Method::POST => handle_test_config(req).await,
        ["stop"] if method == http::Method::POST => handle_stop(&state),
        ["health"] if method == http::Method::GET => handle_health(),
        ["upstreams"] if method == http::Method::GET => handle_upstreams(&state),
        ["metrics"] if method == http::Method::GET => handle_metrics(&state, metrics),
        ["runtime"] if method == http::Method::GET => handle_runtime(),
        ["runtime", "state"] if method == http::Method::GET => handle_runtime_state(&state),
        ["services"] if method == http::Method::GET => handle_list_services(&state),
        ["services", service_id] if method == http::Method::GET => {
            handle_get_service(&state, service_id)
        }
        ["services", service_id] if method == http::Method::PUT => {
            handle_put_service(req, Arc::clone(&state), service_id).await
        }
        ["services", service_id] if method == http::Method::PATCH => {
            handle_patch_service(req, Arc::clone(&state), service_id).await
        }
        ["services", service_id] if method == http::Method::DELETE => {
            handle_delete_service(req, Arc::clone(&state), service_id).await
        }
        ["services", service_id, "listeners"] if method == http::Method::GET => {
            handle_list_listeners(&state, service_id)
        }
        ["services", service_id, "listeners", listener_id] if method == http::Method::GET => {
            handle_get_listener(&state, service_id, listener_id)
        }
        ["services", service_id, "listeners", listener_id] if method == http::Method::PUT => {
            handle_put_listener(req, Arc::clone(&state), service_id, listener_id).await
        }
        ["services", service_id, "listeners", listener_id] if method == http::Method::PATCH => {
            handle_patch_listener(req, Arc::clone(&state), service_id, listener_id).await
        }
        ["services", service_id, "listeners", listener_id] if method == http::Method::DELETE => {
            handle_delete_listener(req, Arc::clone(&state), service_id, listener_id).await
        }
        ["services", service_id, "routes"] if method == http::Method::GET => {
            handle_list_routes(&state, service_id)
        }
        ["services", service_id, "routes", route_id] if method == http::Method::GET => {
            handle_get_route(&state, service_id, route_id)
        }
        ["services", service_id, "routes", route_id] if method == http::Method::PUT => {
            handle_put_route(req, Arc::clone(&state), service_id, route_id).await
        }
        ["services", service_id, "routes", route_id] if method == http::Method::PATCH => {
            handle_patch_route(req, Arc::clone(&state), service_id, route_id).await
        }
        ["services", service_id, "routes", route_id] if method == http::Method::DELETE => {
            handle_delete_route(req, Arc::clone(&state), service_id, route_id).await
        }
        ["services", service_id, "routes", route_id, "targets"] if method == http::Method::GET => {
            handle_list_route_targets(&state, service_id, route_id)
        }
        [
            "services",
            service_id,
            "routes",
            route_id,
            "target-groups",
            group_id,
            "targets",
        ] if method == http::Method::GET => {
            handle_list_targets(&state, service_id, route_id, group_id)
        }
        [
            "services",
            service_id,
            "routes",
            route_id,
            "target-groups",
            group_id,
            "targets",
            target_id,
        ] if method == http::Method::GET => {
            handle_get_target(&state, service_id, route_id, group_id, target_id)
        }
        [
            "services",
            service_id,
            "routes",
            route_id,
            "target-groups",
            group_id,
            "targets",
            target_id,
        ] if method == http::Method::PUT => {
            handle_put_target(
                req,
                Arc::clone(&state),
                service_id,
                route_id,
                group_id,
                target_id,
            )
            .await
        }
        [
            "services",
            service_id,
            "routes",
            route_id,
            "target-groups",
            group_id,
            "targets",
            target_id,
        ] if method == http::Method::PATCH => {
            handle_patch_target(
                req,
                Arc::clone(&state),
                service_id,
                route_id,
                group_id,
                target_id,
            )
            .await
        }
        [
            "services",
            service_id,
            "routes",
            route_id,
            "target-groups",
            group_id,
            "targets",
            target_id,
        ] if method == http::Method::DELETE => {
            handle_delete_target(
                req,
                Arc::clone(&state),
                service_id,
                route_id,
                group_id,
                target_id,
            )
            .await
        }
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
            Ok(new_config) => match state.reload(new_config).await {
                Ok(()) => json_response(StatusCode::OK, r#"{"status":"reloaded"}"#),
                Err(error) => json_runtime_state_error(error),
            },
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

fn handle_runtime_state(state: &AppState) -> Response<Body> {
    json_pretty_response(StatusCode::OK, &state.runtime_state_status())
}

/// `GET /services` — return runtime-managed services.
fn handle_list_services(state: &AppState) -> Response<Body> {
    json_pretty_response(StatusCode::OK, &state.runtime_services.list())
}

/// `GET /services/{id}` — return a runtime-managed service.
fn handle_get_service(state: &AppState, service_id: &str) -> Response<Body> {
    match state.runtime_services.get(service_id) {
        Some(service) => json_pretty_response(StatusCode::OK, &service),
        None => json_response(StatusCode::NOT_FOUND, r#"{"error":"service not found"}"#),
    }
}

fn handle_list_listeners(state: &AppState, service_id: &str) -> Response<Body> {
    let snapshot = state.runtime_services.snapshot();
    match snapshot.list_listeners(service_id) {
        Ok(listeners) => json_pretty_response(StatusCode::OK, &listeners),
        Err(error) => json_runtime_service_error(error),
    }
}

fn handle_get_listener(state: &AppState, service_id: &str, listener_id: &str) -> Response<Body> {
    let snapshot = state.runtime_services.snapshot();
    match snapshot.get_listener(service_id, listener_id) {
        Ok(listener) => json_pretty_response(StatusCode::OK, &listener),
        Err(error) => json_runtime_service_error(error),
    }
}

fn handle_list_routes(state: &AppState, service_id: &str) -> Response<Body> {
    let snapshot = state.runtime_services.snapshot();
    match snapshot.list_routes(service_id) {
        Ok(routes) => json_pretty_response(StatusCode::OK, &routes),
        Err(error) => json_runtime_service_error(error),
    }
}

fn handle_get_route(state: &AppState, service_id: &str, route_id: &str) -> Response<Body> {
    let snapshot = state.runtime_services.snapshot();
    match snapshot.get_route(service_id, route_id) {
        Ok(route) => json_pretty_response(StatusCode::OK, &route),
        Err(error) => json_runtime_service_error(error),
    }
}

fn handle_list_route_targets(state: &AppState, service_id: &str, route_id: &str) -> Response<Body> {
    let snapshot = state.runtime_services.snapshot();
    match snapshot.list_route_targets(service_id, route_id) {
        Ok(targets) => json_pretty_response(
            StatusCode::OK,
            &targets
                .into_iter()
                .map(|target| target_view_from_ref(state, service_id, route_id, target))
                .collect::<Vec<_>>(),
        ),
        Err(error) => json_runtime_service_error(error),
    }
}

fn handle_list_targets(
    state: &AppState,
    service_id: &str,
    route_id: &str,
    group_id: &str,
) -> Response<Body> {
    let snapshot = state.runtime_services.snapshot();
    match snapshot.list_targets(service_id, route_id, group_id) {
        Ok(targets) => json_pretty_response(
            StatusCode::OK,
            &targets
                .into_iter()
                .map(|target| target_view(state, service_id, route_id, group_id, target))
                .collect::<Vec<_>>(),
        ),
        Err(error) => json_runtime_service_error(error),
    }
}

fn handle_get_target(
    state: &AppState,
    service_id: &str,
    route_id: &str,
    group_id: &str,
    target_id: &str,
) -> Response<Body> {
    let snapshot = state.runtime_services.snapshot();
    match snapshot.get_target(service_id, route_id, group_id, target_id) {
        Ok(target) => json_pretty_response(
            StatusCode::OK,
            &target_view(state, service_id, route_id, group_id, target),
        ),
        Err(error) => json_runtime_service_error(error),
    }
}

async fn handle_put_service(
    req: Request<Incoming>,
    state: Arc<AppState>,
    service_id: &str,
) -> Response<Body> {
    let actor = runtime_actor(&req);
    let before = state.runtime_services.snapshot();
    let expected_revision = expected_revision(&req);
    let mut service: RuntimeService = match json_body(req).await {
        Ok(service) => service,
        Err(response) => return response,
    };
    service.id = service_id.to_string();

    match state
        .mutate_runtime_state(|runtime| runtime.upsert_service(service, expected_revision))
        .await
    {
        Ok(service) => {
            let after = state.runtime_services.snapshot();
            log_runtime_mutation(&actor, "upsert_service", &before, &after);
            json_pretty_response(StatusCode::OK, &service)
        }
        Err(error) => json_runtime_state_error(error),
    }
}

async fn handle_patch_service(
    req: Request<Incoming>,
    state: Arc<AppState>,
    service_id: &str,
) -> Response<Body> {
    let actor = runtime_actor(&req);
    let before = state.runtime_services.snapshot();
    let expected_revision = expected_revision(&req);
    let patch: RuntimeServicePatch = match json_body(req).await {
        Ok(patch) => patch,
        Err(response) => return response,
    };

    match state
        .mutate_runtime_state(|runtime| runtime.patch_service(service_id, patch, expected_revision))
        .await
    {
        Ok(service) => {
            let after = state.runtime_services.snapshot();
            log_runtime_mutation(&actor, "patch_service", &before, &after);
            json_pretty_response(StatusCode::OK, &service)
        }
        Err(error) => json_runtime_state_error(error),
    }
}

async fn handle_delete_service(
    req: Request<Incoming>,
    state: Arc<AppState>,
    service_id: &str,
) -> Response<Body> {
    let actor = runtime_actor(&req);
    let before = state.runtime_services.snapshot();
    let expected_revision = expected_revision(&req);
    match state
        .mutate_runtime_state(|runtime| runtime.remove_service(service_id, expected_revision))
        .await
    {
        Ok(service) => {
            let after = state.runtime_services.snapshot();
            log_runtime_mutation(&actor, "remove_service", &before, &after);
            json_pretty_response(StatusCode::OK, &service)
        }
        Err(error) => json_runtime_state_error(error),
    }
}

async fn handle_put_listener(
    req: Request<Incoming>,
    state: Arc<AppState>,
    service_id: &str,
    listener_id: &str,
) -> Response<Body> {
    let actor = runtime_actor(&req);
    let before = state.runtime_services.snapshot();
    let expected_revision = expected_revision(&req);
    let mut listener: RuntimeListener = match json_body(req).await {
        Ok(listener) => listener,
        Err(response) => return response,
    };
    listener.id = listener_id.to_string();

    match state
        .mutate_runtime_state(|runtime| {
            runtime.upsert_listener(service_id, listener_id, listener, expected_revision)
        })
        .await
    {
        Ok(listener) => {
            let after = state.runtime_services.snapshot();
            log_runtime_mutation(&actor, "upsert_listener", &before, &after);
            json_pretty_response(StatusCode::OK, &listener)
        }
        Err(error) => json_runtime_state_error(error),
    }
}

async fn handle_patch_listener(
    req: Request<Incoming>,
    state: Arc<AppState>,
    service_id: &str,
    listener_id: &str,
) -> Response<Body> {
    let actor = runtime_actor(&req);
    let before = state.runtime_services.snapshot();
    let expected_revision = expected_revision(&req);
    let patch: RuntimeListenerPatch = match json_body(req).await {
        Ok(patch) => patch,
        Err(response) => return response,
    };

    match state
        .mutate_runtime_state(|runtime| {
            runtime.patch_listener(service_id, listener_id, patch, expected_revision)
        })
        .await
    {
        Ok(listener) => {
            let after = state.runtime_services.snapshot();
            log_runtime_mutation(&actor, "patch_listener", &before, &after);
            json_pretty_response(StatusCode::OK, &listener)
        }
        Err(error) => json_runtime_state_error(error),
    }
}

async fn handle_delete_listener(
    req: Request<Incoming>,
    state: Arc<AppState>,
    service_id: &str,
    listener_id: &str,
) -> Response<Body> {
    let actor = runtime_actor(&req);
    let before = state.runtime_services.snapshot();
    let expected_revision = expected_revision(&req);
    match state
        .mutate_runtime_state(|runtime| {
            runtime.remove_listener(service_id, listener_id, expected_revision)
        })
        .await
    {
        Ok(listener) => {
            let after = state.runtime_services.snapshot();
            log_runtime_mutation(&actor, "remove_listener", &before, &after);
            json_pretty_response(StatusCode::OK, &listener)
        }
        Err(error) => json_runtime_state_error(error),
    }
}

async fn handle_put_route(
    req: Request<Incoming>,
    state: Arc<AppState>,
    service_id: &str,
    route_id: &str,
) -> Response<Body> {
    let actor = runtime_actor(&req);
    let before = state.runtime_services.snapshot();
    let expected_revision = expected_revision(&req);
    let mut route: RuntimeRoute = match json_body(req).await {
        Ok(route) => route,
        Err(response) => return response,
    };
    route.id = route_id.to_string();

    match state
        .mutate_runtime_state(|runtime| {
            runtime.upsert_route(service_id, route_id, route, expected_revision)
        })
        .await
    {
        Ok(route) => {
            let after = state.runtime_services.snapshot();
            log_runtime_mutation(&actor, "upsert_route", &before, &after);
            json_pretty_response(StatusCode::OK, &route)
        }
        Err(error) => json_runtime_state_error(error),
    }
}

async fn handle_patch_route(
    req: Request<Incoming>,
    state: Arc<AppState>,
    service_id: &str,
    route_id: &str,
) -> Response<Body> {
    let actor = runtime_actor(&req);
    let before = state.runtime_services.snapshot();
    let expected_revision = expected_revision(&req);
    let patch: RuntimeRoutePatch = match json_body(req).await {
        Ok(patch) => patch,
        Err(response) => return response,
    };

    match state
        .mutate_runtime_state(|runtime| {
            runtime.patch_route(service_id, route_id, patch, expected_revision)
        })
        .await
    {
        Ok(route) => {
            let after = state.runtime_services.snapshot();
            log_runtime_mutation(&actor, "patch_route", &before, &after);
            json_pretty_response(StatusCode::OK, &route)
        }
        Err(error) => json_runtime_state_error(error),
    }
}

async fn handle_delete_route(
    req: Request<Incoming>,
    state: Arc<AppState>,
    service_id: &str,
    route_id: &str,
) -> Response<Body> {
    let actor = runtime_actor(&req);
    let before = state.runtime_services.snapshot();
    let expected_revision = expected_revision(&req);
    match state
        .mutate_runtime_state(|runtime| {
            runtime.remove_route(service_id, route_id, expected_revision)
        })
        .await
    {
        Ok(route) => {
            let after = state.runtime_services.snapshot();
            log_runtime_mutation(&actor, "remove_route", &before, &after);
            json_pretty_response(StatusCode::OK, &route)
        }
        Err(error) => json_runtime_state_error(error),
    }
}

async fn handle_put_target(
    req: Request<Incoming>,
    state: Arc<AppState>,
    service_id: &str,
    route_id: &str,
    group_id: &str,
    target_id: &str,
) -> Response<Body> {
    let actor = runtime_actor(&req);
    let before = state.runtime_services.snapshot();
    let expected_revision = expected_revision(&req);
    let mut target: RuntimeTarget = match json_body(req).await {
        Ok(target) => target,
        Err(response) => return response,
    };
    target.id = target_id.to_string();

    match state
        .mutate_runtime_state(|runtime| {
            runtime.upsert_target(
                service_id,
                route_id,
                group_id,
                target_id,
                target,
                expected_revision,
            )
        })
        .await
    {
        Ok(target) => {
            let after = state.runtime_services.snapshot();
            log_runtime_mutation(&actor, "upsert_target", &before, &after);
            json_pretty_response(StatusCode::OK, &target)
        }
        Err(error) => json_runtime_state_error(error),
    }
}

async fn handle_patch_target(
    req: Request<Incoming>,
    state: Arc<AppState>,
    service_id: &str,
    route_id: &str,
    group_id: &str,
    target_id: &str,
) -> Response<Body> {
    let actor = runtime_actor(&req);
    let before = state.runtime_services.snapshot();
    let expected_revision = expected_revision(&req);
    let patch: RuntimeTargetPatch = match json_body(req).await {
        Ok(patch) => patch,
        Err(response) => return response,
    };

    match state
        .mutate_runtime_state(|runtime| {
            runtime.patch_target(
                service_id,
                route_id,
                group_id,
                target_id,
                patch,
                expected_revision,
            )
        })
        .await
    {
        Ok(target) => {
            let after = state.runtime_services.snapshot();
            log_runtime_mutation(&actor, "patch_target", &before, &after);
            json_pretty_response(StatusCode::OK, &target)
        }
        Err(error) => json_runtime_state_error(error),
    }
}

async fn handle_delete_target(
    req: Request<Incoming>,
    state: Arc<AppState>,
    service_id: &str,
    route_id: &str,
    group_id: &str,
    target_id: &str,
) -> Response<Body> {
    let actor = runtime_actor(&req);
    let before = state.runtime_services.snapshot();
    let expected_revision = expected_revision(&req);
    match state
        .mutate_runtime_state(|runtime| {
            runtime.patch_target(
                service_id,
                route_id,
                group_id,
                target_id,
                RuntimeTargetPatch {
                    state: Some(RuntimeTargetState::Draining),
                    ..RuntimeTargetPatch::default()
                },
                expected_revision,
            )
        })
        .await
    {
        Ok(target) => {
            let after = state.runtime_services.snapshot();
            log_runtime_mutation(&actor, "drain_target", &before, &after);
            json_pretty_response(StatusCode::ACCEPTED, &target)
        }
        Err(error) => json_runtime_state_error(error),
    }
}

/// `GET /metrics` — return Prometheus-format metrics.
fn handle_metrics(state: &AppState, metrics: &Metrics) -> Response<Body> {
    let mut output = metrics.render_prometheus();
    output.push_str(&render_runtime_target_metrics(
        &state.runtime_services.snapshot(),
        &state.backend_activity,
    ));
    output.push_str(&render_runtime_state_status_metrics(
        &state.runtime_state_status(),
    ));
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
            match state.reload(new_config).await {
                Ok(()) => {
                    info!("config applied via admin API");
                    json_response(
                        StatusCode::OK,
                        &format!(r#"{{"status":"applied","sites":{sites},"routes":{routes}}}"#),
                    )
                }
                Err(error) => json_runtime_state_error(error),
            }
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

fn json_pretty_response<T>(status: StatusCode, value: &T) -> Response<Body>
where
    T: serde::Serialize,
{
    match serde_json::to_string_pretty(value) {
        Ok(json) => Response::builder()
            .status(status)
            .header("Content-Type", "application/json")
            .body(full_body(json))
            .unwrap(),
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!(r#"{{"error":"serialization failed: {e}"}}"#),
        ),
    }
}

fn target_view(
    state: &AppState,
    service_id: &str,
    route_id: &str,
    group_id: &str,
    target: RuntimeTarget,
) -> RuntimeTargetView {
    let activity = state.backend_activity.active(&runtime_target_activity_key(
        service_id, route_id, group_id, &target.id,
    ));
    RuntimeTargetView {
        service_id: service_id.to_string(),
        route_id: route_id.to_string(),
        group_id: group_id.to_string(),
        activity,
        target,
    }
}

fn target_view_from_ref(
    state: &AppState,
    service_id: &str,
    route_id: &str,
    target_ref: RuntimeTargetRef,
) -> RuntimeTargetView {
    target_view(
        state,
        service_id,
        route_id,
        &target_ref.group_id,
        target_ref.target,
    )
}

fn json_runtime_service_error(error: RuntimeServiceError) -> Response<Body> {
    json_response(
        status_for_runtime_error(&error),
        &format!(r#"{{"error":"{error}"}}"#),
    )
}

fn json_runtime_state_error(error: RuntimeStateError) -> Response<Body> {
    match error {
        RuntimeStateError::Validation(error) => json_runtime_service_error(error),
        RuntimeStateError::Persist(message)
        | RuntimeStateError::Restore(message)
        | RuntimeStateError::Apply(message) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!(r#"{{"error":"{message}"}}"#),
        ),
    }
}

fn status_for_runtime_error(error: &RuntimeServiceError) -> StatusCode {
    match error {
        RuntimeServiceError::UnknownService(_)
        | RuntimeServiceError::UnknownListener { .. }
        | RuntimeServiceError::UnknownRoute { .. }
        | RuntimeServiceError::UnknownTargetGroup { .. }
        | RuntimeServiceError::UnknownTarget { .. } => StatusCode::NOT_FOUND,
        RuntimeServiceError::RevisionConflict { .. } => StatusCode::CONFLICT,
        RuntimeServiceError::UnsupportedVersion(_) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    }
}

async fn json_body<T>(req: Request<Incoming>) -> Result<T, Response<Body>>
where
    T: serde::de::DeserializeOwned,
{
    let body_bytes = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(error) => {
            return Err(json_response(
                StatusCode::BAD_REQUEST,
                &format!(r#"{{"error":"failed to read body: {error}"}}"#),
            ));
        }
    };

    serde_json::from_slice(&body_bytes).map_err(|error| {
        json_response(
            StatusCode::BAD_REQUEST,
            &format!(r#"{{"error":"invalid JSON payload: {error}"}}"#),
        )
    })
}

fn expected_revision(req: &Request<Incoming>) -> Option<u64> {
    req.headers()
        .get(http::header::IF_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim_matches('"'))
        .and_then(|value| value.parse::<u64>().ok())
}

type AdminAuthorizationResult = Result<(), Box<Response<Body>>>;

fn authorize_admin_request<B>(
    req: &Request<B>,
    global: &GlobalConfig,
    method: &http::Method,
    segments: &[&str],
) -> AdminAuthorizationResult {
    let needs_write = admin_request_needs_write(method, segments);

    if let Some(token) = &global.admin_auth_token {
        if check_bearer_auth(req, token) {
            return Ok(());
        }
        return Err(Box::new(json_response(
            StatusCode::UNAUTHORIZED,
            r#"{"error":"unauthorized"}"#,
        )));
    }

    let read_allowed = global
        .admin_read_token
        .as_ref()
        .is_some_and(|token| check_bearer_auth(req, token));
    let write_allowed = global
        .admin_write_token
        .as_ref()
        .is_some_and(|token| check_bearer_auth(req, token));

    if needs_write {
        if write_allowed {
            Ok(())
        } else if global.admin_write_token.is_some() || global.admin_read_token.is_some() {
            Err(Box::new(json_response(
                StatusCode::FORBIDDEN,
                r#"{"error":"write access required"}"#,
            )))
        } else {
            Ok(())
        }
    } else if read_allowed || write_allowed {
        Ok(())
    } else if global.admin_read_token.is_some() || global.admin_write_token.is_some() {
        Err(Box::new(json_response(
            StatusCode::UNAUTHORIZED,
            r#"{"error":"unauthorized"}"#,
        )))
    } else {
        Ok(())
    }
}

fn admin_request_needs_write(method: &http::Method, segments: &[&str]) -> bool {
    !matches!(
        (method, segments),
        (&http::Method::POST, ["config", "test"])
            | (&http::Method::GET, _)
            | (&http::Method::HEAD, _)
            | (&http::Method::OPTIONS, _)
    )
}

/// Check for a valid `Authorization: Bearer <token>` header.
fn check_bearer_auth<B>(req: &Request<B>, expected_token: &str) -> bool {
    req.headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.starts_with("Bearer ")
                && crate::crypto::constant_time_eq(&v.as_bytes()[7..], expected_token.as_bytes())
        })
        .unwrap_or(false)
}

fn render_runtime_target_metrics(
    snapshot: &crate::runtime_services::RuntimeState,
    backend_activity: &crate::proxy::activity::BackendActivityTracker,
) -> String {
    let mut counts: BTreeMap<(String, String, String, &'static str), u64> = BTreeMap::new();
    let mut activity_rows: Vec<(String, String, String, String, &'static str, usize)> = Vec::new();
    for service in snapshot.list_services() {
        for route in service.routes {
            for group in route.target_groups {
                for target in group.targets {
                    let state = match target.state {
                        RuntimeTargetState::Active => "healthy",
                        RuntimeTargetState::Warming => "warming",
                        RuntimeTargetState::Draining => "draining",
                        RuntimeTargetState::Failed => "failed",
                    };
                    *counts
                        .entry((
                            service.id.clone(),
                            route.id.clone(),
                            group.id.clone(),
                            state,
                        ))
                        .or_default() += 1;
                    activity_rows.push((
                        service.id.clone(),
                        route.id.clone(),
                        group.id.clone(),
                        target.id.clone(),
                        state,
                        backend_activity.active(&runtime_target_activity_key(
                            &service.id,
                            &route.id,
                            &group.id,
                            &target.id,
                        )),
                    ));
                }
            }
        }
    }

    let mut output = String::new();
    output.push_str(
        "# HELP gatel_runtime_targets Number of runtime targets by service, route, group, and state.\n",
    );
    output.push_str("# TYPE gatel_runtime_targets gauge\n");
    for ((service, route, group, state), count) in counts {
        output.push_str(&format!(
            "gatel_runtime_targets{{service=\"{service}\",route=\"{route}\",group=\"{group}\",state=\"{state}\"}} {count}\n"
        ));
    }
    output.push_str(
        "# HELP gatel_runtime_target_activity Active in-flight activity per runtime target.\n",
    );
    output.push_str("# TYPE gatel_runtime_target_activity gauge\n");
    for (service, route, group, target, state, activity) in activity_rows {
        output.push_str(&format!(
            "gatel_runtime_target_activity{{service=\"{service}\",route=\"{route}\",group=\"{group}\",target=\"{target}\",state=\"{state}\"}} {activity}\n"
        ));
    }
    output
}

fn render_runtime_state_status_metrics(status: &crate::server::RuntimeStateStatus) -> String {
    let mut output = String::new();
    output.push_str(
        "# HELP gatel_runtime_state_corrupted Whether the persisted runtime state is currently marked corrupted.\n",
    );
    output.push_str("# TYPE gatel_runtime_state_corrupted gauge\n");
    output.push_str(&format!(
        "gatel_runtime_state_corrupted {}\n",
        if status.corrupted { 1 } else { 0 }
    ));
    output.push_str(
        "# HELP gatel_runtime_state_available Whether a runtime state snapshot is currently available.\n",
    );
    output.push_str("# TYPE gatel_runtime_state_available gauge\n");
    output.push_str(&format!(
        "gatel_runtime_state_available {}\n",
        if status.available { 1 } else { 0 }
    ));
    output
}

fn runtime_actor<B>(req: &Request<B>) -> String {
    req.headers()
        .get("x-gatel-actor")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .unwrap_or("admin_api")
        .to_string()
}

fn log_runtime_mutation(actor: &str, action: &str, before: &RuntimeState, after: &RuntimeState) {
    info!(
        actor = actor,
        action = action,
        before_revision = before.revision,
        after_revision = after.revision,
        diff = %summarize_runtime_diff(before, after),
        "runtime state mutated"
    );
}

fn summarize_runtime_diff(before: &RuntimeState, after: &RuntimeState) -> String {
    let before_services: BTreeSet<_> = before.services.keys().cloned().collect();
    let after_services: BTreeSet<_> = after.services.keys().cloned().collect();
    let services_added = after_services.difference(&before_services).count();
    let services_removed = before_services.difference(&after_services).count();
    let services_updated = before_services
        .intersection(&after_services)
        .filter(|service_id| before.services.get(*service_id) != after.services.get(*service_id))
        .count();

    let before_listeners = collect_listener_map(before);
    let after_listeners = collect_listener_map(after);
    let listeners_added = count_added(&before_listeners, &after_listeners);
    let listeners_removed = count_added(&after_listeners, &before_listeners);
    let listeners_updated = count_updated(&before_listeners, &after_listeners);

    let before_routes = collect_route_map(before);
    let after_routes = collect_route_map(after);
    let routes_added = count_added(&before_routes, &after_routes);
    let routes_removed = count_added(&after_routes, &before_routes);
    let routes_updated = count_updated(&before_routes, &after_routes);

    let before_targets = collect_target_map(before);
    let after_targets = collect_target_map(after);
    let targets_added = count_added(&before_targets, &after_targets);
    let targets_removed = count_added(&after_targets, &before_targets);
    let targets_updated = count_updated(&before_targets, &after_targets);

    format!(
        "services +{services_added} -{services_removed} ~{services_updated}; listeners +{listeners_added} -{listeners_removed} ~{listeners_updated}; routes +{routes_added} -{routes_removed} ~{routes_updated}; targets +{targets_added} -{targets_removed} ~{targets_updated}"
    )
}

fn collect_listener_map(state: &RuntimeState) -> BTreeMap<(String, String), RuntimeListener> {
    let mut listeners = BTreeMap::new();
    for service in state.list_services() {
        for listener in service.listeners {
            listeners.insert((service.id.clone(), listener.id.clone()), listener);
        }
    }
    listeners
}

fn collect_route_map(state: &RuntimeState) -> BTreeMap<(String, String), RuntimeRoute> {
    let mut routes = BTreeMap::new();
    for service in state.list_services() {
        for route in service.routes {
            routes.insert((service.id.clone(), route.id.clone()), route);
        }
    }
    routes
}

fn collect_target_map(
    state: &RuntimeState,
) -> BTreeMap<(String, String, String, String), RuntimeTarget> {
    let mut targets = BTreeMap::new();
    for service in state.list_services() {
        for route in service.routes {
            for group in route.target_groups {
                for target in group.targets {
                    targets.insert(
                        (
                            service.id.clone(),
                            route.id.clone(),
                            group.id.clone(),
                            target.id.clone(),
                        ),
                        target,
                    );
                }
            }
        }
    }
    targets
}

fn count_added<K, V>(before: &BTreeMap<K, V>, after: &BTreeMap<K, V>) -> usize
where
    K: Ord,
{
    after
        .keys()
        .filter(|key| !before.contains_key(*key))
        .count()
}

fn count_updated<K, V>(before: &BTreeMap<K, V>, after: &BTreeMap<K, V>) -> usize
where
    K: Ord,
    V: PartialEq,
{
    before
        .iter()
        .filter(|(key, value)| {
            after
                .get(*key)
                .is_some_and(|after_value| after_value != *value)
        })
        .count()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::config::AppConfig;
    use crate::runtime_services::{
        RuntimeRoute, RuntimeService, RuntimeTarget, RuntimeTargetGroup,
    };

    fn request(method: http::Method) -> Request<()> {
        Request::builder()
            .method(method)
            .uri("/services")
            .body(())
            .unwrap()
    }

    #[test]
    fn write_token_denies_mutation_for_read_only_token() {
        let mut req = request(http::Method::POST);
        req.headers_mut().insert(
            http::header::AUTHORIZATION,
            "Bearer read-token".parse().unwrap(),
        );
        let global = GlobalConfig {
            admin_read_token: Some("read-token".to_string()),
            admin_write_token: Some("write-token".to_string()),
            ..GlobalConfig::default()
        };

        let response =
            authorize_admin_request(&req, &global, req.method(), &["services"]).unwrap_err();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn write_token_allows_reads_and_writes() {
        let mut req = request(http::Method::GET);
        req.headers_mut().insert(
            http::header::AUTHORIZATION,
            "Bearer write-token".parse().unwrap(),
        );
        let global = GlobalConfig {
            admin_write_token: Some("write-token".to_string()),
            ..GlobalConfig::default()
        };

        authorize_admin_request(&req, &global, req.method(), &["services"]).unwrap();
        assert!(admin_request_needs_write(
            &http::Method::POST,
            &["services"]
        ));
    }

    #[tokio::test]
    async fn runtime_service_and_target_crud_via_admin_api() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let state = AppState::new(AppConfig::default(), None);
        let metrics = Arc::clone(&state.metrics);
        let server = tokio::spawn({
            let state = Arc::clone(&state);
            async move { start_admin_server(addr, state, metrics).await.unwrap() }
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let base_url = format!("http://{addr}");

        for _ in 0..40 {
            if let Ok(response) = client.get(format!("{base_url}/health")).send().await
                && response.status().is_success()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        let response = client
            .put(format!("{base_url}/services/api"))
            .json(&json!({
                "id": "api",
                "listeners": [{ "id": "https", "protocol": "https" }],
                "routes": [{
                    "id": "api",
                    "hosts": ["example.com"],
                    "path_prefix": "/api",
                    "target_groups": [{
                        "id": "primary",
                        "targets": [{
                            "id": "app-1",
                            "addr": "127.0.0.1:3000",
                            "state": "active"
                        }]
                    }]
                }]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert!(state.runtime_services.get("api").is_some());

        let response = client
            .get(format!(
                "{base_url}/services/api/routes/api/target-groups/primary/targets/app-1"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let payload: serde_json::Value = response.json().await.unwrap();
        assert_eq!(payload["activity"], 0);
        assert_eq!(payload["service_id"], "api");
        assert_eq!(payload["route_id"], "api");
        assert_eq!(payload["group_id"], "primary");

        let response = client
            .patch(format!(
                "{base_url}/services/api/routes/api/target-groups/primary/targets/app-1"
            ))
            .json(&json!({ "weight": 250, "drain_timeout": "2s" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            state
                .runtime_services
                .snapshot()
                .get_target("api", "api", "primary", "app-1")
                .unwrap()
                .weight,
            250
        );

        let response = client
            .delete(format!(
                "{base_url}/services/api/routes/api/target-groups/primary/targets/app-1"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
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

        let response = client
            .delete(format!("{base_url}/services/api"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert!(state.runtime_services.get("api").is_none());

        server.abort();
    }

    #[test]
    fn render_runtime_target_metrics_includes_activity_gauge() {
        let mut runtime = RuntimeState::default();
        runtime
            .upsert_service(
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
                        lb: crate::config::LbPolicy::RoundRobin,
                        lb_header: None,
                        lb_cookie: None,
                        response: None,
                        timeout_seconds: None,
                        target_groups: vec![RuntimeTargetGroup {
                            id: "primary".to_string(),
                            weight: 100,
                            targets: vec![RuntimeTarget {
                                id: "app-1".to_string(),
                                addr: "127.0.0.1:3000".to_string(),
                                weight: 100,
                                state: RuntimeTargetState::Draining,
                                drain_timeout: Duration::from_secs(5),
                                last_error: None,
                            }],
                        }],
                        health_check: Default::default(),
                    }],
                    tls: None,
                },
                None,
            )
            .unwrap();

        let activity = crate::proxy::activity::BackendActivityTracker::default();
        let _guard = activity.acquire(runtime_target_activity_key(
            "api", "api", "primary", "app-1",
        ));

        let metrics = render_runtime_target_metrics(&runtime, &activity);
        assert!(metrics.contains("gatel_runtime_targets"));
        assert!(metrics.contains("gatel_runtime_target_activity"));
        assert!(metrics.contains("target=\"app-1\""));
        assert!(metrics.contains("state=\"draining\""));
        assert!(metrics.contains("} 1"));
    }
}
