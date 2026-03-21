use std::io::Cursor;

use async_compression::Level;
use async_compression::tokio::bufread::{BrotliEncoder, DeflateEncoder, GzipEncoder, ZstdEncoder};
use http::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE};
use http_body_util::BodyExt;
use salvo::http::ResBody;
use salvo::{Depot, FlowCtrl, Request, Response, async_trait};
use tokio::io::AsyncReadExt;
use tracing::debug;

/// Minimum response size (in bytes) to bother compressing.
const MIN_COMPRESS_SIZE: usize = 256;

/// Supported compression encodings, in preference order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Zstd,
    Brotli,
    Gzip,
    Deflate,
}

impl Encoding {
    fn as_str(self) -> &'static str {
        match self {
            Encoding::Zstd => "zstd",
            Encoding::Brotli => "br",
            Encoding::Gzip => "gzip",
            Encoding::Deflate => "deflate",
        }
    }
}

/// Response compression middleware.
///
/// Inspects the `Accept-Encoding` request header, selects the best supported
/// encoding, and compresses the response body if it is a compressible content
/// type and large enough to be worth compressing.
pub struct CompressHoop {
    /// Enabled encodings, in preference order.
    enabled: Vec<Encoding>,
    /// Optional compression level. When set, uses `Level::Precise(level)`.
    level: Option<u32>,
}

impl CompressHoop {
    /// Create from a list of encoding names (e.g. `["gzip", "zstd", "br"]`) and an optional level.
    pub fn new(encodings: &[String], level: Option<u32>) -> Self {
        let mut enabled = Vec::new();
        for name in encodings {
            match name.as_str() {
                "gzip" => enabled.push(Encoding::Gzip),
                "zstd" => enabled.push(Encoding::Zstd),
                "br" | "brotli" => enabled.push(Encoding::Brotli),
                "deflate" => enabled.push(Encoding::Deflate),
                other => {
                    debug!(encoding = other, "unknown encoding requested, skipping");
                }
            }
        }
        // If nothing valid was provided, default to all four.
        if enabled.is_empty() {
            enabled = vec![
                Encoding::Zstd,
                Encoding::Brotli,
                Encoding::Gzip,
                Encoding::Deflate,
            ];
        }
        Self { enabled, level }
    }
}

#[async_trait]
impl salvo::Handler for CompressHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        // Determine which encoding the client accepts that we also support.
        let chosen = choose_encoding(req.headers(), &self.enabled);

        ctrl.call_next(req, depot, res).await;

        // If no acceptable encoding, return uncompressed.
        let encoding = match chosen {
            Some(e) => e,
            None => return,
        };

        // Skip if response already has Content-Encoding.
        if res.headers().contains_key(CONTENT_ENCODING) {
            return;
        }

        // Only compress compressible content types.
        if !is_compressible_content_type(res.headers()) {
            return;
        }

        // Take the body and collect it.
        let body = res.take_body();
        let body_bytes = match collect_res_body(body).await {
            Ok(bytes) => bytes,
            Err(_) => return,
        };

        // Skip if body is too small.
        if body_bytes.len() < MIN_COMPRESS_SIZE {
            res.body(body_bytes);
            return;
        }

        // Compress.
        let compressed = match compress_bytes(&body_bytes, encoding, self.level).await {
            Ok(c) => c,
            Err(_) => {
                res.body(body_bytes);
                return;
            }
        };

        debug!(
            encoding = encoding.as_str(),
            original = body_bytes.len(),
            compressed = compressed.len(),
            "compressed response"
        );

        // Update headers.
        res.headers_mut()
            .insert(CONTENT_ENCODING, encoding.as_str().parse().unwrap());
        res.headers_mut().remove(CONTENT_LENGTH);
        res.headers_mut()
            .insert(CONTENT_LENGTH, compressed.len().into());

        res.body(compressed);
    }
}

/// Collect a Salvo ResBody into bytes (public for reuse by other middleware).
pub async fn collect_res_body_bytes(body: ResBody) -> Result<Vec<u8>, ()> {
    collect_res_body(body).await
}

