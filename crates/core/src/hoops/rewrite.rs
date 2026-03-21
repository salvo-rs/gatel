use std::path::Path;

use http::Uri;
use regex::Regex;
use salvo::{Depot, FlowCtrl, Request, Response, async_trait};
use tracing::debug;

use crate::ProxyError;

/// URI rewrite middleware.
///
/// Supports:
/// - **strip_prefix**: remove a path prefix before forwarding (e.g., strip "/api" so
///   "/api/users?q=1" becomes "/users?q=1")
/// - **uri**: full URI rewrite with simple pattern matching. The pattern may contain `{path}` which
///   is replaced by the original path, and `{query}` which is replaced by the original query
///   string.
/// - **regex_rules**: sequential regex replacements applied to the path.
/// - **if_not_file** / **if_not_dir**: only apply rewrite when the path does not resolve to an
///   existing file or directory under `root`.
pub struct RewriteHoop {
    strip_prefix: Option<String>,
    uri_template: Option<String>,
    regex_rules: Vec<(Regex, String)>,
    if_not_file: bool,
    if_not_dir: bool,
    root: Option<String>,
    normalize_slashes: bool,
}

impl RewriteHoop {
    pub fn new(
        strip_prefix: Option<String>,
        uri_template: Option<String>,
        regex_rules: Vec<(Regex, String)>,
        if_not_file: bool,
        if_not_dir: bool,
        root: Option<String>,
        normalize_slashes: bool,
    ) -> Self {
        Self {
            strip_prefix,
            uri_template,
            regex_rules,
            if_not_file,
            if_not_dir,
            root,
            normalize_slashes,
        }
    }
}

#[async_trait]
impl salvo::Handler for RewriteHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        if let Err(e) = apply_rewrite(self, req) {
            debug!(error = %e, "rewrite error");
            res.status_code(salvo::http::StatusCode::INTERNAL_SERVER_ERROR);
            res.body(e.to_string());
            ctrl.skip_rest();
            return;
        }

        ctrl.call_next(req, depot, res).await;
    }
}

fn apply_rewrite(mw: &RewriteHoop, req: &mut Request) -> Result<(), ProxyError> {
    let original_uri = req.uri().clone();

    // 0. Normalize consecutive slashes BEFORE any other rewrite rule.
    if mw.normalize_slashes {
        let path = original_uri.path();
        if path.contains("//") {
            let normalized = collapse_slashes(path);
            let pq = match original_uri.query() {
                Some(q) if !q.is_empty() => format!("{normalized}?{q}"),
                _ => normalized,
            };
            let new_uri = rebuild_uri(&original_uri, &pq)?;
            *req.uri_mut() = new_uri;
        }
    }

    // Re-read the (possibly updated) URI.
    let original_uri = req.uri().clone();
    let original_path = original_uri.path();
    let original_query = original_uri.query().unwrap_or("");

    // Conditional checks: if_not_file / if_not_dir.
    if (mw.if_not_file || mw.if_not_dir)
        && let Some(root) = &mw.root
    {
        let fs_path = Path::new(root).join(original_path.trim_start_matches('/'));
        if mw.if_not_file && fs_path.is_file() {
            debug!(
                path = original_path,
                "rewrite skipped: path resolves to existing file"
            );
            return Ok(());
        }
        if mw.if_not_dir && fs_path.is_dir() {
            debug!(
                path = original_path,
                "rewrite skipped: path resolves to existing directory"
            );
            return Ok(());
        }
    }

    let mut new_path_and_query: Option<String> = None;

    // 1. Strip prefix.
    if let Some(prefix) = &mw.strip_prefix {
        let stripped = if original_path == prefix {
            "/".to_string()
        } else if let Some(rest) = original_path.strip_prefix(prefix.as_str()) {
            if rest.is_empty() || rest.starts_with('/') {
                if rest.is_empty() {
                    "/".to_string()
                } else {
                    rest.to_string()
                }
            } else {
                // Prefix does not align on a segment boundary — no rewrite.
                original_path.to_string()
            }
        } else {
            original_path.to_string()
        };

        let pq = if original_query.is_empty() {
            stripped
        } else {
            format!("{stripped}?{original_query}")
        };
        new_path_and_query = Some(pq);

        debug!(
            prefix = prefix.as_str(),
            original = original_path,
            rewritten = new_path_and_query.as_deref().unwrap_or(""),
            "stripped path prefix"
        );
    }

    // 2. Full URI rewrite template.
    if let Some(template) = &mw.uri_template {
        let current_path = new_path_and_query
            .as_deref()
            .map(|pq| pq.split('?').next().unwrap_or(pq))
            .unwrap_or(original_path);
        let current_query = new_path_and_query
            .as_deref()
            .and_then(|pq| pq.split_once('?').map(|(_, q)| q))
            .unwrap_or(original_query);

        let rewritten = template
            .replace("{path}", current_path)
            .replace("{query}", current_query);
        new_path_and_query = Some(rewritten.clone());

        debug!(
            template = template.as_str(),
            result = rewritten.as_str(),
            "applied URI rewrite template"
        );
    }

    // 3. Regex rules — applied sequentially to the current path.
    if !mw.regex_rules.is_empty() {
        let current_path = new_path_and_query
            .as_deref()
            .map(|pq| pq.split('?').next().unwrap_or(pq))
            .unwrap_or(original_path);
        let current_query = new_path_and_query
            .as_deref()
            .and_then(|pq| pq.split_once('?').map(|(_, q)| q))
            .unwrap_or(original_query);

        let mut path = current_path.to_string();
        for (re, replacement) in &mw.regex_rules {
            let replaced = re.replace(&path, replacement.as_str()).into_owned();
            debug!(
                pattern = re.as_str(),
                before = path.as_str(),
                after = replaced.as_str(),
                "applied regex rewrite rule"
            );
            path = replaced;
        }

        let pq = if current_query.is_empty() {
            path
        } else {
            format!("{path}?{current_query}")
        };
        new_path_and_query = Some(pq);
    }

    // Apply the rewritten URI.
    if let Some(pq) = new_path_and_query {
        let new_uri = rebuild_uri(&original_uri, &pq)?;
        *req.uri_mut() = new_uri;
    }

    Ok(())
}

/// Collapse consecutive slashes in a path segment.
/// E.g. `//foo///bar` → `/foo/bar`.
fn collapse_slashes(path: &str) -> String {
    let mut result = String::with_capacity(path.len());
    let mut prev_slash = false;
    for ch in path.chars() {
        if ch == '/' {
            if !prev_slash {
                result.push(ch);
            }
            prev_slash = true;
        } else {
            result.push(ch);
            prev_slash = false;
        }
    }
    result
}

/// Rebuild a URI, replacing only the path-and-query portion.
fn rebuild_uri(original: &Uri, new_path_and_query: &str) -> Result<Uri, ProxyError> {
    let mut builder = Uri::builder();
    if let Some(scheme) = original.scheme() {
        builder = builder.scheme(scheme.clone());
    }
    if let Some(authority) = original.authority() {
        builder = builder.authority(authority.clone());
    }
    builder = builder.path_and_query(new_path_and_query.to_string());
    builder
        .build()
        .map_err(|e| ProxyError::Internal(format!("failed to build rewritten URI: {e}")))
}
