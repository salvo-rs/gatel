use std::sync::Arc;

use http::Request as HttpRequest;
use salvo::compression::{Compression, CompressionLevel};
use salvo::cors::Cors;
use salvo::force_https::ForceHttps;
use salvo::logging::Logger;
use salvo::request_id::RequestId;
use salvo::serve_static::StaticDir;
use salvo::timeout::Timeout;
use salvo::trailing_slash::{TrailingSlash, TrailingSlashAction};
use salvo::{Depot, FlowCtrl, Handler, Request, Response, Router, Service, async_trait};

use crate::config::{
    AppConfig, HandlerConfig, HoopConfig, RouteCondition, RouteConfig, SiteConfig,
};
use crate::plugin::ModuleRegistry;
use crate::router::matcher::{RequestMatcher, path_matches, pattern_specificity};

pub fn build_service(config: &AppConfig, modules: &ModuleRegistry) -> Service {
    Service::new(build_router(config, modules)).hoop(Logger::new())
}

fn build_router(config: &AppConfig, modules: &ModuleRegistry) -> Router {
    let mut root = Router::new();
    for site in &config.sites {
        root = root.push(build_site_router(site, modules));
    }
    root
}

fn build_site_router(site: &SiteConfig, modules: &ModuleRegistry) -> Router {
    let mut site_router = if site.host == "*" {
        Router::new()
    } else {
        let site_host = site.host.clone();
        Router::new().filter_fn(move |req, _| site_matches(&site_host, req))
    };

    let mut routes: Vec<(usize, &RouteConfig)> = site.routes.iter().enumerate().collect();
    routes.sort_by(|(index_a, route_a), (index_b, route_b)| {
        pattern_specificity(&route_b.path)
            .cmp(&pattern_specificity(&route_a.path))
            .then(index_a.cmp(index_b))
    });

    for (_, route) in routes {
        site_router = site_router.push(build_route_router(route, modules));
    }
    site_router
}