/// Collect a Salvo ResBody into bytes.
async fn collect_res_body(body: ResBody) -> Result<Vec<u8>, ()> {
    match body {
        ResBody::None => Ok(Vec::new()),
        ResBody::Once(bytes) => Ok(bytes.to_vec()),
        ResBody::Boxed(boxed) => {
            let collected = boxed.collect().await.map_err(|_| ())?;
            Ok(collected.to_bytes().to_vec())
        }
        other => {
            // For Hyper and other body types, try to collect through the Body trait.
            use http_body::Body;
            let mut buf = Vec::new();
            let mut pinned = Box::pin(other);
            loop {
                match std::future::poll_fn(|cx| pinned.as_mut().poll_frame(cx)).await {
                    Some(Ok(frame)) => {
                        if let Ok(data) = frame.into_data() {
                            buf.extend_from_slice(&data);
                        }
                    }
                    Some(Err(_)) => return Err(()),
                    None => break,
                }
            }
            Ok(buf)
        }
    }
}

/// Choose the best encoding from the client's Accept-Encoding that we support.
fn choose_encoding(headers: &http::HeaderMap, enabled: &[Encoding]) -> Option<Encoding> {
    let accept = headers.get(ACCEPT_ENCODING)?.to_str().ok()?;
    // Parse quality values (simplified: we just check presence, not q values).
    // Order by our preference (the order in `enabled`).
    for enc in enabled {
        let token = enc.as_str();
        if accept.contains(token) || accept.contains("*") {
            return Some(*enc);
        }
    }
    None
}

/// Returns true if the response Content-Type is a compressible text-like MIME.
fn is_compressible_content_type(headers: &http::HeaderMap) -> bool {
    let ct = match headers.get(CONTENT_TYPE) {
        Some(v) => match v.to_str() {
            Ok(s) => s.to_ascii_lowercase(),
            Err(_) => return false,
        },
        // No Content-Type — don't compress.
        None => return false,
    };

    // text/* is always compressible.
    if ct.starts_with("text/") {
        return true;
    }

    const COMPRESSIBLE: &[&str] = &[
        "application/json",
        "application/javascript",
        "application/xml",
        "application/xhtml+xml",
        "application/rss+xml",
        "application/atom+xml",
        "application/wasm",
        "application/manifest+json",
        "application/ld+json",
        "application/graphql+json",
        "application/geo+json",
        "application/vnd.api+json",
        "image/svg+xml",
    ];

    for mime in COMPRESSIBLE {
        if ct.starts_with(mime) {
            return true;
        }
    }

    false
}

/// Compress bytes with the given encoding and optional level.
async fn compress_bytes(
    data: &[u8],
    encoding: Encoding,
    level: Option<u32>,
) -> Result<Vec<u8>, crate::ProxyError> {
    let cursor = Cursor::new(data);
    let reader = tokio::io::BufReader::new(cursor);
    let mut output = Vec::new();
    let compress_level = level.map(|l| Level::Precise(l as i32)).unwrap_or_default();

    match encoding {
        Encoding::Gzip => {
            let mut encoder = GzipEncoder::with_quality(reader, compress_level);
            encoder
                .read_to_end(&mut output)
                .await
                .map_err(|e| crate::ProxyError::Internal(format!("gzip compression error: {e}")))?;
        }
        Encoding::Zstd => {
            let mut encoder = ZstdEncoder::with_quality(reader, compress_level);
            encoder
                .read_to_end(&mut output)
                .await
                .map_err(|e| crate::ProxyError::Internal(format!("zstd compression error: {e}")))?;
        }
        Encoding::Brotli => {
            let mut encoder = BrotliEncoder::with_quality(reader, compress_level);
            encoder.read_to_end(&mut output).await.map_err(|e| {
                crate::ProxyError::Internal(format!("brotli compression error: {e}"))
            })?;
        }
        Encoding::Deflate => {
            let mut encoder = DeflateEncoder::with_quality(reader, compress_level);
            encoder.read_to_end(&mut output).await.map_err(|e| {
                crate::ProxyError::Internal(format!("deflate compression error: {e}"))
            })?;
        }
    }

    Ok(output)
}
