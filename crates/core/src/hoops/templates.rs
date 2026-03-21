use std::path::{Path, PathBuf};

use bytes::Bytes;
use http::header::{CONTENT_LENGTH, CONTENT_TYPE, HOST};
use salvo::{Depot, FlowCtrl, Request, Response, async_trait};
use tracing::debug;

/// Maximum response size to process for template substitution (default 1 MB).
const MAX_TEMPLATE_SIZE: usize = 1024 * 1024;

/// Server-side template processing middleware.
///
/// Intercepts HTML responses and replaces `{{variable}}` placeholders with
/// values derived from the request context. Supports:
/// - `{{host}}` — request Host header
/// - `{{path}}` — request URI path
/// - `{{method}}` — HTTP method
/// - `{{scheme}}` — "https" or "http"
/// - `{{client_ip}}` — client IP address
/// - `{{query}}` — query string (without leading `?`)
/// - `{{uri}}` — full request URI
/// - `{{remote_addr}}` — full client socket address
/// - `{{server_name}}` — server hostname (from Host header, without port)
/// - `{{.Env.VARNAME}}` — environment variable lookup
/// - `{{include "path"}}` — include another file's contents
pub struct TemplatesHoop {
    /// Root directory for `{{include}}` paths. If `None`, includes are relative to CWD.
    root: Option<PathBuf>,
}

impl TemplatesHoop {
    pub fn new(root: Option<String>) -> Self {
        debug!(root = root.as_deref(), "templates middleware initialized");
        Self {
            root: root.map(PathBuf::from),
        }
    }
}

/// Variables extracted from the request context for template substitution.
struct TemplateVars {
    host: String,
    path: String,
    method: String,
    scheme: String,
    client_ip: String,
    query: String,
    uri: String,
    remote_addr: String,
    server_name: String,
}

#[async_trait]
impl salvo::Handler for TemplatesHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        // Extract template variables from the request before passing it downstream.
        let client_addr = super::client_addr(req);
        let host = req
            .headers()
            .get(HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let server_name = host.split(':').next().unwrap_or("").to_string();
        let vars = TemplateVars {
            host: host.clone(),
            path: req.uri().path().to_string(),
            method: req.method().to_string(),
            scheme: req.uri().scheme_str().unwrap_or("http").to_string(),
            client_ip: client_addr.ip().to_string(),
            query: req.uri().query().unwrap_or("").to_string(),
            uri: req.uri().to_string(),
            remote_addr: client_addr.to_string(),
            server_name,
        };

        // Call downstream handler.
        ctrl.call_next(req, depot, res).await;

        // Only process text/html responses.
        let is_html = res
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.to_ascii_lowercase().contains("text/html"))
            .unwrap_or(false);

        if !is_html {
            return;
        }

        // Collect response body.
        let body = res.take_body();
        let body_bytes = match super::compress::collect_res_body_bytes(body).await {
            Ok(b) => b,
            Err(_) => return,
        };

        // Skip if body is too large.
        if body_bytes.len() > MAX_TEMPLATE_SIZE {
            debug!(
                size = body_bytes.len(),
                max = MAX_TEMPLATE_SIZE,
                "response too large for template processing"
            );
            res.body(body_bytes);
            return;
        }

        // Convert to string. If it's not valid UTF-8, return as-is.
        let html = match std::str::from_utf8(&body_bytes) {
            Ok(s) => s.to_string(),
            Err(_) => {
                res.body(body_bytes);
                return;
            }
        };

        // Apply template substitution.
        let processed = process_template(&html, &vars, &self.root);

        let processed_bytes = Bytes::from(processed);

        // Update Content-Length.
        res.headers_mut().remove(CONTENT_LENGTH);
        res.headers_mut()
            .insert(CONTENT_LENGTH, processed_bytes.len().into());

        debug!("applied template substitution to HTML response");
        res.body(processed_bytes.to_vec());
    }
}

/// Process template substitution on an HTML string.
fn process_template(input: &str, vars: &TemplateVars, root: &Option<PathBuf>) -> String {
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Look for opening `{{`.
        if i + 1 < len && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // Find the closing `}}`.
            if let Some(close_pos) = find_closing_braces(input, i + 2) {
                let tag_content = &input[i + 2..close_pos];
                let tag_content = tag_content.trim();

                let replacement = resolve_tag(tag_content, vars, root);
                result.push_str(&replacement);
                i = close_pos + 2; // skip past `}}`
                continue;
            }
        }

        result.push(bytes[i] as char);
        i += 1;
    }

    result
}

/// Find the position of `}}` starting from `start` in the string.
fn find_closing_braces(s: &str, start: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = start;
    while i + 1 < bytes.len() {
        if bytes[i] == b'}' && bytes[i + 1] == b'}' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Resolve a single template tag to its replacement value.
fn resolve_tag(tag: &str, vars: &TemplateVars, root: &Option<PathBuf>) -> String {
    // Simple variable lookups.
    match tag {
        "host" => return vars.host.clone(),
        "path" => return vars.path.clone(),
        "method" => return vars.method.clone(),
        "scheme" => return vars.scheme.clone(),
        "client_ip" => return vars.client_ip.clone(),
        "query" => return vars.query.clone(),
        "uri" => return vars.uri.clone(),
        "remote_addr" => return vars.remote_addr.clone(),
        "server_name" => return vars.server_name.clone(),
        _ => {}
    }

    // Environment variable: {{.Env.VARNAME}}
    if let Some(var_name) = tag.strip_prefix(".Env.") {
        let var_name = var_name.trim();
        return std::env::var(var_name).unwrap_or_default();
    }

    // Include directive: {{include "path"}} or {{include 'path'}}
    if let Some(rest) = tag.strip_prefix("include ") {
        let rest = rest.trim();
        let include_path = rest.trim_matches('"').trim_matches('\'');
        return resolve_include(include_path, root);
    }

    // Unknown tag — return it as-is to avoid data loss.
    format!("{{{{{tag}}}}}")
}

/// Read and return the contents of an included file.
fn resolve_include(path_str: &str, root: &Option<PathBuf>) -> String {
    let path = Path::new(path_str);

    // If the path is relative and root is set, resolve relative to root.
    let full_path = if path.is_relative() {
        match root {
            Some(root_dir) => root_dir.join(path),
            None => PathBuf::from(path),
        }
    } else {
        PathBuf::from(path)
    };

    // Prevent path traversal in includes.
    let path_str_normalized = full_path.to_string_lossy().replace('\\', "/");
    for component in path_str_normalized.split('/') {
        if component == ".." {
            debug!(path = %full_path.display(), "blocked include with path traversal");
            return String::new();
        }
    }

    match std::fs::read_to_string(&full_path) {
        Ok(contents) => {
            debug!(path = %full_path.display(), "included template file");
            contents
        }
        Err(e) => {
            debug!(path = %full_path.display(), error = %e, "failed to include template file");
            String::new()
        }
    }
}