fn build_route_router(route: &RouteConfig, modules: &ModuleRegistry) -> Router {
    let mut router = Router::with_path("{**gatel_rest}");

    let route_path = route.path.clone();
    let matchers = route.matchers.clone();
    let condition = route.condition.clone();
    router = router.filter_fn(move |req, _| {
        route_matches_request(&route_path, &matchers, condition.as_ref(), req)
    });

    // Hoop each middleware individually onto the router.
    for mw_cfg in &route.middlewares {
        match mw_cfg {
            HoopConfig::IpFilter {
                allow,
                deny,
                forwarded_for,
            } => {
                router = router.hoop(crate::hoops::ip_filter::IpFilterHoop::new(
                    allow,
                    deny,
                    *forwarded_for,
                ));
            }
            HoopConfig::RateLimit { window, max, burst } => {
                router = router.hoop(crate::hoops::rate_limit::RateLimitHoop::new(
                    *window, *max, *burst,
                ));
            }
            HoopConfig::ForwardAuth { url, copy_headers } => {
                router = router.hoop(crate::hoops::forward_auth::ForwardAuthHoop::new(
                    url.clone(),
                    copy_headers.clone(),
                ));
            }
            HoopConfig::BasicAuth {
                users,
                brute_force_max,
                brute_force_window,
            } => {
                router = router.hoop(crate::hoops::auth::BasicAuthHoop::new(
                    users,
                    *brute_force_max,
                    *brute_force_window,
                ));
            }
            HoopConfig::Headers(cfg) => {
                router = router.hoop(crate::hoops::headers::HeadersHoop::new(cfg));
            }
            HoopConfig::Rewrite {
                strip_prefix,
                uri,
                regex_rules,
                if_not_file,
                if_not_dir,
                root,
                normalize_slashes,
            } => {
                // Compile regex patterns; log a warning and skip invalid ones.
                let compiled_regex: Vec<(regex::Regex, String)> = regex_rules
                    .iter()
                    .filter_map(|(pattern, replacement)| match regex::Regex::new(pattern) {
                        Ok(re) => Some((re, replacement.clone())),
                        Err(e) => {
                            tracing::warn!(
                                pattern = pattern.as_str(),
                                error = %e,
                                "invalid regex in rewrite rule, skipping"
                            );
                            None
                        }
                    })
                    .collect();
                router = router.hoop(crate::hoops::rewrite::RewriteHoop::new(
                    strip_prefix.clone(),
                    uri.clone(),
                    compiled_regex,
                    *if_not_file,
                    *if_not_dir,
                    root.clone(),
                    *normalize_slashes,
                ));
            }
            HoopConfig::Encode { encodings, level } => {
                // Try Salvo's built-in compression first.
                if let Some(compression) = build_salvo_compression(encodings, *level) {
                    router = router.hoop(compression);
                } else {
                    // Fallback to our custom compress middleware.
                    router =
                        router.hoop(crate::hoops::compress::CompressHoop::new(encodings, *level));
                }
            }
            HoopConfig::Cache(cfg) => {
                router = router.hoop(crate::hoops::cache::CacheHoop::new(cfg));
            }
            HoopConfig::Templates { root } => {
                router = router.hoop(crate::hoops::templates::TemplatesHoop::new(root.clone()));
            }
            HoopConfig::Replace { rules, once } => {
                router = router.hoop(crate::hoops::replace::ReplaceHoop::new(
                    rules.clone(),
                    *once,
                ));
            }
            HoopConfig::BufferLimit {
                max_request_body,
                max_response_body,
            } => {
                router = router.hoop(crate::hoops::buffer::BufferLimitHoop::new(
                    *max_request_body,
                    *max_response_body,
                ));
            }
            HoopConfig::Cors {
                allow_origins,
                allow_methods,
                allow_headers,
                allow_credentials,
                expose_headers,
                max_age,
            } => {
                let mut cors = if allow_origins.len() == 1 && allow_origins[0] == "*" {
                    Cors::new().allow_origin(salvo::cors::AllowOrigin::any())
                } else {
                    Cors::new().allow_origin(allow_origins)
                };

                if allow_methods.len() == 1 && allow_methods[0] == "*" {
                    cors = cors.allow_methods(salvo::cors::AllowMethods::any());
                } else if !allow_methods.is_empty() {
                    let methods: Vec<http::Method> = allow_methods
                        .iter()
                        .filter_map(|m| m.parse().ok())
                        .collect();
                    cors = cors.allow_methods(methods);
                }

                if allow_headers.len() == 1 && allow_headers[0] == "*" {
                    cors = cors.allow_headers(salvo::cors::AllowHeaders::any());
                } else if !allow_headers.is_empty() {
                    cors = cors.allow_headers(allow_headers);
                }

                cors = cors.allow_credentials(*allow_credentials);

                if !expose_headers.is_empty() {
                    if expose_headers.len() == 1 && expose_headers[0] == "*" {
                        cors = cors.expose_headers(salvo::cors::ExposeHeaders::any());
                    } else {
                        cors = cors.expose_headers(expose_headers);
                    }
                }

                if let Some(age) = max_age {
                    cors = cors.max_age(*age);
                }

                router = router.hoop(cors.into_handler());
            }
            HoopConfig::Timeout { duration } => {
                router = router.hoop(Timeout::new(*duration));
            }
            HoopConfig::RequestId {
                header_name,
                overwrite,
            } => {
                let mut rid = RequestId::new();
                if let Some(name) = header_name {
                    if let Ok(hn) = name.parse::<http::header::HeaderName>() {
                        rid = rid.header_name(hn);
                    }
                }
                rid = rid.overwrite(*overwrite);
                router = router.hoop(rid);
            }
            HoopConfig::ForceHttps { https_port } => {
                let mut fh = ForceHttps::new();
                if let Some(port) = https_port {
                    fh = fh.https_port(*port);
                }
                router = router.hoop(fh);
            }
            HoopConfig::TrailingSlash { action } => {
                let ts = match action.as_str() {
                    "remove" => TrailingSlash::new(TrailingSlashAction::Remove),
                    _ => TrailingSlash::new(TrailingSlashAction::Add),
                };
                router = router.hoop(ts);
            }
            HoopConfig::Decompress { max_size } => {
                router = router.hoop(crate::hoops::decompress::DecompressHoop::new(*max_size));
            }
            HoopConfig::ErrorPages { pages } => {
                router = router.hoop(crate::hoops::error_pages::ErrorPagesHoop::new(
                    pages.clone(),
                ));
            }
            HoopConfig::StreamReplace { rules, once } => {
                router = router.hoop(crate::hoops::stream_replace::StreamReplaceHoop::new(
                    rules.clone(),
                    *once,
                ));
            }
            HoopConfig::Module { name, config } => {
                if let Some(loader) = modules.get(name) {
                    match loader.create_middleware(config) {
                        Ok(Some(mw)) => {
                            router = router.hoop(SalvoHandlerArc(mw));
                        }
                        Ok(None) => {
                            tracing::debug!(module = %name, "module did not provide middleware");
                        }
                        Err(e) => {
                            tracing::warn!(module = %name, error = %e, "module middleware creation failed");
                        }
                    }
                } else {
                    tracing::warn!(module = %name, "unknown module, skipping");
                }
            }
        }
    }

    // Set the terminal handler (goal).
    match &route.handler {
        HandlerConfig::FileServer(cfg) if cfg.trailing_slash => {
            let mut handler = StaticDir::new(cfg.root.clone()).auto_list(cfg.browse);
            if !cfg.index.is_empty() {
                handler = handler.defaults(cfg.index.clone());
            }
            router.goal(handler)
        }
        HandlerConfig::Redirect { to, permanent } => router.goal(RedirectGoal {
            target: to.clone(),
            permanent: *permanent,
        }),
        HandlerConfig::Respond { status, body } => router.goal(RespondGoal {
            status: *status,
            body: body.clone(),
        }),
        HandlerConfig::Proxy(proxy_cfg) => router.goal(crate::proxy::ReverseProxy::new(proxy_cfg)),
        HandlerConfig::FastCgi(cfg) => router.goal(crate::proxy::fastcgi::FastCgiTransport::new(
            cfg.addr.clone(),
            cfg.script_root.clone(),
            cfg.index.clone(),
            cfg.split_path.clone(),
            cfg.env.clone(),
        )),
        HandlerConfig::ForwardProxy(cfg) => router.goal(
            crate::proxy::forward_proxy::ForwardProxy::new(&cfg.auth_users),
        ),
        HandlerConfig::Cgi(cfg) => router.goal(crate::proxy::cgi::CgiHandler::new(
            cfg.root.clone(),
            cfg.env.clone(),
        )),
        HandlerConfig::Scgi(cfg) => router.goal(crate::proxy::scgi::ScgiHandler::new(
            cfg.addr.clone(),
            cfg.env.clone(),
        )),
        HandlerConfig::FileServer(cfg) => {
            // Non-trailing-slash file server uses our custom handler.
            router.goal(crate::goals::file_server::FileServerGoal::new(
                &cfg.root,
                cfg.browse,
                cfg.trailing_slash,
                cfg.index.clone(),
            ))
        }
        HandlerConfig::Module { name, config } => {
            if let Some(loader) = modules.get(name) {
                match loader.create_handler(config) {
                    Ok(Some(h)) => router.goal(SalvoHandlerArc(h)),
                    Ok(None) => router.goal(RespondGoal {
                        status: 500,
                        body: format!("module '{name}' did not provide a handler"),
                    }),
                    Err(e) => router.goal(RespondGoal {
                        status: 500,
                        body: format!("module '{name}' error: {e}"),
                    }),
                }
            } else {
                router.goal(RespondGoal {
                    status: 500,
                    body: format!("unknown module: {name}"),
                })
            }
        }
    }
}

