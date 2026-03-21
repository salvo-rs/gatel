use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use kdl::{KdlDocument, KdlNode};

use super::types::*;
use crate::router::matcher::RequestMatcher;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("KDL parse error: {0}")]
    Kdl(#[from] kdl::KdlError),

    #[error("missing required field: {0}")]
    MissingField(String),

    #[error("unknown directive: {0}")]
    UnknownDirective(String),

    #[error("invalid value for '{field}': {detail}")]
    InvalidValue { field: String, detail: String },
}

/// Parse a KDL configuration string into an `AppConfig`.
pub fn parse_config(input: &str) -> Result<AppConfig, ConfigError> {
    let doc: KdlDocument = input.parse()?;
    let mut config = AppConfig::default();

    // First pass: collect all `snippet "name" { ... }` blocks.
    let mut snippets: HashMap<String, KdlNode> = HashMap::new();
    for node in doc.nodes() {
        if node.name().to_string() == "snippet"
            && let Some(name) = first_string_arg(node)
        {
            snippets.insert(name, node.clone());
        }
    }

    // Second pass: parse the rest of the config, skipping snippet nodes.
    for node in doc.nodes() {
        match node.name().to_string().as_str() {
            "snippet" => { /* already collected above */ }
            "global" => config.global = parse_global(node)?,
            "tls" => config.tls = Some(parse_tls(node)?),
            "site" => {
                let host = first_string_arg(node)
                    .ok_or_else(|| ConfigError::MissingField("site host".into()))?;
                config.sites.push(parse_site(&host, node, &snippets)?);
            }
            "stream" => config.stream = Some(parse_stream(node)?),
            other => return Err(ConfigError::UnknownDirective(other.into())),
        }
    }
    Ok(config)
}

/// Generate a minimal KDL config string from well-known environment variables.
///
/// Returns `None` if no relevant environment variables are set.
///
/// Recognised variables:
/// - `GATEL_HTTP_ADDR`   — HTTP listen address (default `:80`)
/// - `GATEL_HTTPS_ADDR`  — HTTPS listen address (default `:443`)
/// - `GATEL_ADMIN_ADDR`  — admin API listen address
/// - `GATEL_ACME_EMAIL`  — ACME email (enables auto-TLS)
/// - `GATEL_ACME_CA`     — ACME CA (`letsencrypt`, `letsencrypt-staging`, `zerossl`; default
///   `letsencrypt`)
/// - `GATEL_HOST`        — virtual-host name for the generated site block
/// - `GATEL_UPSTREAM`    — upstream proxy target (enables a `proxy` route)
pub fn auto_config_from_env() -> Option<String> {
    let http_addr = std::env::var("GATEL_HTTP_ADDR").ok();
    let https_addr = std::env::var("GATEL_HTTPS_ADDR").ok();
    let admin_addr = std::env::var("GATEL_ADMIN_ADDR").ok();
    let acme_email = std::env::var("GATEL_ACME_EMAIL").ok();
    let acme_ca = std::env::var("GATEL_ACME_CA").unwrap_or_else(|_| "letsencrypt".to_string());
    let host = std::env::var("GATEL_HOST").ok();
    let upstream = std::env::var("GATEL_UPSTREAM").ok();

    // Require at least one meaningful env var to be present.
    if http_addr.is_none()
        && https_addr.is_none()
        && admin_addr.is_none()
        && acme_email.is_none()
        && host.is_none()
        && upstream.is_none()
    {
        return None;
    }

    let mut out = String::new();

    // Global block.
    out.push_str("global {\n");
    if let Some(addr) = &http_addr {
        out.push_str(&format!("    http \"{addr}\"\n"));
    }
    if let Some(addr) = &https_addr {
        out.push_str(&format!("    https \"{addr}\"\n"));
    }
    if let Some(addr) = &admin_addr {
        out.push_str(&format!("    admin \"{addr}\"\n"));
    }
    out.push_str("}\n");

    // TLS / ACME block (only when an email is provided).
    if let Some(email) = &acme_email {
        out.push_str("tls {\n");
        out.push_str("    acme {\n");
        out.push_str(&format!("        email \"{email}\"\n"));
        out.push_str(&format!("        ca \"{acme_ca}\"\n"));
        out.push_str("    }\n");
        out.push_str("}\n");
    }

    // Site / proxy block (only when an upstream is provided).
    if let Some(upstream_addr) = &upstream {
        let site_host = host.as_deref().unwrap_or("*");
        out.push_str(&format!("site \"{site_host}\" {{\n"));
        out.push_str("    route \"/*\" {\n");
        out.push_str(&format!("        proxy \"{upstream_addr}\"\n"));
        out.push_str("    }\n");
        out.push_str("}\n");
    }

    Some(out)
}

// ---------------------------------------------------------------------------
// Global
// ---------------------------------------------------------------------------

fn parse_global(node: &KdlNode) -> Result<GlobalConfig, ConfigError> {
    let mut cfg = GlobalConfig::default();
    let Some(children) = node.children() else {
        return Ok(cfg);
    };
    for child in children.nodes() {
        match child.name().to_string().as_str() {
            "admin" => {
                if let Some(addr) = first_string_arg(child) {
                    cfg.admin_addr = Some(parse_listen_addr(&addr)?);
                }
            }
            "log" => {
                if let Some(level) = child.get("level") {
                    cfg.log_level = level
                        .as_string()
                        .ok_or_else(|| ConfigError::InvalidValue {
                            field: "log level".into(),
                            detail: "expected string".into(),
                        })?
                        .to_string();
                }
                if let Some(format) = child.get("format") {
                    cfg.log_format = format
                        .as_string()
                        .ok_or_else(|| ConfigError::InvalidValue {
                            field: "log format".into(),
                            detail: "expected string".into(),
                        })?
                        .to_string();
                }
            }
            "grace-period" => {
                if let Some(d) = first_string_arg(child) {
                    cfg.grace_period = parse_duration(&d)?;
                }
            }
            "http" => {
                if let Some(addr) = first_string_arg(child) {
                    cfg.http_addr = parse_listen_addr(&addr)?;
                }
            }
            "https" => {
                if let Some(addr) = first_string_arg(child) {
                    cfg.https_addr = parse_listen_addr(&addr)?;
                }
            }
            "http3" => {
                // `http3 true` or `http3 false`
                cfg.http3 = child
                    .entries()
                    .iter()
                    .find(|e| e.name().is_none())
                    .and_then(|e| e.value().as_bool())
                    .unwrap_or(true);
            }
            "proxy-protocol" => {
                cfg.proxy_protocol = child
                    .entries()
                    .iter()
                    .find(|e| e.name().is_none())
                    .and_then(|e| e.value().as_bool())
                    .unwrap_or(true);
            }
            "access-log" => {
                cfg.access_log = Some(parse_log_file_config(child)?);
            }
            "error-log" => {
                cfg.error_log = Some(parse_log_file_config(child)?);
            }
            "tcp-nodelay" => {
                cfg.tcp_nodelay = child
                    .entries()
                    .iter()
                    .find(|e| e.name().is_none())
                    .and_then(|e| e.value().as_bool())
                    .unwrap_or(true);
            }
            "tcp-send-buffer" => {
                cfg.tcp_send_buffer = child
                    .entries()
                    .iter()
                    .find(|e| e.name().is_none())
                    .and_then(|e| e.value().as_integer())
                    .map(|v| v as usize);
            }
            "tcp-recv-buffer" => {
                cfg.tcp_recv_buffer = child
                    .entries()
                    .iter()
                    .find(|e| e.name().is_none())
                    .and_then(|e| e.value().as_integer())
                    .map(|v| v as usize);
            }
            "otlp-endpoint" => {
                cfg.otlp_endpoint = first_string_arg(child);
            }
            "otlp-service-name" => {
                cfg.otlp_service_name = first_string_arg(child);
            }
            "admin-auth-token" => {
                cfg.admin_auth_token = first_string_arg(child);
            }
            other => return Err(ConfigError::UnknownDirective(other.into())),
        }
    }
    Ok(cfg)
}

