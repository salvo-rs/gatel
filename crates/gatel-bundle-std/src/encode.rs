//! `encode`: compress a response on the way out.
//!
//! Streaming, so a large response is never held in memory and a slow client
//! applies backpressure all the way to the backend. Compression is skipped
//! rather than attempted whenever it would not help — an already-encoded body,
//! an image, a response too small to be worth a header.

use std::any::Any;
use std::sync::Arc;

use async_compression::tokio::bufread::{BrotliEncoder, DeflateEncoder, GzipEncoder, ZstdEncoder};
use bytes::Bytes;
use futures_util::TryStreamExt;
use gatel_macros::FromNode;
use gatel_model::{ByteSize, Code, Diagnostic, Diagnostics, FromNode as _, Hasher, Node};
use gatel_plan::{Directive, DirectiveConfig, DirectiveMeta, Phase, Scopes};
use salvo_core::http::{Request, ResBody, Response};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};
use tokio_util::io::{ReaderStream, StreamReader};

/// The encodings this directive can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Zstd,
    Brotli,
    Gzip,
    Deflate,
}

impl Encoding {
    /// The token as it appears in `Accept-Encoding` and `Content-Encoding`.
    pub const fn token(self) -> &'static str {
        match self {
            Encoding::Zstd => "zstd",
            Encoding::Brotli => "br",
            Encoding::Gzip => "gzip",
            Encoding::Deflate => "deflate",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "zstd" => Some(Encoding::Zstd),
            "br" | "brotli" => Some(Encoding::Brotli),
            "gzip" => Some(Encoding::Gzip),
            "deflate" => Some(Encoding::Deflate),
            _ => None,
        }
    }
}

#[derive(Debug, FromNode)]
struct Fields {
    /// Encodings the operator is willing to produce, best first.
    #[gatel(args)]
    encodings: Vec<String>,
    /// Below this, the header overhead costs more than compression saves.
    #[gatel(prop, default = ByteSize(1024))]
    min_size: ByteSize,
}

#[derive(Debug)]
pub struct Config {
    encodings: Vec<Encoding>,
    min_size: ByteSize,
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hasher: &mut Hasher) {
        // Order is preference, so it is part of the behaviour.
        hasher.update_ordered(&self.encodings, |encoding, hasher| {
            hasher.update_str(encoding.token());
        });
        hasher.update_u64(self.min_size.bytes());
    }

    fn build(&self, _state: Option<Arc<dyn Any + Send + Sync>>) -> Result<Arc<dyn Handler>, String> {
        Ok(Arc::new(EncodeHandler {
            encodings: self.encodings.clone(),
            min_size: self.min_size,
        }))
    }

    fn describe(&self) -> String {
        format!(
            "encode {} above {}",
            self.encodings
                .iter()
                .map(|encoding| encoding.token())
                .collect::<Vec<_>>()
                .join(" "),
            self.min_size
        )
    }
}

pub struct Encode;

impl Directive for Encode {
    fn meta(&self) -> &DirectiveMeta {
        // In the response phase, and ordered after `headers` so a header the
        // operator set is present on the compressed response too.
        static META: DirectiveMeta = DirectiveMeta::middleware("encode", Phase::Response, Scopes::BOTH);
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let fields = Fields::from_node(node, diagnostics)?;
        let mut encodings = Vec::new();
        for raw in &fields.encodings {
            match Encoding::parse(raw) {
                Some(encoding) if encodings.contains(&encoding) => {
                    diagnostics.error(
                        Code::Duplicate,
                        node.origin.clone(),
                        format!("`{raw}` is listed twice"),
                    );
                    return None;
                }
                Some(encoding) => encodings.push(encoding),
                None => {
                    diagnostics.push(
                        Diagnostic::error(
                            Code::InvalidValue,
                            node.origin.clone(),
                            format!("unknown encoding `{raw}`"),
                        )
                        .with_help("known encodings: zstd, br, gzip, deflate"),
                    );
                    return None;
                }
            }
        }
        if encodings.is_empty() {
            // A sensible default order: better ratio first, and every client
            // that supports one of these says so.
            encodings = vec![Encoding::Zstd, Encoding::Brotli, Encoding::Gzip];
        }
        Some(Arc::new(Config {
            encodings,
            min_size: fields.min_size,
        }))
    }
}

