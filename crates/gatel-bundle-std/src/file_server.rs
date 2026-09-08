//! `file-server`: serve files from a directory.
//!
//! The whole risk of a file server is serving a file it was not meant to. The
//! containment check therefore happens on the resolved path, after symbolic
//! links are followed, rather than on the request text — a check on the text
//! alone is defeated by a link inside the root pointing outside it.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::time::SystemTime;

use gatel_macros::FromNode;
use gatel_model::{Code, Diagnostic, Diagnostics, FromNode as _, Hasher, Node};
use gatel_plan::{Directive, DirectiveConfig, DirectiveMeta};
use salvo_core::http::{Request, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

use crate::file_metadata::{etag, http_date};

#[derive(Debug, FromNode)]
struct Fields {
    #[gatel(prop)]
    root: String,
    /// What to serve for a directory. Empty disables it.
    #[gatel(prop, default = String::from("index.html"))]
    index: String,
    /// List a directory that has no index file.
    ///
    /// Off by default: a listing exposes every file name in the directory, and
    /// that should be a decision rather than what happens when an index is
    /// missing.
    #[gatel(prop, default = false)]
    browse: bool,
    /// Serve `page.html.gz` for `page.html` when the client accepts gzip and
    /// the compressed file is already on disk.
    #[gatel(prop, default = false)]
    precompressed: bool,
    /// What to serve when nothing matched. Empty disables it.
    #[gatel(prop, default = String::new())]
    fallback: String,
}

#[derive(Debug)]
pub struct Config {
    root: PathBuf,
    index: Option<String>,
    browse: bool,
    precompressed: bool,
    fallback: Option<String>,
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(&self.root.to_string_lossy());
        hasher.update_opt_str(self.index.as_deref());
        hasher.update_bool(self.browse);
        hasher.update_bool(self.precompressed);
        hasher.update_opt_str(self.fallback.as_deref());
    }

    fn build(&self, cx: gatel_plan::BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        // Resolve the root once, here, so every request compares against a
        // path that is already canonical. A relative path is relative to the
        // configuration, like every other path in it — otherwise the same file
        // would mean different things depending on where the process was
        // started from.
        let root = std::fs::canonicalize(cx.root.join(&self.root))
            .map_err(|error| format!("cannot serve from `{}`: {error}", self.root.display()))?;
        if !root.is_dir() {
            return Err(format!("`{}` is not a directory", self.root.display()));
        }
        Ok(Arc::new(FileServer {
            root,
            index: self.index.clone(),
            browse: self.browse,
            precompressed: self.precompressed,
            fallback: self.fallback.clone(),
        }))
    }

    fn describe(&self) -> String {
        format!(
            "file-server {}{}{}",
            self.root.display(),
            if self.browse { " browsable" } else { "" },
            if self.precompressed { " precompressed" } else { "" }
        ) + &self
            .fallback
            .as_deref()
            .map(|fallback| format!(" falling back to {fallback}"))
            .unwrap_or_default()
    }
}

pub struct FileServerDirective;

impl Directive for FileServerDirective {
    fn meta(&self) -> &DirectiveMeta {
        static META: DirectiveMeta = DirectiveMeta::terminal("file-server").with_version("11");
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let fields = Fields::from_node(node, diagnostics)?;
        if fields.root.trim().is_empty() {
            diagnostics.push(
                Diagnostic::error(
                    Code::MissingField,
                    node.origin.clone(),
                    "`file-server` needs a `root`",
                )
                .with_help("write `file-server root=\"/var/www\"`"),
            );
            return None;
        }
        let fallback = fields.fallback.trim().to_string();
        if !fallback.is_empty() && !fallback.starts_with('/') {
            diagnostics.push(
                Diagnostic::error(
                    Code::InvalidValue,
                    node.origin.clone(),
                    format!("`fallback` must be a path inside the root, not `{fallback}`"),
                )
                .with_help("write `fallback=\"/index.html\"`"),
            );
            return None;
        }
        Some(Arc::new(Config {
            root: PathBuf::from(fields.root),
            index: (!fields.index.is_empty()).then_some(fields.index),
            browse: fields.browse,
            precompressed: fields.precompressed,
            fallback: (!fallback.is_empty()).then_some(fallback),
        }))
    }
}