fn parse_log_file_config(node: &KdlNode) -> Result<LogFileConfig, ConfigError> {
    let path =
        first_string_arg(node).ok_or_else(|| ConfigError::MissingField("log file path".into()))?;
    let mut format = None;
    let mut rotate_size = None;
    let mut rotate_keep = None;

    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().to_string().as_str() {
                "format" => {
                    format = first_string_arg(child);
                }
                "rotate-size" => {
                    rotate_size = child
                        .entries()
                        .iter()
                        .find(|e| e.name().is_none())
                        .and_then(|e| e.value().as_integer())
                        .map(|v| v as u64);
                }
                "rotate-keep" => {
                    rotate_keep = child
                        .entries()
                        .iter()
                        .find(|e| e.name().is_none())
                        .and_then(|e| e.value().as_integer())
                        .map(|v| v as usize);
                }
                _ => {}
            }
        }
    }

    Ok(LogFileConfig {
        path,
        format,
        rotate_size,
        rotate_keep,
    })
}

// ---------------------------------------------------------------------------
// TLS (global)
// ---------------------------------------------------------------------------

fn parse_tls(node: &KdlNode) -> Result<TlsConfig, ConfigError> {
    let mut tls = TlsConfig {
        acme: None,
        client_auth: None,
        on_demand: None,
        min_version: None,
        max_version: None,
        cipher_suites: Vec::new(),
        ocsp_stapling: false,
        ecdh_curves: Vec::new(),
    };
    let Some(children) = node.children() else {
        return Ok(tls);
    };
    for child in children.nodes() {
        match child.name().to_string().as_str() {
            "acme" => tls.acme = Some(parse_acme(child)?),
            "client-auth" => {
                let mut ca_certs = Vec::new();
                let required = child
                    .get("required")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                if let Some(cc) = child.children() {
                    for n in cc.nodes() {
                        if n.name().to_string() == "ca-cert"
                            && let Some(path) = first_string_arg(n)
                        {
                            ca_certs.push(path);
                        }
                    }
                }
                tls.client_auth = Some(ClientAuthConfig { ca_certs, required });
            }
            "on-demand" => {
                let ask = child
                    .get("ask")
                    .and_then(|v| v.as_string())
                    .map(|s| s.to_string());
                let rate_limit = child
                    .get("rate-limit")
                    .and_then(|v| v.as_integer())
                    .map(|v| v as u32);
                tls.on_demand = Some(OnDemandTlsConfig { ask, rate_limit });
            }
            "min-version" => {
                tls.min_version = first_string_arg(child);
            }
            "max-version" => {
                tls.max_version = first_string_arg(child);
            }
            "cipher-suites" => {
                // Accept space or comma separated suite names as positional args.
                let args = string_args(child);
                for arg in args {
                    for part in arg.split([' ', ',']) {
                        let part = part.trim();
                        if !part.is_empty() {
                            tls.cipher_suites.push(part.to_string());
                        }
                    }
                }
            }
            "ocsp-stapling" => {
                tls.ocsp_stapling = child
                    .entries()
                    .iter()
                    .find(|e| e.name().is_none())
                    .and_then(|e| e.value().as_bool())
                    .unwrap_or(true);
            }
            "ecdh-curves" => {
                // Accept space or comma separated curve names as positional args.
                let args = string_args(child);
                for arg in args {
                    for part in arg.split([' ', ',']) {
                        let part = part.trim();
                        if !part.is_empty() {
                            tls.ecdh_curves.push(part.to_string());
                        }
                    }
                }
            }
            other => return Err(ConfigError::UnknownDirective(other.into())),
        }
    }
    Ok(tls)
}

fn parse_acme(node: &KdlNode) -> Result<AcmeConfig, ConfigError> {
    let mut email = String::new();
    let mut ca = CertAuthority::default();
    let mut challenge = ChallengeType::default();
    let mut eab: Option<EabConfig> = None;
    let mut dns_provider: Option<DnsProviderConfig> = None;

    let Some(children) = node.children() else {
        return Err(ConfigError::MissingField("acme email".into()));
    };
    for child in children.nodes() {
        match child.name().to_string().as_str() {
            "email" => {
                email = first_string_arg(child)
                    .ok_or_else(|| ConfigError::MissingField("acme email value".into()))?;
            }
            "ca" => {
                let v = first_string_arg(child).unwrap_or_default();
                ca = match v.as_str() {
                    "letsencrypt" | "le" => CertAuthority::LetsEncrypt,
                    "letsencrypt-staging" | "le-staging" => CertAuthority::LetsEncryptStaging,
                    "zerossl" => CertAuthority::ZeroSsl,
                    _ => {
                        return Err(ConfigError::InvalidValue {
                            field: "ca".into(),
                            detail: format!("unknown CA: {v}"),
                        });
                    }
                };
            }
            "challenge" => {
                let v = first_string_arg(child).unwrap_or_default();
                challenge = match v.as_str() {
                    "http-01" => ChallengeType::Http01,
                    "tls-alpn-01" => ChallengeType::TlsAlpn01,
                    "dns-01" => ChallengeType::Dns01,
                    _ => {
                        return Err(ConfigError::InvalidValue {
                            field: "challenge".into(),
                            detail: format!("unknown challenge type: {v}"),
                        });
                    }
                };
            }
            "eab" => {
                eab = Some(parse_eab(child)?);
            }
            "dns-provider" => {
                dns_provider = Some(parse_dns_provider(child)?);
            }
            other => return Err(ConfigError::UnknownDirective(other.into())),
        }
    }
    if email.is_empty() {
        return Err(ConfigError::MissingField("acme email".into()));
    }
    Ok(AcmeConfig {
        email,
        ca,
        challenge,
        eab,
        dns_provider,
    })
}

fn parse_eab(node: &KdlNode) -> Result<EabConfig, ConfigError> {
    let mut kid = String::new();
    let mut hmac_key = String::new();
    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().to_string().as_str() {
                "kid" => kid = first_string_arg(child).unwrap_or_default(),
                "hmac-key" => hmac_key = first_string_arg(child).unwrap_or_default(),
                _ => {}
            }
        }
    }
    Ok(EabConfig { kid, hmac_key })
}

fn parse_dns_provider(node: &KdlNode) -> Result<DnsProviderConfig, ConfigError> {
    let provider = first_string_arg(node)
        .ok_or_else(|| ConfigError::MissingField("dns-provider name".into()))?;
    let mut api_token = None;
    let mut api_key = None;
    let mut api_secret = None;
    let mut options = HashMap::new();
    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().to_string().as_str() {
                "api-token" => api_token = first_string_arg(child),
                "api-key" => api_key = first_string_arg(child),
                "api-secret" => api_secret = first_string_arg(child),
                other => {
                    if let Some(val) = first_string_arg(child) {
                        options.insert(other.to_string(), val);
                    }
                }
            }
        }
    }
    Ok(DnsProviderConfig {
        provider,
        api_token,
        api_key,
        api_secret,
        options,
    })
}

// ---------------------------------------------------------------------------
// Site
// ---------------------------------------------------------------------------

fn parse_site(
    host: &str,
    node: &KdlNode,
    snippets: &HashMap<String, KdlNode>,
) -> Result<SiteConfig, ConfigError> {
    let mut site = SiteConfig {
        host: host.to_string(),
        tls: None,
        routes: Vec::new(),
    };
    let Some(children) = node.children() else {
        return Ok(site);
    };
    for child in children.nodes() {
        match child.name().to_string().as_str() {
            "tls" => site.tls = Some(parse_site_tls(child)?),
            "route" => {
                let path = first_string_arg(child)
                    .ok_or_else(|| ConfigError::MissingField("route path".into()))?;
                site.routes.push(parse_route(&path, child, snippets)?);
            }
            other => return Err(ConfigError::UnknownDirective(other.into())),
        }
    }
    Ok(site)
}

