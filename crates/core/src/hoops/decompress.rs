use std::io::Cursor;

use async_compression::tokio::bufread::{BrotliDecoder, DeflateDecoder, GzipDecoder, ZstdDecoder};
use http::header::{CONTENT_ENCODING, CONTENT_LENGTH};
use salvo::http::ReqBody;
use salvo::{Depot, FlowCtrl, Request, Response, async_trait};
use tokio::io::AsyncReadExt;
use tracing::debug;

/// Request body decompression middleware.
///
/// Inspects the `Content-Encoding` request header and decompresses the body
/// before forwarding to the next handler. Supports gzip, brotli, zstd, and
/// deflate. After decompression, the `Content-Encoding` header is removed and
/// `Content-Length` is updated.
pub struct DecompressHoop {
    /// Maximum decompressed body size to prevent decompression bombs.
    max_size: usize,
}

impl DecompressHoop {
    /// Create a new decompression middleware.
    ///
    /// `max_size` limits the decompressed output to prevent zip bombs.
    /// Default: 64 MiB.
    pub fn new(max_size: Option<usize>) -> Self {
        Self {
            max_size: max_size.unwrap_or(64 * 1024 * 1024),
        }
    }
}

#[async_trait]
impl salvo::Handler for DecompressHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let encoding = req
            .headers()
            .get(CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_ascii_lowercase());

        let encoding = match encoding {
            Some(e) if e == "gzip" || e == "br" || e == "zstd" || e == "deflate" => e,
            _ => {
                // No compression or unsupported — pass through.
                ctrl.call_next(req, depot, res).await;
                return;
            }
        };

        // Read the compressed body.
        let compressed = match req.payload().await {
            Ok(bytes) => bytes.to_vec(),
            Err(_) => {
                ctrl.call_next(req, depot, res).await;
                return;
            }
        };

        if compressed.is_empty() {
            ctrl.call_next(req, depot, res).await;
            return;
        }

        // Decompress.
        let decompressed = match decompress_bytes(&compressed, &encoding, self.max_size).await {
            Ok(d) => d,
            Err(e) => {
                debug!(error = %e, encoding = encoding.as_str(), "request decompression failed");
                res.status_code(http::StatusCode::BAD_REQUEST);
                res.body("decompression failed");
                ctrl.skip_rest();
                return;
            }
        };

        debug!(
            encoding = encoding.as_str(),
            compressed = compressed.len(),
            decompressed = decompressed.len(),
            "decompressed request body"
        );

        // Replace the body with the decompressed content.
        req.headers_mut().remove(CONTENT_ENCODING);
        req.headers_mut()
            .insert(CONTENT_LENGTH, decompressed.len().into());
        *req.body_mut() = ReqBody::Once(decompressed.into());

        ctrl.call_next(req, depot, res).await;
    }
}

async fn decompress_bytes(data: &[u8], encoding: &str, max_size: usize) -> Result<Vec<u8>, String> {
    let cursor = Cursor::new(data);
    let reader = tokio::io::BufReader::new(cursor);
    let mut output = Vec::new();

    match encoding {
        "gzip" => {
            let mut decoder = GzipDecoder::new(reader);
            read_limited(&mut decoder, &mut output, max_size).await?;
        }
        "br" => {
            let mut decoder = BrotliDecoder::new(reader);
            read_limited(&mut decoder, &mut output, max_size).await?;
        }
        "zstd" => {
            let mut decoder = ZstdDecoder::new(reader);
            read_limited(&mut decoder, &mut output, max_size).await?;
        }
        "deflate" => {
            let mut decoder = DeflateDecoder::new(reader);
            read_limited(&mut decoder, &mut output, max_size).await?;
        }
        _ => return Err(format!("unsupported encoding: {encoding}")),
    }

    Ok(output)
}

async fn read_limited<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    output: &mut Vec<u8>,
    max_size: usize,
) -> Result<(), String> {
    let mut buf = [0u8; 8192];
    loop {
        let n = reader
            .read(&mut buf)
            .await
            .map_err(|e| format!("decompression error: {e}"))?;
        if n == 0 {
            break;
        }
        if output.len() + n > max_size {
            return Err(format!(
                "decompressed body exceeds limit ({max_size} bytes)"
            ));
        }
        output.extend_from_slice(&buf[..n]);
    }
    Ok(())
}