struct FileServer {
    root: PathBuf,
    index: Option<String>,
    browse: bool,
    precompressed: bool,
    fallback: Option<String>,
}

/// What a request resolved to.
enum Target {
    File {
        path: PathBuf,
    },
    Listing(PathBuf),
    /// A directory reached without a trailing slash: relative links inside it
    /// would resolve one level too high, so the client is sent to the right URL.
    Redirect(String),
    NotFound,
}

impl FileServer {
    /// Auxiliary files (index and encoded siblings) obey the same resolved
    /// containment boundary as a directly requested file.
    fn contained_file(&self, candidate: &Path) -> Option<(PathBuf, std::fs::Metadata)> {
        let resolved = std::fs::canonicalize(candidate).ok()?;
        if !crate::file_path::public_resolved(&self.root, &resolved) {
            return None;
        }
        let metadata = std::fs::metadata(&resolved).ok()?;
        metadata.is_file().then_some((resolved, metadata))
    }

    /// What to serve for this request, including the fallback.
    ///
    /// The fallback is what makes a single-page application work: its router
    /// owns `/users/42`, no such file exists, and the answer is the
    /// application shell with a 200 — not a 404. It is `try_files $uri $uri/
    /// /index.html` in nginx and `try_files {path} /index.html` in Caddy, and
    /// the first two of those three are what `resolve` already does.
    ///
    /// It applies to any miss, which is what both of those do and therefore
    /// what people expect. That has a sharp edge worth knowing: a request for
    /// a script that is genuinely missing gets HTML with a 200, and the
    /// browser reports a MIME error rather than a 404. Serving assets under a
    /// path prefix with its own route avoids it.
    fn serve(&self, request_path: &str) -> Target {
        let target = self.resolve(request_path);
        let Target::NotFound = target else {
            return target;
        };
        let Some(fallback) = &self.fallback else {
            return target;
        };
        // Resolved through the same containment checks as any other path, and
        // once: a fallback that is itself missing is a miss, not a loop.
        match self.resolve(fallback) {
            file @ Target::File { .. } => file,
            _ => Target::NotFound,
        }
    }

    /// Turn a request path into a file, refusing anything that leaves the root.
    fn resolve(&self, request_path: &str) -> Target {
        let Some(relative) = safe_relative(request_path) else {
            return Target::NotFound;
        };
        let candidate = self.root.join(&relative);

        // Canonicalizing follows symbolic links, so this catches a link inside
        // the root that points outside it — which a check on the request text
        // alone would not.
        let Ok(resolved) = std::fs::canonicalize(&candidate) else {
            return Target::NotFound;
        };
        if !crate::file_path::public_resolved(&self.root, &resolved) {
            return Target::NotFound;
        }
        let Ok(metadata) = std::fs::metadata(&resolved) else {
            return Target::NotFound;
        };

        if metadata.is_file() {
            return Target::File { path: resolved };
        }
        if !metadata.is_dir() {
            // A device or a socket is not something to serve.
            return Target::NotFound;
        }

        if !request_path.ends_with('/') {
            return Target::Redirect(format!("{request_path}/"));
        }
        if let Some(index) = &self.index {
            let candidate = resolved.join(index);
            if let Some((path, _)) = self.contained_file(&candidate) {
                return Target::File { path };
            }
        }
        if self.browse {
            Target::Listing(resolved)
        } else {
            Target::NotFound
        }
    }