fn parse_site_tls(node: &KdlNode) -> Result<SiteTlsConfig, ConfigError> {
    let mut cert = String::new();
    let mut key = String::new();
    let Some(children) = node.children() else {
        return Err(ConfigError::MissingField("tls cert/key".into()));
    };
    for child in children.nodes() {
        match child.name().to_string().as_str() {
            "cert" => {
                cert = first_string_arg(child)
                    .ok_or_else(|| ConfigError::MissingField("tls cert path".into()))?;
            }
            "key" => {
                key = first_string_arg(child)
                    .ok_or_else(|| ConfigError::MissingField("tls key path".into()))?;
            }
            other => return Err(ConfigError::UnknownDirective(other.into())),
        }
    }
    if cert.is_empty() || key.is_empty() {
        return Err(ConfigError::MissingField("tls cert and key".into()));
    }
    Ok(SiteTlsConfig { cert, key })
}

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

fn parse_route(
    path: &str,
    node: &KdlNode,
    snippets: &HashMap<String, KdlNode>,
) -> Result<RouteConfig, ConfigError> {
    let mut middlewares = Vec::new();
    let mut matchers = Vec::new();
    let mut handler = None;
    let mut condition: Option<RouteCondition> = None;

    let Some(children) = node.children() else {
        return Err(ConfigError::MissingField("route handler".into()));
    };

    // Flatten `use "snippet-name"` directives by collecting all effective nodes.
    // We build an owned list so we can iterate without borrowing issues.
    let mut effective_nodes: Vec<KdlNode> = Vec::new();
    for child in children.nodes() {
        if child.name().to_string() == "use" {
            if let Some(snippet_name) = first_string_arg(child) {
                match snippets.get(&snippet_name) {
                    Some(snippet_node) => {
                        if let Some(snippet_children) = snippet_node.children() {
                            for sn in snippet_children.nodes() {
                                effective_nodes.push(sn.clone());
                            }
                        }
                    }
                    None => {
                        return Err(ConfigError::InvalidValue {
                            field: "use".into(),
                            detail: format!("unknown snippet: {snippet_name}"),
                        });
                    }
                }
            }
        } else {
            effective_nodes.push(child.clone());
        }
    }

    for child in &effective_nodes {
        match child.name().to_string().as_str() {
            "match" => {
                matchers.push(parse_matcher(child)?);
            }
            "if" => {
                condition = Some(parse_route_condition(child, false)?);
            }
            "if-not" => {
                condition = Some(parse_route_condition(child, true)?);
            }
            "rate-limit" => middlewares.push(parse_rate_limit(child)?),
            "encode" => middlewares.push(parse_encode(child)?),
            "basic-auth" => middlewares.push(parse_basic_auth(child)?),
            "cache" => middlewares.push(parse_cache(child)?),
            "ip-filter" => middlewares.push(parse_ip_filter(child)?),
            "rewrite" => middlewares.push(parse_rewrite(child)?),
            "replace" => middlewares.push(parse_replace(child)?),
            "forward-auth" => middlewares.push(parse_forward_auth(child)?),
            "buffer-limit" => middlewares.push(parse_buffer_limit(child)?),
            "cors" => middlewares.push(parse_cors(child)?),
            "timeout" => middlewares.push(parse_timeout(child)?),
            "request-id" => middlewares.push(parse_request_id(child)?),
            "force-https" => middlewares.push(parse_force_https(child)?),
            "trailing-slash" => middlewares.push(parse_trailing_slash(child)?),
            "decompress" => {
                let max_size = child
                    .get("max-size")
                    .and_then(|v| v.as_integer())
                    .map(|v| v as usize);
                middlewares.push(HoopConfig::Decompress { max_size });
            }
            "error-pages" => middlewares.push(parse_error_pages(child)?),
            "stream-replace" => middlewares.push(parse_stream_replace(child)?),
            "templates" => {
                let root = first_string_arg(child).or_else(|| {
                    child
                        .get("root")
                        .and_then(|v| v.as_string())
                        .map(|s| s.to_string())
                });
                middlewares.push(HoopConfig::Templates { root });
            }
            "header-up" | "header-down" => {
                // Collect header directives — handled below with the proxy
                // For now, these are part of the proxy config, skip here.
            }
            "proxy" => handler = Some(HandlerConfig::Proxy(parse_proxy(child)?)),
            "fastcgi" => handler = Some(HandlerConfig::FastCgi(parse_fastcgi(child)?)),
            "forward-proxy" => {
                handler = Some(HandlerConfig::ForwardProxy(parse_forward_proxy(child)?));
            }
            "cgi" => handler = Some(HandlerConfig::Cgi(parse_cgi(child)?)),
            "scgi" => handler = Some(HandlerConfig::Scgi(parse_scgi(child)?)),
            "file-server" => {
                // file-server has no children usually; root is set separately
                // We handle it as: find "root" sibling, then file-server node.
            }
            "root" => {
                // root sets the directory for file-server
            }
            "redirect" => {
                let to = first_string_arg(child)
                    .ok_or_else(|| ConfigError::MissingField("redirect target".into()))?;
                let permanent = child
                    .get("permanent")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                handler = Some(HandlerConfig::Redirect { to, permanent });
            }
            "respond" => {
                let body = first_string_arg(child).unwrap_or_default();
                let status = child
                    .get("status")
                    .and_then(|v| v.as_integer())
                    .unwrap_or(200) as u16;
                handler = Some(HandlerConfig::Respond { status, body });
            }
            other => {
                // Try to interpret as a module middleware directive.
                let mut config = HashMap::new();
                // Collect all named entries as key-value config.
                for entry in child.entries() {
                    if let Some(name) = entry.name() {
                        if let Some(val) = entry.value().as_string() {
                            config.insert(name.to_string(), val.to_string());
                        } else if let Some(val) = entry.value().as_integer() {
                            config.insert(name.to_string(), val.to_string());
                        } else if let Some(val) = entry.value().as_bool() {
                            config.insert(name.to_string(), val.to_string());
                        }
                    }
                }
                // Also collect children as nested config
                if let Some(children) = child.children() {
                    for c in children.nodes() {
                        if let Some(val) = first_string_arg(c) {
                            config.insert(c.name().to_string(), val);
                        }
                    }
                }
                middlewares.push(HoopConfig::Module {
                    name: other.to_string(),
                    config,
                });
            }
        }
    }

    // Handle file-server: look for root + file-server pair
    if handler.is_none() {
        let fs_node = effective_nodes
            .iter()
            .find(|n| n.name().to_string() == "file-server");
        if let Some(fs_node) = fs_node {
            let root = effective_nodes
                .iter()
                .find(|n| n.name().to_string() == "root")
                .and_then(first_string_arg)
                .unwrap_or_else(|| ".".into());
            let browse = fs_node
                .get("browse")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let trailing_slash = fs_node
                .get("trailing-slash")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let mut index: Vec<String> = Vec::new();
            if let Some(fs_children) = fs_node.children() {
                for n in fs_children.nodes() {
                    if n.name().to_string() == "index" {
                        for arg in string_args(n) {
                            index.push(arg);
                        }
                    }
                }
            }
            if index.is_empty() {
                index.push("index.html".to_string());
            }
            handler = Some(HandlerConfig::FileServer(FileServerConfig {
                root,
                browse,
                trailing_slash,
                index,
            }));
        }
    }

    // If no built-in handler matched, check for any module handler among the
    // nodes that were collected as module middleware but that might be the
    // terminal handler (i.e. there is no built-in handler and the module
    // loader would provide one).  We promote the last Module middleware entry
    // that could serve as a handler into HandlerConfig::Module.
    if handler.is_none() {
        // Look for any unknown directive that was not a known middleware directive.
        // Since unknown directives are already collected as Module middlewares,
        // we scan effective_nodes for any node not matching known directives.
        for n in &effective_nodes {
            let name = n.name().to_string();
            if !is_known_directive(&name) {
                let mut config = HashMap::new();
                for entry in n.entries() {
                    if let Some(ename) = entry.name()
                        && let Some(val) = entry.value().as_string()
                    {
                        config.insert(ename.to_string(), val.to_string());
                    }
                }
                handler = Some(HandlerConfig::Module { name, config });
                break;
            }
        }
    }

    let handler =
        handler.ok_or_else(|| ConfigError::MissingField("route must have a handler".into()))?;

    Ok(RouteConfig {
        path: path.to_string(),
        matchers,
        middlewares,
        handler,
        condition,
    })
}