fn build_salvo_compression(encodings: &[String], level: Option<u32>) -> Option<Compression> {
    let mut compression = Compression::new().disable_all();
    let mut enabled_any = false;

    let level = match level {
        Some(l) => CompressionLevel::Precise(l),
        None => CompressionLevel::Default,
    };

    for encoding in encodings {
        match encoding.as_str() {
            "gzip" => {
                compression = compression.enable_gzip(level);
                enabled_any = true;
            }
            "br" | "brotli" => {
                compression = compression.enable_brotli(level);
                enabled_any = true;
            }
            "deflate" => {
                compression = compression.enable_deflate(level);
                enabled_any = true;
            }
            "zstd" => {
                compression = compression.enable_zstd(level);
                enabled_any = true;
            }
            _ => {}
        }
    }

    if enabled_any { Some(compression) } else { None }
}

fn site_matches(site_host: &str, req: &Request) -> bool {
    extract_host(req) == site_host
}

fn route_matches_request(
    route_path: &str,
    matchers: &[RequestMatcher],
    condition: Option<&RouteCondition>,
    req: &Request,
) -> bool {
    let request = request_for_matching(req);
    if !path_matches(route_path, request.uri().path()) {
        return false;
    }

    let client_addr = req
        .remote_addr()
        .clone()
        .into_std()
        .unwrap_or_else(unknown_client_addr);

    if !matchers
        .iter()
        .all(|matcher| matcher.matches(&request, client_addr))
    {
        return false;
    }

    if let Some(condition) = condition {
        evaluate_condition(condition, &request, client_addr)
    } else {
        true
    }
}

