use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
use futures_util::TryStreamExt;
use salvo_core::http::{ReqBody, StatusCode};
use tokio::io::AsyncReadExt;
use tokio_util::io::StreamReader;

pub(super) struct Decoded {
    pub data: Vec<u8>,
    pub _permit: tokio::sync::OwnedSemaphorePermit,
}
impl AsRef<[u8]> for Decoded {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

pub(super) async fn decode(
    input: ReqBody,
    encoding: &str,
    config: &super::Config,
) -> Result<Vec<u8>, StatusCode> {
    let seen = Arc::new(AtomicU64::new(0));
    let counted = seen.clone();
    let max = config.max_encoded.bytes();
    let stream = http_body_util::BodyStream::new(input)
        .map_err(std::io::Error::other)
        .and_then(move |frame| {
            let result = match frame.into_data() {
                Ok(bytes) => {
                    let count = counted.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    if bytes.len() as u64 > max.saturating_sub(count) {
                        Err(std::io::Error::other(gatel_plugin_runtime::BodyTooLarge {
                            limit: max,
                        }))
                    } else {
                        Ok(bytes)
                    }
                }
                Err(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "encoded request trailers are unsupported",
                )),
            };
            futures_util::future::ready(result)
        });
    // A decoder can consume many empty members without producing output.
    // Bound each input slice and yield there too, not only after output, so
    // one ready body/frame cannot monopolize a runtime thread until EOF.
    let stream = stream
        .map_ok(|bytes| {
            futures_util::stream::unfold(bytes, |mut bytes| async move {
                tokio::task::yield_now().await;
                if bytes.is_empty() {
                    None
                } else {
                    let chunk = bytes.split_to(bytes.len().min(8192));
                    Some((Ok::<_, std::io::Error>(chunk), bytes))
                }
            })
        })
        .try_flatten();
    let reader = StreamReader::new(Box::pin(stream));
    macro_rules! run {
        ($decoder:expr) => {{
            let mut decoder = $decoder;
            decoder.multiple_members(true);
            let mut output = Vec::new();
            let mut chunk = [0; 8192];
            loop {
                let size = decoder.read(&mut chunk).await.map_err(classify)?;
                if size == 0 {
                    break;
                }
                if size as u64 > config.max_decoded.bytes().saturating_sub(output.len() as u64) {
                    return Err(StatusCode::PAYLOAD_TOO_LARGE);
                }
                output.extend_from_slice(&chunk[..size]);
                tokio::task::yield_now().await;
            }
            // Never silently ignore a trailing member or unread input frames.
            if decoder.into_inner().read(&mut [0]).await.map_err(classify)? != 0 {
                return Err(StatusCode::BAD_REQUEST);
            }
            if seen.load(Ordering::Relaxed) == 0 {
                return Err(StatusCode::BAD_REQUEST);
            }
            if output.len() as u64
                > seen
                    .load(Ordering::Relaxed)
                    .saturating_mul(config.max_ratio as u64)
            {
                return Err(StatusCode::PAYLOAD_TOO_LARGE);
            }
            Ok(output)
        }};
    }
    match encoding {
        "gzip" => run!(GzipDecoder::new(reader)),
        "br" => run!(BrotliDecoder::new(reader)),
        "zstd" => run!(ZstdDecoder::with_params(
            reader,
            &[async_compression::codecs::zstd::params::DParameter::window_log_max(23)]
        )),
        "deflate" => run!(ZlibDecoder::new(reader)),
        _ => Err(StatusCode::UNSUPPORTED_MEDIA_TYPE),
    }
}
fn classify(error: std::io::Error) -> StatusCode {
    if gatel_plugin_runtime::BodyTooLarge::find(&error).is_some() {
        StatusCode::PAYLOAD_TOO_LARGE
    } else {
        StatusCode::BAD_REQUEST
    }
}