/// Returns true for directive names that are handled natively by gatel.
/// Unknown directives are treated as potential module middleware or handlers.
fn is_known_directive(name: &str) -> bool {
    matches!(
        name,
        "match"
            | "rate-limit"
            | "encode"
            | "basic-auth"
            | "cache"
            | "templates"
            | "header-up"
            | "header-down"
            | "proxy"
            | "fastcgi"
            | "forward-proxy"
            | "cgi"
            | "scgi"
            | "file-server"
            | "root"
            | "redirect"
            | "respond"
            | "ip-filter"
            | "rewrite"
            | "replace"
            | "forward-auth"
            | "headers"
            | "buffer-limit"
            | "cors"
            | "timeout"
            | "request-id"
            | "force-https"
            | "trailing-slash"
            | "decompress"
            | "error-pages"
            | "stream-replace"
            | "use"
            | "if"
            | "if-not"
    )
}

/// Parse an `if` or `if-not` condition node.
///
/// Supported attribute forms:
/// - `if remote-ip="10.0.0.0/8"`
/// - `if header="X-Internal" value="yes"`
fn parse_route_condition(node: &KdlNode, negate: bool) -> Result<RouteCondition, ConfigError> {
    if let Some(cidr) = node.get("remote-ip").and_then(|v| v.as_string()) {
        let cidrs: Vec<String> = cidr.split(',').map(|s| s.trim().to_string()).collect();
        return Ok(if negate {
            RouteCondition::NotRemoteIp(cidrs)
        } else {
            RouteCondition::RemoteIp(cidrs)
        });
    }

    if let Some(name) = node.get("header").and_then(|v| v.as_string()) {
        let value = node
            .get("value")
            .and_then(|v| v.as_string())
            .unwrap_or("")
            .to_string();
        return Ok(if negate {
            RouteCondition::NotHeader {
                name: name.to_string(),
                value,
            }
        } else {
            RouteCondition::Header {
                name: name.to_string(),
                value,
            }
        });
    }

    Err(ConfigError::InvalidValue {
        field: if negate { "if-not" } else { "if" }.into(),
        detail: "expected remote-ip or header attribute".into(),
    })
}

/// Parse a `forward-proxy` handler node.
///
/// ```kdl
/// forward-proxy {
///     user "alice" hash="$2b$..."
///     user "bob"   hash="plaintext"
/// }
/// ```
fn parse_forward_proxy(node: &KdlNode) -> Result<ForwardProxyConfig, ConfigError> {
    let mut auth_users = Vec::new();
    if let Some(children) = node.children() {
        for child in children.nodes() {
            if child.name().to_string() == "user" {
                let username = first_string_arg(child).ok_or_else(|| {
                    ConfigError::MissingField("forward-proxy user username".into())
                })?;
                let password_hash = child
                    .get("hash")
                    .and_then(|v| v.as_string())
                    .unwrap_or("")
                    .to_string();
                auth_users.push(BasicAuthUser {
                    username,
                    password_hash,
                });
            }
        }
    }
    Ok(ForwardProxyConfig { auth_users })
}

// ---------------------------------------------------------------------------
// Proxy
// ---------------------------------------------------------------------------