    /// Prefer an already-compressed file when the client accepts that encoding.
    fn precompressed_variants(
        &self,
        path: &Path,
        metadata: &std::fs::Metadata,
    ) -> Vec<(PathBuf, &'static str)> {
        if !self.precompressed {
            return Vec::new();
        }
        // A leftover build artifact must not hide a newer source. Unknown
        // timestamps fail closed to identity. Equal times remain valid for
        // coarse-resolution filesystems and reproducible deployment archives.
        // This is a freshness heuristic, not a cross-file snapshot guarantee.
        let Ok(modified) = metadata.modified() else {
            return Vec::new();
        };
        let mut available = Vec::new();
        for (suffix, encoding) in [("br", "br"), ("zst", "zstd"), ("gz", "gzip")] {
            let mut candidate = path.as_os_str().to_os_string();
            candidate.push(format!(".{suffix}"));
            if let Some((resolved, metadata)) = self.contained_file(Path::new(&candidate))
                && metadata.modified().is_ok_and(|time| time >= modified)
            {
                available.push((resolved, encoding));
            }
        }
        available
    }
}

#[async_trait]
impl Handler for FileServer {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        _ctrl: &mut FlowCtrl,
    ) {
        if !matches!(*request.method(), http::Method::GET | http::Method::HEAD) {
            crate::generated(depot);
            response.status_code(StatusCode::METHOD_NOT_ALLOWED);
            response
                .headers_mut()
                .insert(http::header::ALLOW, http::HeaderValue::from_static("GET, HEAD"));
            response.headers_mut().insert(
                http::header::CACHE_CONTROL,
                http::HeaderValue::from_static("no-store"),
            );
            response.headers_mut().insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("text/plain; charset=utf-8"),
            );
            let _ = response.write_body("405 file-server only supports GET and HEAD\n");
            return;
        }
        let path = request.uri().path().to_string();
        let file = match self.serve(&path) {
            Target::File { path, .. } => path,
            Target::Redirect(location) => {
                response.status_code(StatusCode::MOVED_PERMANENTLY);
                if let Some(value) = crate::local_redirect::location(&location, request.uri().query()) {
                    response.headers_mut().insert(http::header::LOCATION, value);
                }
                return;
            }
            Target::Listing(directory) => {
                crate::file_listing::write(response, depot, &self.root, &directory, &path).await;
                return;
            }
            // One answer for "no such file", "outside the root" and "not a
            // regular file": the difference is not the client's business.
            Target::NotFound => {
                crate::generated(depot);
                response.status_code(StatusCode::NOT_FOUND);
                let _ = response.write_body("404 not found\n");
                return;
            }
        };

        let mut opened = match crate::ranges::OpenedFile::open(&file).await {
            Ok(opened) => opened,
            Err(error) => return unreadable(&file, &error, depot, response),
        };
        let variants = self.precompressed_variants(&file, opened.metadata());
        let dynamic =
            crate::file_encoding::available(depot, request, opened.metadata().len(), content_type(&file));
        let selection = crate::encoding_accept::select(
            request.headers(),
            variants
                .iter()
                .map(|(_, coding)| *coding)
                .chain(dynamic.iter().map(|coding| coding.token())),
        );
        crate::file_encoding::selected(depot);
        // Even an identity-only file can now reject an Accept-Encoding value.
        crate::vary::append(response.headers_mut(), "accept-encoding");
        if selection == crate::encoding_accept::Selection::NotAcceptable {
            return crate::file_encoding::reject(depot, response);
        }
        let encoding = selection.coding();
        let mut source = file.clone();
        let mut transform = None;
        if let Some(coding) = encoding {
            if let Some((variant, _)) = variants.into_iter().find(|(_, token)| *token == coding) {
                source = variant;
                opened = match crate::ranges::OpenedFile::open(&source).await {
                    Ok(opened) => opened,
                    Err(error) => return unreadable(&source, &error, depot, response),
                };
            } else {
                transform = dynamic.into_iter().find(|offer| offer.token() == coding);
            }
        }
        let metadata = opened.metadata();
        let tag = match (encoding, transform) {
            (_, Some(_)) => format!("W/{}", etag(metadata)),
            (Some(coding), None) => format!("\"{coding}-{}\"", etag(metadata).trim_matches('"')),
            (None, None) => etag(metadata),
        };
        if let Ok(value) = http::HeaderValue::from_str(&tag) {
            response.headers_mut().insert(http::header::ETAG, value);
        }
        if let Some(modified) = metadata.modified().ok().and_then(http_date)
            && let Ok(value) = http::HeaderValue::from_str(&modified)
        {
            response.headers_mut().insert(http::header::LAST_MODIFIED, value);
        }

        if let Some(encoding) = encoding {
            response.headers_mut().insert(
                http::header::CONTENT_ENCODING,
                http::HeaderValue::from_static(encoding),
            );
        }
        if let Ok(value) = http::HeaderValue::from_str(content_type(&file)) {
            response.headers_mut().insert(http::header::CONTENT_TYPE, value);
        }

        // Preconditions describe the selected representation, not its identity
        // sibling. Vary and validators must also be present on the 304 path.
        if let Some(status) =
            crate::preconditions::evaluate(request.method(), request.headers(), response.headers())
        {
            response.status_code(status);
            if status != StatusCode::NOT_MODIFIED {
                crate::generated(depot);
                response.headers_mut().remove(http::header::CONTENT_ENCODING);
                response.headers_mut().remove(http::header::ETAG);
                response.headers_mut().remove(http::header::LAST_MODIFIED);
                response.headers_mut().insert(
                    http::header::CONTENT_TYPE,
                    http::HeaderValue::from_static("text/plain; charset=utf-8"),
                );
                response.headers_mut().insert(
                    http::header::CACHE_CONTROL,
                    http::HeaderValue::from_static("no-store"),
                );
                let _ = response.write_body(format!("{} file precondition rejected\n", status.as_u16()));
            }
            return;
        }

        let size = metadata.len();
        if request.method() == http::Method::HEAD {
            response.status_code(StatusCode::OK);
            if transform.is_some() {
                // Streaming encoded length is unknown; never report the
                // identity length for the negotiated HEAD representation.
                return;
            }
            response.headers_mut().insert(
                http::header::ACCEPT_RANGES,
                http::HeaderValue::from_static("bytes"),
            );
            response.headers_mut().insert(
                http::header::CONTENT_LENGTH,
                http::HeaderValue::from_str(&size.to_string()).expect("decimal file size"),
            );
            return;
        }

        // A range names bytes of the representation being sent, and a
        // precompressed variant is a different representation with different
        // offsets. Rather than reason about that, the whole thing is sent —
        // which a server may always do.
        let wanted = if encoding.is_some() {
            crate::ranges::Wanted::Whole
        } else {
            crate::ranges::wanted(request, size, &tag)
        };

        if let Err(error) = crate::ranges::deliver(response, opened, wanted).await {
            unreadable(&source, &error, depot, response);
        } else if let Some(encoding) = transform {
            crate::encode_metadata::transformed(response.headers_mut());
            let body = std::mem::take(response.body_mut());
            *response.body_mut() = crate::encode::compress(body, encoding);
        }
    }
}