struct EncodeHandler {
    encodings: Vec<Encoding>,
    min_size: ByteSize,
}

impl EncodeHandler {
    /// The best encoding both sides accept, in the operator's order of
    /// preference rather than the client's.
    fn negotiate(&self, request: &Request) -> Option<Encoding> {
        let accepted = request
            .headers()
            .get(http::header::ACCEPT_ENCODING)
            .and_then(|value| value.to_str().ok())?;
        let offers: Vec<(&str, f32)> = accepted
            .split(',')
            .filter_map(|entry| {
                let mut parts = entry.split(';');
                let token = parts.next()?.trim();
                let quality = parts
                    .find_map(|part| part.trim().strip_prefix("q=")?.parse::<f32>().ok())
                    .unwrap_or(1.0);
                Some((token, quality))
            })
            .collect();

        self.encodings.iter().copied().find(|encoding| {
            offers
                .iter()
                .any(|(token, quality)| *quality > 0.0 && (*token == encoding.token() || *token == "*"))
        })
    }
}

#[async_trait]
impl Handler for EncodeHandler {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let chosen = self.negotiate(request);
        let head_only = request.method() == http::Method::HEAD;

        ctrl.call_next(request, depot, response).await;

        // Whatever happens, the response depends on what the client accepts,
        // so a shared cache must key on it. This is set even when nothing is
        // compressed, or a cache could store an uncompressed copy and serve it
        // to a client that would have received a compressed one.
        response.headers_mut().insert(
            http::header::VARY,
            http::HeaderValue::from_static("accept-encoding"),
        );

        let Some(encoding) = chosen else { return };
        // Taken first because the decision depends on the body's own size
        // hint: a handler that wrote bytes in memory leaves the length header
        // for hyper to compute, so the header is not there yet.
        let body = std::mem::take(response.body_mut());
        if head_only || !should_compress(response, &body, self.min_size) {
            *response.body_mut() = body;
            return;
        }

        // Length changes, and is not known until the body ends.
        response.headers_mut().remove(http::header::CONTENT_LENGTH);
        response.headers_mut().insert(
            http::header::CONTENT_ENCODING,
            http::HeaderValue::from_static(encoding.token()),
        );
        *response.body_mut() = compress(body, encoding);
    }
}