fn parse_proxy(node: &KdlNode) -> Result<ProxyConfig, ConfigError> {
    // Simple form: `proxy "host:port"` — single upstream, no children
    if let Some(addr) = first_string_arg(node)
        && node.children().is_none()
    {
        return Ok(ProxyConfig {
            upstreams: vec![UpstreamConfig { addr, weight: 1 }],
            lb: LbPolicy::default(),
            lb_header: None,
            lb_cookie: None,
            health_check: None,
            passive_health: None,
            headers_up: HashMap::new(),
            headers_down: HashMap::new(),
            retries: 0,
            dynamic_upstreams: None,
            error_pages: HashMap::new(),
            headers_up_replace: Vec::new(),
            tls_skip_verify: false,
            upstream_http2: false,
            max_connections: None,
            keepalive_timeout: None,
            sanitize_uri: true,
            srv_upstream: None,
        });
    }

    let mut upstreams = Vec::new();
    let mut lb = LbPolicy::default();
    let mut lb_header = None;
    let mut lb_cookie = None;
    let mut health_check = None;
    let mut passive_health = None;
    let mut headers_up = HashMap::new();
    let mut headers_down = HashMap::new();
    let mut retries = 0u32;
    let mut dynamic_upstreams = None;
    let mut error_pages: HashMap<u16, String> = HashMap::new();
    let mut headers_up_replace: Vec<(String, String, String)> = Vec::new();
    let mut tls_skip_verify = false;
    let mut upstream_http2 = false;
    let mut max_connections: Option<usize> = None;
    let mut keepalive_timeout: Option<Duration> = None;
    let mut sanitize_uri = true;
    let mut srv_upstream: Option<SrvUpstreamConfig> = None;

    let Some(children) = node.children() else {
        return Err(ConfigError::MissingField("proxy upstream".into()));
    };
    for child in children.nodes() {
        match child.name().to_string().as_str() {
            "upstream" => {
                let addr = first_string_arg(child)
                    .ok_or_else(|| ConfigError::MissingField("upstream address".into()))?;
                let weight = child
                    .get("weight")
                    .and_then(|v| v.as_integer())
                    .unwrap_or(1) as u32;
                upstreams.push(UpstreamConfig { addr, weight });
            }
            "lb" => {
                let policy = first_string_arg(child).unwrap_or_default();
                lb = match policy.as_str() {
                    "round_robin" => LbPolicy::RoundRobin,
                    "random" => LbPolicy::Random,
                    "weighted_round_robin" => LbPolicy::WeightedRoundRobin,
                    "ip_hash" => LbPolicy::IpHash,
                    "least_conn" => LbPolicy::LeastConn,
                    "uri_hash" => LbPolicy::UriHash,
                    "header_hash" => LbPolicy::HeaderHash,
                    "cookie_hash" => LbPolicy::CookieHash,
                    "first" => LbPolicy::First,
                    "two_random_choices" => LbPolicy::TwoRandomChoices,
                    _ => {
                        return Err(ConfigError::InvalidValue {
                            field: "lb".into(),
                            detail: format!("unknown policy: {policy}"),
                        });
                    }
                };
                // Read optional header/cookie name from the same node
                if let Some(h) = child.get("header").and_then(|v| v.as_string()) {
                    lb_header = Some(h.to_string());
                }
                if let Some(c) = child.get("cookie").and_then(|v| v.as_string()) {
                    lb_cookie = Some(c.to_string());
                }
            }
            "health-check" => {
                let uri = child
                    .get("uri")
                    .and_then(|v| v.as_string())
                    .unwrap_or("/health")
                    .to_string();
                let interval = child
                    .get("interval")
                    .and_then(|v| v.as_string())
                    .map(parse_duration)
                    .transpose()?
                    .unwrap_or(Duration::from_secs(10));
                let timeout = child
                    .get("timeout")
                    .and_then(|v| v.as_string())
                    .map(parse_duration)
                    .transpose()?
                    .unwrap_or(Duration::from_secs(5));
                let unhealthy_threshold = child
                    .get("unhealthy-threshold")
                    .and_then(|v| v.as_integer())
                    .unwrap_or(3) as u32;
                let healthy_threshold = child
                    .get("healthy-threshold")
                    .and_then(|v| v.as_integer())
                    .unwrap_or(2) as u32;
                health_check = Some(HealthCheckConfig {
                    uri,
                    interval,
                    timeout,
                    unhealthy_threshold,
                    healthy_threshold,
                });
            }
            "passive-health" => {
                let max_fails = child
                    .get("max-fails")
                    .and_then(|v| v.as_integer())
                    .unwrap_or(5) as u32;
                let fail_window = child
                    .get("fail-window")
                    .and_then(|v| v.as_string())
                    .map(parse_duration)
                    .transpose()?
                    .unwrap_or(Duration::from_secs(30));
                let cooldown = child
                    .get("cooldown")
                    .and_then(|v| v.as_string())
                    .map(parse_duration)
                    .transpose()?
                    .unwrap_or(Duration::from_secs(60));
                passive_health = Some(PassiveHealthConfig {
                    max_fails,
                    fail_window,
                    cooldown,
                });
            }
            "retries" => {
                retries = first_string_arg(child)
                    .or_else(|| {
                        child
                            .entries()
                            .iter()
                            .find(|e| e.name().is_none())
                            .and_then(|e| e.value().as_integer())
                            .map(|i| i.to_string())
                    })
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(0);
            }
            "header-up" => {
                let entries: Vec<String> = string_args(child);
                if entries.len() >= 2 {
                    headers_up.insert(entries[0].clone(), entries[1].clone());
                } else if entries.len() == 1 && entries[0].starts_with('-') {
                    headers_up.insert(entries[0].clone(), String::new());
                }
            }
            "header-down" => {
                let entries: Vec<String> = string_args(child);
                if entries.len() >= 2 {
                    headers_down.insert(entries[0].clone(), entries[1].clone());
                } else if entries.len() == 1 && entries[0].starts_with('-') {
                    headers_down.insert(entries[0].clone(), String::new());
                }
            }
            "dns-upstream" => {
                let dns_name = child
                    .get("name")
                    .and_then(|v| v.as_string())
                    .map(|s| s.to_string())
                    .or_else(|| first_string_arg(child))
                    .ok_or_else(|| ConfigError::MissingField("dns-upstream name".into()))?;
                let port = child.get("port").and_then(|v| v.as_integer()).unwrap_or(80) as u16;
                let refresh_interval = child
                    .get("refresh")
                    .and_then(|v| v.as_string())
                    .map(parse_duration)
                    .transpose()?
                    .unwrap_or(Duration::from_secs(30));
                dynamic_upstreams = Some(DnsUpstreamConfig {
                    dns_name,
                    port,
                    refresh_interval,
                });
            }
            "error-page" => {
                // `error-page 502 "Bad Gateway - upstream unreachable"`
                let entries = child.entries();
                let positional: Vec<_> = entries.iter().filter(|e| e.name().is_none()).collect();
                if positional.len() >= 2
                    && let (Some(code), Some(body)) = (
                        positional[0].value().as_integer(),
                        positional[1].value().as_string(),
                    )
                {
                    error_pages.insert(code as u16, body.to_string());
                }
            }
            "header-up-replace" => {
                // `header-up-replace "Host" "old.example.com" "new.example.com"`
                let args = string_args(child);
                if args.len() >= 3 {
                    headers_up_replace.push((args[0].clone(), args[1].clone(), args[2].clone()));
                }
            }
            "tls-skip-verify" => {
                tls_skip_verify = child.get(0).and_then(|v| v.as_bool()).unwrap_or(true);
            }
            "http2" => {
                upstream_http2 = child.get(0).and_then(|v| v.as_bool()).unwrap_or(true);
            }
            "max-connections" => {
                max_connections = child
                    .get(0)
                    .and_then(|v| v.as_integer())
                    .map(|v| v as usize);
            }
            "keepalive" => {
                keepalive_timeout = child
                    .get(0)
                    .and_then(|v| v.as_string())
                    .map(parse_duration)
                    .transpose()?;
            }
            "sanitize-uri" => {
                sanitize_uri = child
                    .entries()
                    .iter()
                    .find(|e| e.name().is_none())
                    .and_then(|e| e.value().as_bool())
                    .unwrap_or(true);
            }
            "srv-upstream" => {
                let service_name = child
                    .get("name")
                    .and_then(|v| v.as_string())
                    .map(|s| s.to_string())
                    .or_else(|| first_string_arg(child))
                    .ok_or_else(|| ConfigError::MissingField("srv-upstream name".into()))?;
                let refresh_interval = child
                    .get("refresh")
                    .and_then(|v| v.as_string())
                    .map(parse_duration)
                    .transpose()?
                    .unwrap_or(Duration::from_secs(30));
                srv_upstream = Some(SrvUpstreamConfig {
                    service_name,
                    refresh_interval,
                });
            }
            _ => {}
        }
    }
    if upstreams.is_empty() && dynamic_upstreams.is_none() && srv_upstream.is_none() {
        return Err(ConfigError::MissingField(
            "proxy must have at least one upstream, a dns-upstream, or srv-upstream".into(),
        ));
    }
    Ok(ProxyConfig {
        upstreams,
        lb,
        lb_header,
        lb_cookie,
        health_check,
        passive_health,
        headers_up,
        headers_down,
        retries,
        dynamic_upstreams,
        error_pages,
        headers_up_replace,
        tls_skip_verify,
        upstream_http2,
        max_connections,
        keepalive_timeout,
        sanitize_uri,
        srv_upstream,
    })
}

// ---------------------------------------------------------------------------
// Middleware parsers
// ---------------------------------------------------------------------------

fn parse_rate_limit(node: &KdlNode) -> Result<HoopConfig, ConfigError> {
    let window = node
        .get("window")
        .and_then(|v| v.as_string())
        .map(parse_duration)
        .transpose()?
        .unwrap_or(Duration::from_secs(60));
    let max = node.get("max").and_then(|v| v.as_integer()).unwrap_or(100) as u64;
    let burst = node
        .get("burst")
        .and_then(|v| v.as_integer())
        .map(|v| v as u64);
    Ok(HoopConfig::RateLimit { window, max, burst })
}

fn parse_encode(node: &KdlNode) -> Result<HoopConfig, ConfigError> {
    let encodings = string_args(node);
    let level = node
        .get("level")
        .and_then(|v| v.as_integer())
        .map(|v| v as u32);
    if encodings.is_empty() {
        return Ok(HoopConfig::Encode {
            encodings: vec!["gzip".into()],
            level,
        });
    }
    Ok(HoopConfig::Encode { encodings, level })
}

fn parse_basic_auth(node: &KdlNode) -> Result<HoopConfig, ConfigError> {
    let mut users = Vec::new();
    let mut brute_force_max: Option<u32> = None;
    let mut brute_force_window: Option<Duration> = None;

    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().to_string().as_str() {
                "user" => {
                    let username = first_string_arg(child)
                        .ok_or_else(|| ConfigError::MissingField("basic-auth username".into()))?;
                    let password_hash = child
                        .get("hash")
                        .and_then(|v| v.as_string())
                        .unwrap_or("")
                        .to_string();
                    users.push(BasicAuthUser {
                        username,
                        password_hash,
                    });
                }
                "brute-force-max" => {
                    brute_force_max = child
                        .entries()
                        .iter()
                        .find(|e| e.name().is_none())
                        .and_then(|e| e.value().as_integer())
                        .map(|v| v as u32);
                }
                "brute-force-window" => {
                    brute_force_window = first_string_arg(child)
                        .map(|s| parse_duration(&s))
                        .transpose()?;
                }
                _ => {}
            }
        }
    }
    Ok(HoopConfig::BasicAuth {
        users,
        brute_force_max,
        brute_force_window,
    })
}