/// The file was there a moment ago and cannot be read now.
fn unreadable(source: &std::path::Path, error: &std::io::Error, depot: &mut Depot, response: &mut Response) {
    tracing::warn!(
        target: "gatel::file",
        event = "read_failed",
        file = %source.display(),
        %error,
    );
    crate::generated(depot);
    response.status_code(StatusCode::INTERNAL_SERVER_ERROR);
    // A failed read is a new error representation, not the selected file.
    for name in [
        http::header::CONTENT_LENGTH,
        http::header::CONTENT_RANGE,
        http::header::CONTENT_ENCODING,
        http::header::ETAG,
        http::header::LAST_MODIFIED,
        http::header::ACCEPT_RANGES,
    ] {
        response.headers_mut().remove(name);
    }
    response.headers_mut().insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    *response.body_mut() = salvo_core::http::ResBody::None;
    let _ = response.write_body("500 could not read the file\n");
}

/// Turn a request path into a relative path, or refuse it.
///
/// Percent-decoding happens first, because `%2e%2e` is `..`; anything that is
/// not a plain name after decoding is refused rather than normalised, so there
/// is nothing to be clever about.
fn safe_relative(request_path: &str) -> Option<PathBuf> {
    let decoded = percent_decode(request_path.trim_start_matches('/'))?;
    if decoded.contains('\0') {
        return None;
    }
    let mut relative = PathBuf::new();
    for component in Path::new(&decoded).components() {
        match component {
            Component::Normal(part) => {
                let text = part.to_str()?;
                // A leading dot is not inherently dangerous, but `.git` and
                // `.env` are the files a directory should least often serve.
                if !crate::file_path::public_component(text) {
                    return None;
                }
                relative.push(part);
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(relative)
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = input.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A small table rather than a dependency: these are the types a gateway
/// actually serves, and an unknown extension is a byte stream.
fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "map" => "application/json",
        "txt" | "md" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "csv" => "text/csv; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, Value};

    use super::*;

    /// A context with nothing in it, for a directive that needs nothing.
    fn empty_context() -> gatel_plan::BuildContext<'static> {
        gatel_plan::BuildContext {
            secrets: Default::default(),
            blocking: gatel_plan::blocking::Executor::process_default(),
            telemetry: crate::testing::telemetry(),
            upstreams: crate::testing::no_upstreams(),
            root: std::path::Path::new("."),
            state: None,
        }
    }
    fn origin() -> Origin {
        Origin::synthetic("test")
    }

    fn decode(node: Node) -> (Option<Arc<dyn DirectiveConfig>>, Diagnostics) {
        let mut diagnostics = Diagnostics::new();
        let config = FileServerDirective.decode(&node, &mut diagnostics);
        (config, diagnostics)
    }

    struct Tree(PathBuf);

    impl Tree {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "gatel-files-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.0.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, contents).unwrap();
            path
        }

        fn server(&self, browse: bool) -> FileServer {
            FileServer {
                root: std::fs::canonicalize(&self.0).unwrap(),
                index: Some("index.html".to_string()),
                browse,
                precompressed: false,
                fallback: None,
            }
        }

        fn server_with_fallback(&self, fallback: &str) -> FileServer {
            FileServer {
                fallback: Some(fallback.to_string()),
                ..self.server(false)
            }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_file_inside_the_root_resolves() {
        let tree = Tree::new("basic");
        tree.write("page.html", "hello");
        assert!(matches!(
            tree.server(false).resolve("/page.html"),
            Target::File { .. }
        ));
    }

    #[test]
    fn directory_index_cannot_escape_root_even_when_configured_as_absolute() {
        let tree = Tree::new("index-containment");
        let outside = Tree::new("index-outside");
        let secret = outside.write("secret.html", "outside secret");
        let mut server = tree.server(false);
        server.index = Some(secret.to_string_lossy().into_owned());
        assert!(matches!(server.resolve("/"), Target::NotFound));
        assert!(server.contained_file(&secret).is_none());
        tree.write("safe.html", "safe");
        server.index = Some("safe.html".into());
        assert!(matches!(server.resolve("/"), Target::File { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn auxiliary_symlinks_cannot_escape_but_internal_links_still_work() {
        use std::os::unix::fs::symlink;
        let tree = Tree::new("auxiliary-links");
        let outside = Tree::new("auxiliary-outside");
        let secret = outside.write("secret", "outside secret");
        let source = tree.write("page.txt", "identity");
        symlink(&secret, tree.0.join("index.html")).unwrap();
        symlink(&secret, tree.0.join("page.txt.gz")).unwrap();
        let mut server = tree.server(false);
        server.precompressed = true;
        assert!(matches!(server.resolve("/"), Target::NotFound));
        let metadata = std::fs::metadata(&source).unwrap();
        assert!(server.precompressed_variants(&source, &metadata).is_empty());
        let internal = tree.write("safe.br", "inside");
        symlink(&internal, tree.0.join("page.txt.br")).unwrap();
        let variants = server.precompressed_variants(&source, &metadata);
        assert_eq!(variants.len(), 1);
        let (selected, coding) = &variants[0];
        assert_eq!(*coding, "br");
        assert_eq!(*selected, std::fs::canonicalize(internal).unwrap());
    }

    #[test]
    fn traversal_is_refused_however_it_is_spelled() {
        let tree = Tree::new("traversal");
        tree.write("page.html", "hello");
        let server = tree.server(false);
        for attempt in [
            "/../secret",
            "/a/../../secret",
            "/%2e%2e/secret",
            "/%2E%2E%2Fsecret",
            "/....//secret",
        ] {
            assert!(
                matches!(server.resolve(attempt), Target::NotFound),
                "`{attempt}` must not escape the root"
            );
        }
    }

    #[test]
    fn a_dotfile_is_not_served() {
        let tree = Tree::new("dotfile");
        tree.write(".env", "SECRET=1");
        assert!(matches!(tree.server(false).resolve("/.env"), Target::NotFound));
    }

    #[test]
    fn a_null_byte_is_refused() {
        assert!(safe_relative("/page%00.html").is_none());
    }

    #[test]
    fn a_directory_without_a_slash_redirects_so_relative_links_work() {
        let tree = Tree::new("slash");
        tree.write("sub/index.html", "hi");
        match tree.server(false).resolve("/sub") {
            Target::Redirect(location) => assert_eq!(location, "/sub/"),
            other => panic!("expected a redirect, got {}", describe(&other)),
        }
    }

    #[test]
    fn a_directory_serves_its_index() {
        let tree = Tree::new("index");
        tree.write("sub/index.html", "hi");
        assert!(matches!(tree.server(false).resolve("/sub/"), Target::File { .. }));
    }

    #[test]
    fn browsing_is_off_unless_asked_for() {
        let tree = Tree::new("browse");
        tree.write("sub/other.txt", "hi");
        assert!(matches!(tree.server(false).resolve("/sub/"), Target::NotFound));
        assert!(matches!(tree.server(true).resolve("/sub/"), Target::Listing(_)));
    }

    #[test]
    fn a_single_page_application_gets_its_shell_for_a_route_it_owns() {
        // The whole reason this exists. `/users/42` is the application's
        // route, not a file, and the answer is the shell with a 200 rather
        // than a 404 — `try_files $uri $uri/ /index.html` in nginx.
        let tree = Tree::new("spa");
        tree.write("index.html", "<html>app</html>");
        tree.write("assets/app.js", "console.log(1)");
        let server = tree.server_with_fallback("/index.html");

        assert_eq!(describe(&server.serve("/users/42")), "file");
        assert_eq!(describe(&server.serve("/deep/nested/route")), "file");
        // A real file still wins over the fallback.
        match server.serve("/assets/app.js") {
            Target::File { path, .. } => assert!(path.ends_with("app.js")),
            other => panic!("{}", describe(&other)),
        }
    }

    #[test]
    fn without_a_fallback_a_miss_is_still_a_miss() {
        let tree = Tree::new("no-fallback");
        tree.write("index.html", "<html>app</html>");
        assert_eq!(describe(&tree.server(false).serve("/users/42")), "not found");
    }

    #[test]
    fn a_fallback_that_is_not_there_is_a_miss_rather_than_a_loop() {
        let tree = Tree::new("fallback-missing");
        tree.write("real.txt", "hi");
        let server = tree.server_with_fallback("/index.html");
        assert_eq!(describe(&server.serve("/anything")), "not found");
        // And the fallback path itself must not recurse into itself.
        assert_eq!(describe(&server.serve("/index.html")), "not found");
    }

    #[test]
    fn a_fallback_cannot_be_used_to_leave_the_root() {
        // It goes through the same containment check as any other path, so a
        // fallback is not a way around the thing this file exists to enforce.
        let tree = Tree::new("fallback-escape");
        tree.write("index.html", "<html>app</html>");
        let server = tree.server_with_fallback("/../../etc/passwd");
        assert_eq!(describe(&server.serve("/anything")), "not found");
    }

    #[test]
    fn a_directory_redirect_is_not_replaced_by_the_fallback() {
        // `/sub` must still become `/sub/`, or relative links inside it
        // resolve one level too high. A fallback answering it would hide that.
        let tree = Tree::new("fallback-redirect");
        tree.write("index.html", "<html>app</html>");
        tree.write("sub/index.html", "<html>sub</html>");
        let server = tree.server_with_fallback("/index.html");
        assert_eq!(describe(&server.serve("/sub")), "redirect");
    }

    #[test]
    fn a_fallback_must_be_a_path() {
        let node = Node::new("file-server", origin())
            .with_prop("root", Value::string(".", origin()))
            .with_prop("fallback", Value::string("index.html", origin()));
        let (config, diagnostics) = decode(node);
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("must be a path inside the root")),
            "{diagnostics}"
        );
    }

    fn describe(target: &Target) -> &'static str {
        match target {
            Target::File { .. } => "file",
            Target::Listing(_) => "listing",
            Target::Redirect(_) => "redirect",
            Target::NotFound => "not found",
        }
    }

    #[test]
    fn content_types_cover_what_a_site_serves() {
        assert_eq!(content_type(Path::new("a.html")), "text/html; charset=utf-8");
        assert_eq!(content_type(Path::new("a.js")), "text/javascript; charset=utf-8");
        assert_eq!(content_type(Path::new("a.woff2")), "font/woff2");
        assert_eq!(content_type(Path::new("a.unknown")), "application/octet-stream");
        assert_eq!(content_type(Path::new("noextension")), "application/octet-stream");
    }

    #[test]
    fn an_etag_changes_with_the_file() {
        let tree = Tree::new("etag");
        let path = tree.write("a.txt", "one");
        let first = etag(&std::fs::metadata(&path).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(10));
        tree.write("a.txt", "a longer body");
        let second = etag(&std::fs::metadata(&path).unwrap());
        assert_ne!(first, second);
        assert!(first.starts_with('"') && first.ends_with('"'));
    }

    #[test]
    fn http_dates_are_formatted_the_way_the_protocol_wants() {
        assert!(
            http_date(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(253_402_300_800)).is_none()
        );
        // 2001-09-09T01:46:40Z, a Sunday.
        let time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        assert_eq!(http_date(time).unwrap(), "Sun, 09 Sep 2001 01:46:40 GMT");
        assert_eq!(
            http_date(SystemTime::UNIX_EPOCH).unwrap(),
            "Thu, 01 Jan 1970 00:00:00 GMT"
        );
    }

    #[test]
    fn a_listing_escapes_names_from_the_filesystem() {
        use crate::file_listing::escape;
        assert_eq!(escape("<script>"), "&lt;script&gt;");
        assert_eq!(escape("a&b"), "a&amp;b");
    }

    #[test]
    fn a_root_is_required_and_checked_when_the_handler_is_built() {
        let (config, diagnostics) = decode(Node::new("file-server", origin()));
        assert!(config.is_none());
        assert!(diagnostics.iter().any(|d| d.code == Code::MissingField));

        let node = Node::new("file-server", origin())
            .with_prop("root", Value::string("/definitely/not/here", origin()));
        let (config, _) = decode(node);
        // A missing directory stops the reload rather than 404-ing every request.
        assert!(config.unwrap().build(empty_context()).is_err());
    }

    #[test]
    fn it_is_a_terminal() {
        assert_eq!(FileServerDirective.meta().role, gatel_plan::Role::Terminal);
    }
    #[test]
    fn a_relative_root_is_relative_to_the_configuration() {
        // Not to wherever the process happened to be started, or the same file
        // would mean different things in a shell and under a service manager.
        let directory = std::env::temp_dir().join("gatel-file-server-relative");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(directory.join("www")).unwrap();
        std::fs::write(directory.join("www/index.html"), "hello").unwrap();

        let node = Node::new("file-server", origin()).with_prop("root", Value::string("www", origin()));
        let mut diagnostics = Diagnostics::new();
        let config = FileServerDirective
            .decode(&node, &mut diagnostics)
            .expect("the configuration is valid");
        assert!(
            config
                .build(gatel_plan::BuildContext {
                    secrets: Default::default(),
                    blocking: gatel_plan::blocking::Executor::process_default(),
                    root: &directory,
                    state: None,
                    telemetry: crate::testing::telemetry(),
                    upstreams: crate::testing::no_upstreams(),
                })
                .is_ok(),
            "the root is `www` under the configuration, not under the current directory"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }
}
