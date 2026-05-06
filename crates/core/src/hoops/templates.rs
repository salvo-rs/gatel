use std::path::{Component, Path, PathBuf};

use bytes::Bytes;
use http::header::{CONTENT_LENGTH, CONTENT_TYPE, HOST};
use salvo::{Depot, FlowCtrl, Request, Response, async_trait};
use tracing::debug;

/// Maximum response size to process for template substitution (default 1 MB).
const MAX_TEMPLATE_SIZE: usize = 1024 * 1024;
const MAX_INCLUDE_SIZE: u64 = 64 * 1024;

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
/// - `{{.Env.VARNAME}}` — environment variable lookup when explicitly enabled
/// - `{{include "path"}}` — include a root-confined file when explicitly enabled
pub struct TemplatesHoop {
    /// Root directory for explicitly enabled `{{include}}` paths.
    root: Option<PathBuf>,
    allow_env: bool,
    allow_include: bool,
}

impl TemplatesHoop {
    pub fn new(root: Option<String>, allow_env: bool, allow_include: bool) -> Self {
        debug!(
            root = root.as_deref(),
            allow_env, allow_include, "templates middleware initialized"
        );
        Self {
            root: root.map(PathBuf::from),
            allow_env,
            allow_include,
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
        let processed = process_template(&html, &vars, self);

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
fn process_template(input: &str, vars: &TemplateVars, hoop: &TemplatesHoop) -> String {
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

                let replacement = resolve_tag(tag_content, vars, hoop);
                result.push_str(&replacement);
                i = close_pos + 2; // skip past `}}`
                continue;
            }
        }

        let ch = input[i..]
            .chars()
            .next()
            .expect("index is always on a UTF-8 character boundary");
        result.push(ch);
        i += ch.len_utf8();
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
fn resolve_tag(tag: &str, vars: &TemplateVars, hoop: &TemplatesHoop) -> String {
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
        if !hoop.allow_env {
            debug!("blocked template environment variable lookup because allow-env is disabled");
            return format!("{{{{{tag}}}}}");
        }
        let var_name = var_name.trim();
        return std::env::var(var_name).unwrap_or_default();
    }

    // Include directive: {{include "path"}} or {{include 'path'}}
    if let Some(rest) = tag.strip_prefix("include ") {
        if !hoop.allow_include {
            debug!("blocked template include because allow-include is disabled");
            return format!("{{{{{tag}}}}}");
        }
        let rest = rest.trim();
        let include_path = rest.trim_matches('"').trim_matches('\'');
        return resolve_include(include_path, &hoop.root);
    }

    // Unknown tag — return it as-is to avoid data loss.
    format!("{{{{{tag}}}}}")
}

/// Read and return the contents of an included file.
fn resolve_include(path_str: &str, root: &Option<PathBuf>) -> String {
    let Some(root) = root else {
        debug!("blocked include because templates root is not configured");
        return String::new();
    };
    let Some(full_path) = resolve_include_path(path_str, root) else {
        debug!(path = path_str, "blocked unsafe template include path");
        return String::new();
    };

    match std::fs::metadata(&full_path) {
        Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_INCLUDE_SIZE => {}
        Ok(_) => {
            debug!(path = %full_path.display(), "blocked oversized or non-file template include");
            return String::new();
        }
        Err(e) => {
            debug!(path = %full_path.display(), error = %e, "failed to inspect template include");
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

fn resolve_include_path(path_str: &str, root: &Path) -> Option<PathBuf> {
    let requested = safe_relative_path(path_str)?;
    let root = root.canonicalize().ok()?;
    let full_path = root.join(requested).canonicalize().ok()?;
    full_path.starts_with(&root).then_some(full_path)
}

fn safe_relative_path(path_str: &str) -> Option<PathBuf> {
    if path_str.as_bytes().contains(&0) {
        return None;
    }

    let path = Path::new(path_str);
    if path.is_absolute() {
        return None;
    }

    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(segment) => relative.push(segment),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(relative)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> TemplateVars {
        TemplateVars {
            host: "example.com".to_string(),
            path: "/index.html".to_string(),
            method: "GET".to_string(),
            scheme: "https".to_string(),
            client_ip: "127.0.0.1".to_string(),
            query: String::new(),
            uri: "/index.html".to_string(),
            remote_addr: "127.0.0.1:12345".to_string(),
            server_name: "example.com".to_string(),
        }
    }

    #[test]
    fn env_lookup_is_disabled_by_default() {
        let hoop = TemplatesHoop::new(None, false, false);

        let output = process_template("token={{.Env.PATH}}", &vars(), &hoop);

        assert_eq!(output, "token={{.Env.PATH}}");
    }

    #[test]
    fn include_is_disabled_by_default() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("header.html"), "safe").unwrap();
        let hoop = TemplatesHoop::new(
            Some(root.path().to_string_lossy().into_owned()),
            false,
            false,
        );

        let output = process_template(r#"{{include "header.html"}}"#, &vars(), &hoop);

        assert_eq!(output, r#"{{include "header.html"}}"#);
    }

    #[test]
    fn include_reads_root_confined_relative_file_when_enabled() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("partials")).unwrap();
        std::fs::write(root.path().join("partials/header.html"), "safe").unwrap();
        let hoop = TemplatesHoop::new(
            Some(root.path().to_string_lossy().into_owned()),
            false,
            true,
        );

        let output = process_template(r#"{{include "partials/header.html"}}"#, &vars(), &hoop);

        assert_eq!(output, "safe");
    }

    #[test]
    fn include_rejects_parent_traversal() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        let hoop = TemplatesHoop::new(
            Some(root.path().to_string_lossy().into_owned()),
            false,
            true,
        );

        let output = process_template(r#"{{include "../secret.txt"}}"#, &vars(), &hoop);

        assert_eq!(output, "");
    }

    #[test]
    fn include_rejects_absolute_path() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "secret").unwrap();
        let hoop = TemplatesHoop::new(
            Some(root.path().to_string_lossy().into_owned()),
            false,
            true,
        );
        let template = format!(r#"{{{{include "{}"}}}}"#, outside.path().display());

        let output = process_template(&template, &vars(), &hoop);

        assert_eq!(output, "");
    }

    #[test]
    fn process_template_preserves_utf8_text() {
        let hoop = TemplatesHoop::new(None, false, false);

        let rendered = process_template("你好 {{path}}", &vars(), &hoop);

        assert_eq!(rendered, "你好 /index.html");
    }
}