fn parse_ip_filter(node: &KdlNode) -> Result<HoopConfig, ConfigError> {
    let mut allow = Vec::new();
    let mut deny = Vec::new();
    let forwarded_for = node
        .get("forwarded-for")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().to_string().as_str() {
                "allow" => {
                    for s in string_args(child) {
                        allow.push(s);
                    }
                }
                "deny" => {
                    for s in string_args(child) {
                        deny.push(s);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(HoopConfig::IpFilter {
        allow,
        deny,
        forwarded_for,
    })
}

fn parse_rewrite(node: &KdlNode) -> Result<HoopConfig, ConfigError> {
    let mut strip_prefix = None;
    let mut uri = None;
    let mut regex_rules = Vec::new();
    let mut if_not_file = false;
    let mut if_not_dir = false;
    let mut root = None;
    let mut normalize_slashes = false;

    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().to_string().as_str() {
                "strip-prefix" => {
                    strip_prefix = first_string_arg(child);
                }
                "uri" => {
                    uri = first_string_arg(child);
                }
                "regex" => {
                    let args = string_args(child);
                    if args.len() >= 2 {
                        regex_rules.push((args[0].clone(), args[1].clone()));
                    }
                }
                "if-not-file" => {
                    if_not_file = child
                        .entries()
                        .iter()
                        .find(|e| e.name().is_none())
                        .and_then(|e| e.value().as_bool())
                        .unwrap_or(true);
                }
                "if-not-dir" => {
                    if_not_dir = child
                        .entries()
                        .iter()
                        .find(|e| e.name().is_none())
                        .and_then(|e| e.value().as_bool())
                        .unwrap_or(true);
                }
                "root" => {
                    root = first_string_arg(child);
                }
                "normalize-slashes" => {
                    normalize_slashes = child
                        .entries()
                        .iter()
                        .find(|e| e.name().is_none())
                        .and_then(|e| e.value().as_bool())
                        .unwrap_or(true);
                }
                _ => {}
            }
        }
    }

    Ok(HoopConfig::Rewrite {
        strip_prefix,
        uri,
        regex_rules,
        if_not_file,
        if_not_dir,
        root,
        normalize_slashes,
    })
}

fn parse_replace(node: &KdlNode) -> Result<HoopConfig, ConfigError> {
    let mut rules = Vec::new();
    let mut once = false;

    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().to_string().as_str() {
                "rule" => {
                    let args = string_args(child);
                    if args.len() >= 2 {
                        rules.push((args[0].clone(), args[1].clone()));
                    }
                }
                "once" => {
                    once = child
                        .entries()
                        .iter()
                        .find(|e| e.name().is_none())
                        .and_then(|e| e.value().as_bool())
                        .unwrap_or(true);
                }
                _ => {}
            }
        }
    }

    Ok(HoopConfig::Replace { rules, once })
}

fn parse_cache(node: &KdlNode) -> Result<HoopConfig, ConfigError> {
    let mut cfg = CacheConfig::default();
    if let Some(v) = node.get("max-entries").and_then(|v| v.as_integer()) {
        cfg.max_entries = v as usize;
    }
    if let Some(v) = node.get("max-entry-size").and_then(|v| v.as_integer()) {
        cfg.max_entry_size = v as usize;
    }
    if let Some(v) = node.get("max-age").and_then(|v| v.as_string()) {
        cfg.default_max_age = parse_duration(v)?;
    }
    Ok(HoopConfig::Cache(cfg))
}

fn parse_forward_auth(node: &KdlNode) -> Result<HoopConfig, ConfigError> {
    let url = first_string_arg(node)
        .ok_or_else(|| ConfigError::MissingField("forward-auth url".into()))?;

    let mut copy_headers = Vec::new();
    if let Some(children) = node.children() {
        for child in children.nodes() {
            if child.name().to_string() == "copy-headers" {
                for header in string_args(child) {
                    copy_headers.push(header);
                }
            }
        }
    }

    Ok(HoopConfig::ForwardAuth { url, copy_headers })
}

fn parse_buffer_limit(node: &KdlNode) -> Result<HoopConfig, ConfigError> {
    let max_request_body = node
        .get("max-request-body")
        .and_then(|v| v.as_integer())
        .map(|v| v as usize);
    let max_response_body = node
        .get("max-response-body")
        .and_then(|v| v.as_integer())
        .map(|v| v as usize);
    Ok(HoopConfig::BufferLimit {
        max_request_body,
        max_response_body,
    })
}

fn parse_cors(node: &KdlNode) -> Result<HoopConfig, ConfigError> {
    let mut allow_origins = Vec::new();
    let mut allow_methods = Vec::new();
    let mut allow_headers = Vec::new();
    let mut allow_credentials = false;
    let mut expose_headers = Vec::new();
    let mut max_age = None;

    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().to_string().as_str() {
                "allow-origin" => {
                    allow_origins.extend(string_args(child));
                }
                "allow-method" => {
                    allow_methods.extend(string_args(child));
                }
                "allow-header" => {
                    allow_headers.extend(string_args(child));
                }
                "allow-credentials" => {
                    allow_credentials = child
                        .entries()
                        .iter()
                        .find(|e| e.name().is_none())
                        .and_then(|e| e.value().as_bool())
                        .unwrap_or(true);
                }
                "expose-header" => {
                    expose_headers.extend(string_args(child));
                }
                "max-age" => {
                    max_age = child
                        .entries()
                        .iter()
                        .find(|e| e.name().is_none())
                        .and_then(|e| e.value().as_integer())
                        .map(|v| v as u64);
                }
                _ => {}
            }
        }
    }

    // Default to permissive if nothing specified.
    if allow_origins.is_empty() {
        allow_origins.push("*".into());
    }
    if allow_methods.is_empty() {
        allow_methods.push("*".into());
    }
    if allow_headers.is_empty() {
        allow_headers.push("*".into());
    }

    Ok(HoopConfig::Cors {
        allow_origins,
        allow_methods,
        allow_headers,
        allow_credentials,
        expose_headers,
        max_age,
    })
}

fn parse_timeout(node: &KdlNode) -> Result<HoopConfig, ConfigError> {
    let duration = first_string_arg(node)
        .map(|s| parse_duration(&s))
        .transpose()?
        .unwrap_or(Duration::from_secs(30));
    Ok(HoopConfig::Timeout { duration })
}

fn parse_request_id(node: &KdlNode) -> Result<HoopConfig, ConfigError> {
    let header_name = node
        .get("header")
        .and_then(|v| v.as_string())
        .map(|s| s.to_string());
    let overwrite = node
        .get("overwrite")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    Ok(HoopConfig::RequestId {
        header_name,
        overwrite,
    })
}

fn parse_force_https(node: &KdlNode) -> Result<HoopConfig, ConfigError> {
    let https_port = node
        .get("port")
        .and_then(|v| v.as_integer())
        .map(|v| v as u16)
        .or_else(|| first_string_arg(node).and_then(|s| s.parse::<u16>().ok()));
    Ok(HoopConfig::ForceHttps { https_port })
}

fn parse_trailing_slash(node: &KdlNode) -> Result<HoopConfig, ConfigError> {
    let action = first_string_arg(node).unwrap_or_else(|| "add".to_string());
    if action != "add" && action != "remove" {
        return Err(ConfigError::InvalidValue {
            field: "trailing-slash".into(),
            detail: format!("action must be 'add' or 'remove', got '{action}'"),
        });
    }
    Ok(HoopConfig::TrailingSlash { action })
}

fn parse_error_pages(node: &KdlNode) -> Result<HoopConfig, ConfigError> {
    let mut pages = std::collections::HashMap::new();
    if let Some(children) = node.children() {
        for child in children.nodes() {
            let code_str = child.name().to_string();
            if let Ok(code) = code_str.parse::<u16>()
                && let Some(body) = first_string_arg(child)
            {
                pages.insert(code, body);
            }
        }
    }
    Ok(HoopConfig::ErrorPages { pages })
}

