//! The production decoding module, without exposing a fuzz-only public API.
//! This same invariant harness is used by stable tests and libFuzzer.
use gatel_model::ByteSize;
use salvo_core::http::{ReqBody, StatusCode};
use tokio::io::AsyncReadExt;

struct Config {
    max_encoded: ByteSize,
    max_decoded: ByteSize,
    max_ratio: u32,
}
#[allow(dead_code)] // Decoded's admission owner is tested by the handler tests.
#[path = "../../src/body.rs"]
mod body;

pub async fn exercise(data: &[u8]) {
    let Some((&selector, payload)) = data.split_first() else {
        return;
    };
    let codec = ["gzip", "br", "zstd", "deflate"][selector as usize % 4];
    let mut candidate = payload[..payload.len().min(4096)].to_vec();
    let generated = selector & 4 != 0;
    let mutated = selector & 8 != 0;
    if generated {
        use async_compression::tokio::bufread::{BrotliEncoder, GzipEncoder, ZlibEncoder, ZstdEncoder};
        let plain = &candidate[..candidate.len().min(256)];
        let mut encoded = Vec::new();
        macro_rules! encode {
            ($encoder:ident) => {
                $encoder::new(plain)
                    .read_to_end(&mut encoded)
                    .await
                    .unwrap()
            };
        }
        match codec {
            "gzip" => encode!(GzipEncoder),
            "br" => encode!(BrotliEncoder),
            "zstd" => encode!(ZstdEncoder),
            "deflate" => encode!(ZlibEncoder),
            _ => unreachable!(),
        };
        candidate = encoded;
        if mutated && !candidate.is_empty() {
            let index = payload.first().copied().unwrap_or(0) as usize % candidate.len();
            candidate[index] ^= payload.get(1).copied().unwrap_or(1);
        }
    }
    let config = Config {
        max_encoded: ByteSize(4096),
        max_decoded: ByteSize(16384),
        max_ratio: 1000,
    };
    let whole = body::decode(ReqBody::Once(candidate.clone().into()), codec, &config).await;
    let width = 1 + selector as usize / 16;
    let frames: Vec<Result<_, salvo_core::BoxedError>> = candidate
        .chunks(width)
        .map(|chunk| Ok(http_body::Frame::data(bytes::Bytes::copy_from_slice(chunk))))
        .collect();
    let fragmented = ReqBody::Boxed {
        inner: Box::pin(http_body_util::StreamBody::new(futures_util::stream::iter(
            frames,
        ))),
        fuse_config: None,
    };
    let fragmented = body::decode(fragmented, codec, &config).await;
    assert_eq!(whole, fragmented, "frame boundaries must not change decoding");
    if generated && !mutated {
        assert_eq!(
            whole.as_ref().unwrap().as_slice(),
            &payload[..payload.len().min(256)]
        );
    }
    match whole {
        Ok(output) => assert!(output.len() <= 16384),
        Err(status) => assert!(matches!(
            status,
            StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE
        )),
    }
}