fn request_for_matching(req: &Request) -> HttpRequest<crate::Body> {
    let mut builder = HttpRequest::builder()
        .method(req.method().clone())
        .uri(req.uri().clone())
        .version(req.version());
    *builder.headers_mut().expect("request builder has headers") = req.headers().clone();
    builder
        .body(crate::empty_body())
        .expect("failed to build request for matching")
}

fn evaluate_condition(
    condition: &RouteCondition,
    req: &HttpRequest<crate::Body>,
    client_addr: std::net::SocketAddr,
) -> bool {
    match condition {
        RouteCondition::RemoteIp(cidrs) => {
            let ip = client_addr.ip();
            cidrs
                .iter()
                .any(|cidr| crate::router::matcher::match_cidr_pub(cidr, &ip))
        }
        RouteCondition::NotRemoteIp(cidrs) => {
            let ip = client_addr.ip();
            !cidrs
                .iter()
                .any(|cidr| crate::router::matcher::match_cidr_pub(cidr, &ip))
        }
        RouteCondition::Header { name, value } => req
            .headers()
            .get(name.as_str())
            .and_then(|value| value.to_str().ok())
            .map(|header| header == value)
            .unwrap_or(false),
        RouteCondition::NotHeader { name, value } => !req
            .headers()
            .get(name.as_str())
            .and_then(|value| value.to_str().ok())
            .map(|header| header == value)
            .unwrap_or(false),
    }
}

fn extract_host(req: &Request) -> String {
    req.headers()
        .get(http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| req.uri().host())
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_string()
}

fn unknown_client_addr() -> std::net::SocketAddr {
    std::net::SocketAddr::from(([0, 0, 0, 0], 0))
}

// ---------------------------------------------------------------------------
// Wrapper for Arc<dyn salvo::Handler>
// ---------------------------------------------------------------------------

struct SalvoHandlerArc(Arc<dyn Handler>);

#[async_trait]
impl Handler for SalvoHandlerArc {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        self.0.handle(req, depot, res, ctrl).await;
    }
}

// ---------------------------------------------------------------------------
// Simple goal handlers
// ---------------------------------------------------------------------------

struct RedirectGoal {
    target: String,
    permanent: bool,
}

#[async_trait]
impl Handler for RedirectGoal {
    async fn handle(
        &self,
        _req: &mut Request,
        _depot: &mut Depot,
        res: &mut Response,
        _ctrl: &mut FlowCtrl,
    ) {
        let status = if self.permanent {
            salvo::http::StatusCode::MOVED_PERMANENTLY
        } else {
            salvo::http::StatusCode::TEMPORARY_REDIRECT
        };
        res.status_code(status);
        let _ = res.add_header("Location", self.target.clone(), true);
    }
}

struct RespondGoal {
    status: u16,
    body: String,
}

#[async_trait]
impl Handler for RespondGoal {
    async fn handle(
        &self,
        _req: &mut Request,
        _depot: &mut Depot,
        res: &mut Response,
        _ctrl: &mut FlowCtrl,
    ) {
        let status = salvo::http::StatusCode::from_u16(self.status)
            .unwrap_or(salvo::http::StatusCode::INTERNAL_SERVER_ERROR);
        res.status_code(status);
        res.body(self.body.clone());
    }
}