fn parse_stream_replace(node: &KdlNode) -> Result<HoopConfig, ConfigError> {
    let mut rules = Vec::new();
    let mut once = false;
    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().to_string().as_str() {
                "rule" => {
                    let args = string_args(child);
                    if args.len() >= 2 {
                        rules.push((args[0].clone(), args[1].clone()));
                    }
                }
                "once" => {
                    once = child
                        .entries()
                        .iter()
                        .find(|e| e.name().is_none())
                        .and_then(|e| e.value().as_bool())
                        .unwrap_or(true);
                }
                _ => {}
            }
        }
    }
    Ok(HoopConfig::StreamReplace { rules, once })
}

// ---------------------------------------------------------------------------
// Matcher parser
// ---------------------------------------------------------------------------

/// Parse a `match` directive into a `RequestMatcher`.
///
/// Supported forms:
/// - `match method="GET"`
/// - `match path="/api/*"`
/// - `match header="X-Custom" pattern="foo*"`
/// - `match query="key" value="val"`
/// - `match remote-ip="192.168.0.0/16"`
/// - `match protocol="https"`
/// - `match expression="{method} == GET && {path} ~ /api/*"`
/// - `match not { ... }` (with children containing inner matcher)
fn parse_matcher(node: &KdlNode) -> Result<RequestMatcher, ConfigError> {
    // Check for keyword-style matchers.
    if let Some(method) = node.get("method").and_then(|v| v.as_string()) {
        let methods: Vec<String> = method.split(',').map(|s| s.trim().to_string()).collect();
        return Ok(RequestMatcher::Method(methods));
    }
    if let Some(path) = node.get("path").and_then(|v| v.as_string()) {
        return Ok(RequestMatcher::Path(path.to_string()));
    }
    if let Some(header_name) = node.get("header").and_then(|v| v.as_string()) {
        let pattern = node
            .get("pattern")
            .and_then(|v| v.as_string())
            .unwrap_or("*")
            .to_string();
        return Ok(RequestMatcher::Header {
            name: header_name.to_string(),
            pattern,
        });
    }
    if let Some(key) = node.get("query").and_then(|v| v.as_string()) {
        let value = node
            .get("value")
            .and_then(|v| v.as_string())
            .map(|s| s.to_string());
        return Ok(RequestMatcher::Query {
            key: key.to_string(),
            value,
        });
    }
    if let Some(cidr) = node.get("remote-ip").and_then(|v| v.as_string()) {
        let cidrs: Vec<String> = cidr.split(',').map(|s| s.trim().to_string()).collect();
        return Ok(RequestMatcher::RemoteIp(cidrs));
    }
    if let Some(proto) = node.get("protocol").and_then(|v| v.as_string()) {
        return Ok(RequestMatcher::Protocol(proto.to_string()));
    }
    if let Some(expr) = node.get("expression").and_then(|v| v.as_string()) {
        return Ok(RequestMatcher::Expression(expr.to_string()));
    }
    if let Some(lang) = node.get("language").and_then(|v| v.as_string()) {
        let langs: Vec<String> = lang.split(',').map(|s| s.trim().to_string()).collect();
        return Ok(RequestMatcher::Language(langs));
    }

    // Check for "not" or composite matchers via children.
    let kind = first_string_arg(node).unwrap_or_default();
    if kind == "not"
        && let Some(children) = node.children()
        && let Some(child) = children.nodes().first()
    {
        let inner = parse_matcher(child)?;
        return Ok(RequestMatcher::Not(Box::new(inner)));
    }

    // Default to a path matcher using the first positional arg.
    if !kind.is_empty() {
        return Ok(RequestMatcher::Path(kind));
    }

    Err(ConfigError::InvalidValue {
        field: "match".into(),
        detail: "unrecognized matcher format".into(),
    })
}

// ---------------------------------------------------------------------------
// FastCGI parser
// ---------------------------------------------------------------------------

/// Parse a `fastcgi` handler directive.
///
/// ```kdl
/// fastcgi "127.0.0.1:9000" {
///     script-root "/var/www/html"
///     split ".php"
///     index "index.php"
///     env "SERVER_SOFTWARE" "gatel"
/// }
/// ```
fn parse_fastcgi(node: &KdlNode) -> Result<FastCgiConfig, ConfigError> {
    let addr = first_string_arg(node)
        .ok_or_else(|| ConfigError::MissingField("fastcgi address".into()))?;
    let mut script_root = String::new();
    let mut index = Vec::new();
    let mut split_path = None;
    let mut env = HashMap::new();

    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().to_string().as_str() {
                "script-root" | "root" => {
                    script_root = first_string_arg(child).unwrap_or_default();
                }
                "split" => {
                    split_path = first_string_arg(child);
                }
                "index" => {
                    for arg in string_args(child) {
                        index.push(arg);
                    }
                }
                "env" => {
                    let args = string_args(child);
                    if args.len() >= 2 {
                        env.insert(args[0].clone(), args[1].clone());
                    }
                }
                _ => {}
            }
        }
    }

    if index.is_empty() {
        index.push("index.php".to_string());
    }

    Ok(FastCgiConfig {
        addr,
        script_root,
        index,
        split_path,
        env,
    })
}

// ---------------------------------------------------------------------------
// CGI parser
// ---------------------------------------------------------------------------

/// Parse a `cgi` handler directive.
///
/// ```kdl
/// cgi "/var/www/cgi-bin" {
///     env "APP_ENV" "production"
/// }
/// ```
fn parse_cgi(node: &KdlNode) -> Result<CgiConfig, ConfigError> {
    let root = first_string_arg(node)
        .or_else(|| {
            node.get("root")
                .and_then(|v| v.as_string())
                .map(|s| s.to_string())
        })
        .ok_or_else(|| ConfigError::MissingField("cgi root directory".into()))?;
    let mut env = HashMap::new();

    if let Some(children) = node.children() {
        for child in children.nodes() {
            if child.name().to_string().as_str() == "env" {
                let args = string_args(child);
                if args.len() >= 2 {
                    env.insert(args[0].clone(), args[1].clone());
                }
            }
        }
    }

    Ok(CgiConfig { root, env })
}

// ---------------------------------------------------------------------------
// SCGI parser
// ---------------------------------------------------------------------------

/// Parse an `scgi` handler directive.
///
/// ```kdl
/// scgi "127.0.0.1:9000" {
///     env "APP_ENV" "production"
/// }
/// ```
fn parse_scgi(node: &KdlNode) -> Result<ScgiConfig, ConfigError> {
    let addr =
        first_string_arg(node).ok_or_else(|| ConfigError::MissingField("scgi address".into()))?;
    let mut env = HashMap::new();

    if let Some(children) = node.children() {
        for child in children.nodes() {
            if child.name().to_string().as_str() == "env" {
                let args = string_args(child);
                if args.len() >= 2 {
                    env.insert(args[0].clone(), args[1].clone());
                }
            }
        }
    }

    Ok(ScgiConfig { addr, env })
}

// ---------------------------------------------------------------------------
// Stream
// ---------------------------------------------------------------------------