/// Whether compressing this response is worth doing at all.
fn should_compress(response: &Response, body: &ResBody, min_size: ByteSize) -> bool {
    use http_body::Body as _;

    let headers = response.headers();
    if headers.contains_key(http::header::CONTENT_ENCODING) {
        // Already encoded — by a backend, or by a `file-server` serving a
        // precompressed file. Compressing again would only cost CPU.
        return false;
    }
    if let Some(status) = response.status_code
        && (status == http::StatusCode::NO_CONTENT || status == http::StatusCode::NOT_MODIFIED)
    {
        return false;
    }
    // The header is often not set yet: a handler that wrote a body in memory
    // leaves the length for hyper to compute. The body itself knows, so ask it
    // first and fall back to the header for a body that does not.
    let known_length = body.size_hint().exact().or_else(|| {
        headers
            .get(http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
    });
    if let Some(length) = known_length
        && length < min_size.bytes()
    {
        return false;
    }
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_none_or(is_compressible)
}

/// Already-compressed formats do not get smaller, and trying wastes CPU on
/// every byte served.
fn is_compressible(content_type: &str) -> bool {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if base.starts_with("text/") {
        return true;
    }
    // A structured suffix covers image/svg+xml, application/ld+json and the
    // rest without listing them.
    if base.ends_with("+xml") || base.ends_with("+json") {
        return true;
    }
    matches!(
        base.as_str(),
        "application/json"
            | "application/javascript"
            | "application/xml"
            | "application/wasm"
            | "application/x-www-form-urlencoded"
            | "font/ttf"
            | "font/otf"
            | "image/x-icon"
    )
}

/// Wrap a body in a streaming encoder.
///
/// Nothing is buffered: the encoder reads as the client reads, so a slow client
/// slows the backend rather than filling memory.
fn compress(body: ResBody, encoding: Encoding) -> ResBody {
    let stream = http_body_util::BodyStream::new(body)
        .try_filter_map(|frame| async move { Ok(frame.into_data().ok()) })
        .map_err(std::io::Error::other);
    let reader = StreamReader::new(stream);

    fn boxed<R>(reader: R) -> ResBody
    where
        R: tokio::io::AsyncRead + Send + Sync + 'static,
    {
        let stream = ReaderStream::new(reader)
            .map_ok(|chunk: Bytes| hyper::body::Frame::data(chunk))
            .map_err(|error| Box::new(error) as salvo_core::BoxedError);
        ResBody::Boxed(Box::pin(http_body_util::StreamBody::new(stream)))
    }

    match encoding {
        Encoding::Zstd => boxed(ZstdEncoder::new(reader)),
        Encoding::Brotli => boxed(BrotliEncoder::new(reader)),
        Encoding::Gzip => boxed(GzipEncoder::new(reader)),
        Encoding::Deflate => boxed(DeflateEncoder::new(reader)),
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, Value};

    use super::*;

    /// A body that does not know its own length, as a streaming backend
    /// response does not.
    fn streaming_body() -> ResBody {
        use futures_util::stream;
        let chunks = stream::iter(Vec::<Result<hyper::body::Frame<Bytes>, salvo_core::BoxedError>>::new());
        ResBody::Boxed(Box::pin(http_body_util::StreamBody::new(chunks)))
    }

    fn origin() -> Origin {
        Origin::synthetic("test")
    }

    fn decode(node: Node) -> (Option<Arc<dyn DirectiveConfig>>, Diagnostics) {
        let mut diagnostics = Diagnostics::new();
        let config = Encode.decode(&node, &mut diagnostics);
        (config, diagnostics)
    }

    fn node(encodings: &[&str]) -> Node {
        let mut node = Node::new("encode", origin());
        for encoding in encodings {
            node = node.with_arg(Value::string(*encoding, origin()));
        }
        node
    }

    fn handler(encodings: &[&str]) -> EncodeHandler {
        EncodeHandler {
            encodings: encodings.iter().filter_map(|raw| Encoding::parse(raw)).collect(),
            min_size: ByteSize(1024),
        }
    }

    fn request_accepting(accept: &str) -> Request {
        let mut request = Request::default();
        if !accept.is_empty() {
            request.headers_mut().insert(
                http::header::ACCEPT_ENCODING,
                http::HeaderValue::from_str(accept).unwrap(),
            );
        }
        request
    }

    #[test]
    fn no_arguments_means_a_sensible_default_order() {
        let (config, diagnostics) = decode(node(&[]));
        assert!(!diagnostics.has_errors(), "{diagnostics}");
        assert_eq!(config.unwrap().describe(), "encode zstd br gzip above 1KiB");
    }

    #[test]
    fn an_unknown_encoding_lists_the_known_ones() {
        let (config, diagnostics) = decode(node(&["lzma"]));
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .next()
                .unwrap()
                .help
                .as_deref()
                .unwrap()
                .contains("zstd, br, gzip")
        );
    }

    #[test]
    fn listing_an_encoding_twice_is_rejected() {
        let (config, diagnostics) = decode(node(&["gzip", "gzip"]));
        assert!(config.is_none());
        assert!(diagnostics.iter().any(|d| d.code == Code::Duplicate));
    }

    #[test]
    fn the_operators_order_of_preference_wins_over_the_clients() {
        let handler = handler(&["zstd", "gzip"]);
        // The client lists gzip first; the operator prefers zstd.
        assert_eq!(
            handler.negotiate(&request_accepting("gzip, deflate, zstd")),
            Some(Encoding::Zstd)
        );
    }

    #[test]
    fn only_an_encoding_both_sides_accept_is_chosen() {
        let handler = handler(&["br"]);
        assert_eq!(handler.negotiate(&request_accepting("gzip")), None);
        assert_eq!(
            handler.negotiate(&request_accepting("br")),
            Some(Encoding::Brotli)
        );
        assert_eq!(handler.negotiate(&request_accepting("")), None);
    }

    #[test]
    fn a_wildcard_accepts_the_first_choice() {
        assert_eq!(
            handler(&["zstd", "gzip"]).negotiate(&request_accepting("*")),
            Some(Encoding::Zstd)
        );
    }

    #[test]
    fn an_encoding_refused_with_q_zero_is_not_used() {
        let handler = handler(&["gzip", "deflate"]);
        assert_eq!(
            handler.negotiate(&request_accepting("gzip;q=0, deflate")),
            Some(Encoding::Deflate)
        );
    }

    #[test]
    fn compressible_types_are_the_ones_that_actually_shrink() {
        assert!(is_compressible("text/html; charset=utf-8"));
        assert!(is_compressible("application/json"));
        assert!(is_compressible("image/svg+xml"));
        assert!(is_compressible("application/ld+json"));
        assert!(is_compressible("application/wasm"));

        assert!(!is_compressible("image/png"));
        assert!(!is_compressible("video/mp4"));
        assert!(!is_compressible("application/zip"));
        assert!(!is_compressible("application/octet-stream"));
    }

    #[test]
    fn an_already_encoded_response_is_left_alone() {
        let mut response = Response::new();
        response.headers_mut().insert(
            http::header::CONTENT_ENCODING,
            http::HeaderValue::from_static("gzip"),
        );
        assert!(!should_compress(&response, &ResBody::None, ByteSize(0)));
    }

    #[test]
    fn a_response_below_the_threshold_is_left_alone() {
        let mut response = Response::new();
        response.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/plain"),
        );
        // The body knows its own length even before a header says so.
        let body = ResBody::Once(Bytes::from(vec![b'x'; 100]));
        assert!(!should_compress(&response, &body, ByteSize(1024)));
        assert!(should_compress(&response, &body, ByteSize(10)));
    }

    #[test]
    fn the_length_header_is_used_when_the_body_does_not_know() {
        let mut response = Response::new();
        response.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/plain"),
        );
        response.headers_mut().insert(
            http::header::CONTENT_LENGTH,
            http::HeaderValue::from_static("100"),
        );
        assert!(!should_compress(&response, &streaming_body(), ByteSize(1024)));
    }

    #[test]
    fn a_bodiless_status_is_left_alone() {
        let mut response = Response::new();
        response.status_code(http::StatusCode::NOT_MODIFIED);
        assert!(!should_compress(&response, &ResBody::None, ByteSize(0)));
    }

    #[test]
    fn a_response_without_a_content_type_is_still_compressed() {
        // Erring toward compressing is right here: the alternative is skipping
        // every response a backend did not label.
        assert!(should_compress(
            &Response::new(),
            &ResBody::Once(Bytes::from_static(b"body")),
            ByteSize(0)
        ));
    }

    #[test]
    fn a_streaming_response_of_unknown_length_is_compressed() {
        // Nothing says it is small, and refusing to compress everything a
        // backend streams would defeat the point of streaming compression.
        let mut response = Response::new();
        response.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/plain"),
        );
        assert!(should_compress(&response, &streaming_body(), ByteSize(1024)));
    }
}