fn parse_stream(node: &KdlNode) -> Result<StreamConfig, ConfigError> {
    let mut listeners = Vec::new();
    let Some(children) = node.children() else {
        return Ok(StreamConfig { listeners });
    };
    for child in children.nodes() {
        if child.name().to_string() == "listen" {
            let addr_str = first_string_arg(child)
                .ok_or_else(|| ConfigError::MissingField("stream listen address".into()))?;
            let listen = parse_listen_addr(&addr_str)?;
            let proxy = child
                .children()
                .and_then(|c| {
                    c.nodes()
                        .iter()
                        .find(|n| n.name().to_string() == "proxy")
                        .and_then(first_string_arg)
                })
                .ok_or_else(|| ConfigError::MissingField("stream proxy target".into()))?;
            listeners.push(StreamListenerConfig { listen, proxy });
        }
    }
    Ok(StreamConfig { listeners })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Get the first positional string argument of a KDL node.
fn first_string_arg(node: &KdlNode) -> Option<String> {
    node.entries()
        .iter()
        .find(|e| e.name().is_none())
        .and_then(|e| e.value().as_string())
        .map(|s| s.to_string())
}

/// Get all positional string arguments.
fn string_args(node: &KdlNode) -> Vec<String> {
    node.entries()
        .iter()
        .filter(|e| e.name().is_none())
        .filter_map(|e| e.value().as_string().map(|s| s.to_string()))
        .collect()
}

/// Parse a duration string like "30s", "1m", "10s".
fn parse_duration(s: &str) -> Result<Duration, ConfigError> {
    let s = s.trim();
    if let Some(secs) = s.strip_suffix('s') {
        let n: u64 = secs.parse().map_err(|_| ConfigError::InvalidValue {
            field: "duration".into(),
            detail: format!("invalid seconds: {s}"),
        })?;
        return Ok(Duration::from_secs(n));
    }
    if let Some(mins) = s.strip_suffix('m') {
        let n: u64 = mins.parse().map_err(|_| ConfigError::InvalidValue {
            field: "duration".into(),
            detail: format!("invalid minutes: {s}"),
        })?;
        return Ok(Duration::from_secs(n * 60));
    }
    if let Some(hours) = s.strip_suffix('h') {
        let n: u64 = hours.parse().map_err(|_| ConfigError::InvalidValue {
            field: "duration".into(),
            detail: format!("invalid hours: {s}"),
        })?;
        return Ok(Duration::from_secs(n * 3600));
    }
    // Try bare seconds
    let n: u64 = s.parse().map_err(|_| ConfigError::InvalidValue {
        field: "duration".into(),
        detail: format!("expected duration like '30s', '1m', got: {s}"),
    })?;
    Ok(Duration::from_secs(n))
}

/// Parse a listen address like ":8080" or "0.0.0.0:8080".
fn parse_listen_addr(s: &str) -> Result<SocketAddr, ConfigError> {
    let s = s.trim();
    // Support ":port" shorthand
    let s = if s.starts_with(':') {
        format!("0.0.0.0{s}")
    } else {
        s.to_string()
    };
    s.parse().map_err(|_| ConfigError::InvalidValue {
        field: "address".into(),
        detail: format!("invalid socket address: {s}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let input = r#"
site "app.example.com" {
    route "/*" {
        proxy "localhost:3001"
    }
}
"#;
        let config = parse_config(input).unwrap();
        assert_eq!(config.sites.len(), 1);
        assert_eq!(config.sites[0].host, "app.example.com");
        assert_eq!(config.sites[0].routes.len(), 1);
        assert_eq!(config.sites[0].routes[0].path, "/*");
        match &config.sites[0].routes[0].handler {
            HandlerConfig::Proxy(p) => {
                assert_eq!(p.upstreams[0].addr, "localhost:3001");
            }
            _ => panic!("expected proxy handler"),
        }
    }

    #[test]
    fn test_parse_full_config() {
        let input = r#"
global {
    admin ":2019"
    log level="info" format="json"
    grace-period "30s"
}

tls {
    acme {
        email "admin@example.com"
        ca "letsencrypt"
        challenge "http-01"
    }
}

site "app.example.com" {
    route "/api/*" {
        rate-limit window="1m" max=100
        proxy {
            upstream "localhost:3001" weight=3
            upstream "localhost:3002" weight=1
            lb "weighted_round_robin"
            health-check uri="/health" interval="10s"
            header-up "X-Real-IP" "{client_ip}"
            header-down "-Server"
        }
    }
    route "/*" {
        encode "gzip" "zstd"
        root "/var/www/html"
        file-server
    }
}

site "api.example.com" {
    tls {
        cert "/path/to/cert.pem"
        key "/path/to/key.pem"
    }
    route "/*" {
        basic-auth {
            user "admin" hash="$2b$12$..."
        }
        proxy "localhost:8080"
    }
}
"#;
        let config = parse_config(input).unwrap();
        assert_eq!(config.sites.len(), 2);

        // Global
        assert_eq!(
            config.global.admin_addr,
            Some("0.0.0.0:2019".parse().unwrap())
        );
        assert_eq!(config.global.grace_period, Duration::from_secs(30));

        // ACME
        let acme = config.tls.as_ref().unwrap().acme.as_ref().unwrap();
        assert_eq!(acme.email, "admin@example.com");

        // First site
        let site0 = &config.sites[0];
        assert_eq!(site0.routes.len(), 2);
        match &site0.routes[0].handler {
            HandlerConfig::Proxy(p) => {
                assert_eq!(p.upstreams.len(), 2);
                assert_eq!(p.lb, LbPolicy::WeightedRoundRobin);
            }
            _ => panic!("expected proxy"),
        }

        // Second site with manual TLS
        let site1 = &config.sites[1];
        assert!(site1.tls.is_some());
    }

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("1m").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
    }

    #[test]
    fn test_parse_listen_addr() {
        assert_eq!(
            parse_listen_addr(":8080").unwrap(),
            "0.0.0.0:8080".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            parse_listen_addr("127.0.0.1:3000").unwrap(),
            "127.0.0.1:3000".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn test_parse_fastcgi_config() {
        let input = r#"
site "app.example.com" {
    route "*.php" {
        fastcgi "127.0.0.1:9000" {
            root "/var/www/html"
            index "index.php"
            split ".php"
            env "APP_ENV" "production"
        }
    }
}
"#;
        let config = parse_config(input).unwrap();
        let route = &config.sites[0].routes[0];
        assert_eq!(route.path, "*.php");
        match &route.handler {
            HandlerConfig::FastCgi(cfg) => {
                assert_eq!(cfg.addr, "127.0.0.1:9000");
                assert_eq!(cfg.script_root, "/var/www/html");
                assert_eq!(cfg.index, vec!["index.php"]);
                assert_eq!(cfg.split_path, Some(".php".to_string()));
                assert_eq!(cfg.env.get("APP_ENV").unwrap(), "production");
            }
            _ => panic!("expected FastCgi handler"),
        }
    }

    #[test]
    fn test_parse_dns_upstream() {
        let input = r#"
site "app.example.com" {
    route "/api/*" {
        proxy {
            upstream "fallback:8080"
            dns-upstream name="app.svc.cluster.local" port=8080 refresh="30s"
        }
    }
}
"#;
        let config = parse_config(input).unwrap();
        match &config.sites[0].routes[0].handler {
            HandlerConfig::Proxy(p) => {
                assert_eq!(p.upstreams.len(), 1);
                let dns = p.dynamic_upstreams.as_ref().unwrap();
                assert_eq!(dns.dns_name, "app.svc.cluster.local");
                assert_eq!(dns.port, 8080);
                assert_eq!(dns.refresh_interval, Duration::from_secs(30));
            }
            _ => panic!("expected proxy handler"),
        }
    }

    #[test]
    fn test_parse_matchers() {
        let input = r#"
site "app.example.com" {
    route "/api/*" {
        match method="GET,POST"
        match header="X-Custom" pattern="foo*"
        proxy "localhost:8080"
    }
}
"#;
        let config = parse_config(input).unwrap();
        let route = &config.sites[0].routes[0];
        assert_eq!(route.matchers.len(), 2);
        match &route.matchers[0] {
            RequestMatcher::Method(methods) => {
                assert!(methods.contains(&"GET".to_string()));
                assert!(methods.contains(&"POST".to_string()));
            }
            _ => panic!("expected Method matcher"),
        }
        match &route.matchers[1] {
            RequestMatcher::Header { name, pattern } => {
                assert_eq!(name, "X-Custom");
                assert_eq!(pattern, "foo*");
            }
            _ => panic!("expected Header matcher"),
        }
    }
}
