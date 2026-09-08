use salvo_core::http::ReqBody;
use tokio::io::AsyncReadExt;

use super::*;

#[tokio::test]
async fn input_frames_after_a_complete_member_are_not_ignored() {
    for codec in ["gzip", "br", "zstd", "deflate"] {
        let wire = encoded(b"fragmented payload", codec).await;
        for mode in 0..4 {
            let mut frames: Vec<Result<http_body::Frame<bytes::Bytes>, salvo_core::BoxedError>> = wire
                .iter()
                .map(|byte| Ok(http_body::Frame::data(bytes::Bytes::copy_from_slice(&[*byte]))))
                .collect();
            let expected = match mode {
                0 => None,
                1 => {
                    frames.push(Err(Box::new(gatel_plugin_runtime::BodyTooLarge { limit: 1 })));
                    Some(StatusCode::PAYLOAD_TOO_LARGE)
                }
                2 => {
                    frames.push(Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "fixture reset",
                    ))));
                    Some(StatusCode::BAD_REQUEST)
                }
                _ => {
                    frames.push(Ok(http_body::Frame::trailers(salvo_core::http::HeaderMap::new())));
                    Some(StatusCode::BAD_REQUEST)
                }
            };
            let input = ReqBody::Boxed {
                inner: Box::pin(http_body_util::StreamBody::new(futures_util::stream::iter(
                    frames,
                ))),
                fuse_config: None,
            };
            let result = body::decode(input, codec, &config()).await;
            if let Some(status) = expected {
                assert_eq!(result, Err(status), "{codec}, mode {mode}");
            } else {
                assert_eq!(result.unwrap(), b"fragmented payload");
            }
        }
    }
}

#[tokio::test]
async fn independent_python_fixtures_validate_checksums_not_just_roundtrip_our_encoder() {
    // Generated independently with Python gzip.compress(..., mtime=0) and
    // zlib.compress on b'independent fixture\x00with binary\xff'.
    let gzip = vec![
        31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 203, 204, 75, 73, 45, 72, 5, 18, 121, 37, 10, 105, 153, 21, 37,
        165, 69, 169, 12, 229, 153, 37, 25, 10, 73, 153, 121, 137, 69, 149, 255, 1, 137, 45, 60, 167, 32, 0,
        0, 0,
    ];
    let zlib = vec![
        120, 156, 203, 204, 75, 73, 45, 72, 205, 75, 73, 205, 43, 81, 72, 203, 172, 40, 41, 45, 74, 101, 40,
        207, 44, 201, 80, 72, 202, 204, 75, 44, 170, 252, 15, 0, 208, 75, 13, 22,
    ];
    let plain = b"independent fixture\x00with binary\xff";
    for (codec, valid, checksum_offset) in [("gzip", gzip, 8), ("deflate", zlib, 4)] {
        assert_eq!(
            body::decode(input(valid.clone()), codec, &config())
                .await
                .unwrap(),
            plain
        );
        let mut corrupt = valid;
        let position = corrupt.len() - checksum_offset;
        corrupt[position] ^= 1;
        assert_eq!(
            body::decode(input(corrupt), codec, &config()).await,
            Err(StatusCode::BAD_REQUEST)
        );
    }
}

#[tokio::test]
async fn empty_member_input_yields_and_cancellation_drops_the_source() {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Source {
        bytes: Vec<u8>,
        dropped: Arc<AtomicBool>,
    }
    impl AsRef<[u8]> for Source {
        fn as_ref(&self) -> &[u8] {
            &self.bytes
        }
    }
    impl Drop for Source {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }
    let empty = encoded(b"", "gzip").await;
    let bytes = empty.repeat(200);
    let dropped = Arc::new(AtomicBool::new(false));
    let source = bytes::Bytes::from_owner(Source {
        bytes,
        dropped: dropped.clone(),
    });
    let config = config();
    let mut future = Box::pin(body::decode(ReqBody::Once(source), "gzip", &config));
    assert!(
        futures_util::poll!(&mut future).is_pending(),
        "ready empty members must not run to completion in one poll"
    );
    assert!(!dropped.load(Ordering::SeqCst));
    drop(future);
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn zstd_window_limit_applies_to_the_first_and_later_members() {
    // Zstd frame: no content size/dictionary/checksum, 16 MiB window,
    // one final raw block holding 'x'. A normal decoder must accept it.
    let frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x70, 0x09, 0x00, 0x00, b'x'];
    let mut plain = Vec::new();
    async_compression::tokio::bufread::ZstdDecoder::new(frame.as_slice())
        .read_to_end(&mut plain)
        .await
        .unwrap();
    assert_eq!(plain, b"x");
    assert_eq!(
        body::decode(input(frame.clone()), "zstd", &config()).await,
        Err(StatusCode::BAD_REQUEST)
    );
    let mut members = encoded(b"allowed first member", "zstd").await;
    members.extend(frame);
    assert_eq!(
        body::decode(input(members), "zstd", &config()).await,
        Err(StatusCode::BAD_REQUEST)
    );
}

fn config() -> Config {
    Config {
        max_encoded: ByteSize(8192),
        max_decoded: ByteSize(8192),
        max_ratio: 1000,
        max_inflight: 1,
        timeout_ms: 50,
    }
}

#[test]
fn compatible_reload_keeps_the_same_admission_state_contract() {
    let original = config();
    let mut changed = original.clone();
    changed.max_decoded = ByteSize(128);
    changed.max_ratio = 2;
    changed.timeout_ms = 10;
    assert_eq!(
        original.state().unwrap().compatibility,
        changed.state().unwrap().compatibility
    );
    changed.max_inflight = 2;
    assert_ne!(
        original.state().unwrap().compatibility,
        changed.state().unwrap().compatibility
    );
}
async fn encoded(plain: &[u8], encoding: &str) -> Vec<u8> {
    use async_compression::tokio::bufread::{BrotliEncoder, GzipEncoder, ZlibEncoder, ZstdEncoder};
    let mut result = Vec::new();
    macro_rules! run {
        ($encoder:ident) => {
            $encoder::new(plain).read_to_end(&mut result).await.unwrap()
        };
    }
    match encoding {
        "gzip" => run!(GzipEncoder),
        "br" => run!(BrotliEncoder),
        "deflate" => run!(ZlibEncoder),
        "zstd" => run!(ZstdEncoder),
        _ => unreachable!(),
    };
    result
}
fn input(data: Vec<u8>) -> ReqBody {
    ReqBody::Once(data.into())
}

#[test]
fn invalid_budgets_are_rejected_during_configuration() {
    for name in [
        "max-encoded",
        "max-decoded",
        "max-ratio",
        "max-inflight",
        "timeout-ms",
    ] {
        let origin = gatel_model::Origin::synthetic("decode test");
        let node = Node::new("decode", origin.clone()).with_prop(name, gatel_model::Value::int(0, origin));
        let mut diagnostics = Diagnostics::new();
        assert!(Decode.decode(&node, &mut diagnostics).is_none(), "{name}");
        assert!(diagnostics.has_errors());
    }
}

#[tokio::test]
async fn cancellation_releases_the_inflight_slot() {
    let handler = DecodeHandler {
        config: config(),
        budget: Arc::new(Semaphore::new(1)),
    };
    let mut req = Request::default();
    req.headers_mut()
        .insert("content-encoding", "gzip".parse().unwrap());
    *req.body_mut() = ReqBody::Boxed {
        inner: Box::pin(http_body_util::StreamBody::new(futures_util::stream::pending::<
            Result<_, salvo_core::BoxedError>,
        >())),
        fuse_config: None,
    };
    let (mut depot, mut res, mut ctrl) = (Depot::new(), Response::default(), FlowCtrl::new(vec![]));
    let mut pending = Box::pin(handler.handle(&mut req, &mut depot, &mut res, &mut ctrl));
    assert!(futures_util::poll!(&mut pending).is_pending());
    assert_eq!(handler.budget.available_permits(), 0);
    drop(pending);
    assert_eq!(handler.budget.available_permits(), 1);
}

#[tokio::test]
async fn all_codecs_validate_complete_streams_and_enforce_both_limits_and_ratio() {
    for codec in ["gzip", "br", "deflate", "zstd"] {
        let plain = b"valid request payload ".repeat(100);
        let compressed = encoded(&plain, codec).await;
        assert_eq!(
            body::decode(input(compressed.clone()), codec, &config())
                .await
                .unwrap(),
            plain,
            "{codec}"
        );
        for (encoded_max, decoded_max, ratio) in [(1, 8192, 1000), (8192, 1, 1000), (8192, 8192, 1)] {
            let mut config = config();
            config.max_encoded = ByteSize(encoded_max);
            config.max_decoded = ByteSize(decoded_max);
            config.max_ratio = ratio;
            assert_eq!(
                body::decode(input(compressed.clone()), codec, &config).await,
                Err(StatusCode::PAYLOAD_TOO_LARGE),
                "{codec}"
            );
        }
        assert_eq!(
            body::decode(input(Vec::new()), codec, &config()).await,
            Err(StatusCode::BAD_REQUEST)
        );
        assert!(
            body::decode(
                input(compressed[..compressed.len() / 2].to_vec()),
                codec,
                &config()
            )
            .await
            .is_err(),
            "{codec}"
        );
        let mut garbage = compressed.clone();
        garbage.extend_from_slice(b"invalid trailing data");
        assert!(
            body::decode(input(garbage), codec, &config()).await.is_err(),
            "{codec}"
        );
    }
}

#[tokio::test]
async fn concatenated_gzip_members_are_not_silently_discarded() {
    let mut wire = encoded(b"first", "gzip").await;
    wire.extend(encoded(b"second", "gzip").await);
    assert_eq!(
        body::decode(input(wire), "gzip", &config()).await.unwrap(),
        b"firstsecond"
    );
}

#[tokio::test]
async fn transformed_bytes_keep_the_admission_permit_until_the_last_clone_drops() {
    use http_body_util::BodyExt;
    let handler = DecodeHandler {
        config: config(),
        budget: Arc::new(Semaphore::new(1)),
    };
    let compressed = encoded(b"hello", "gzip").await;
    let mut req = Request::default();
    req.headers_mut()
        .insert("content-encoding", "gzip".parse().unwrap());
    req.headers_mut().insert("content-digest", "old".parse().unwrap());
    *req.body_mut() = input(compressed.clone());
    handler
        .handle(
            &mut req,
            &mut Depot::new(),
            &mut Response::default(),
            &mut FlowCtrl::new(vec![]),
        )
        .await;
    assert!(!req.headers().contains_key("content-encoding"));
    assert!(!req.headers().contains_key("content-digest"));
    assert_eq!(req.headers()["content-length"], "5");
    let bytes = std::mem::take(req.body_mut()).collect().await.unwrap().to_bytes();
    assert_eq!(bytes.as_ref(), b"hello");
    let retained = bytes.clone();
    drop(bytes);
    assert_eq!(handler.budget.available_permits(), 0);
    *req.body_mut() = input(compressed);
    req.headers_mut()
        .insert("content-encoding", "gzip".parse().unwrap());
    let mut res = Response::default();
    handler
        .handle(&mut req, &mut Depot::new(), &mut res, &mut FlowCtrl::new(vec![]))
        .await;
    assert_eq!(res.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
    drop(retained);
    assert_eq!(handler.budget.available_permits(), 1);
}

#[tokio::test]
async fn unsupported_codings_and_transformation_prohibitions_fail_closed() {
    let handler = DecodeHandler {
        config: config(),
        budget: Arc::new(Semaphore::new(1)),
    };
    for (encoding, extra) in [
        ("unknown", None),
        ("gzip, br", None),
        ("gzip", Some("content-range")),
        ("gzip", Some("cache-control")),
        ("gzip", Some("content-encoding")),
    ] {
        let mut req = Request::default();
        req.headers_mut()
            .insert("content-encoding", encoding.parse().unwrap());
        if let Some(header) = extra {
            req.headers_mut().append(header, "no-transform".parse().unwrap());
        }
        let mut res = Response::default();
        handler
            .handle(&mut req, &mut Depot::new(), &mut res, &mut FlowCtrl::new(vec![]))
            .await;
        assert_eq!(res.status_code, Some(StatusCode::UNSUPPORTED_MEDIA_TYPE));
        assert_eq!(handler.budget.available_permits(), 1);
    }
}

#[tokio::test]
async fn stalled_input_times_out_and_releases_admission() {
    let handler = DecodeHandler {
        config: config(),
        budget: Arc::new(Semaphore::new(1)),
    };
    let mut req = Request::default();
    req.headers_mut()
        .insert("content-encoding", "gzip".parse().unwrap());
    *req.body_mut() = ReqBody::Boxed {
        inner: Box::pin(http_body_util::StreamBody::new(futures_util::stream::pending::<
            Result<_, salvo_core::BoxedError>,
        >())),
        fuse_config: None,
    };
    let mut res = Response::default();
    handler
        .handle(&mut req, &mut Depot::new(), &mut res, &mut FlowCtrl::new(vec![]))
        .await;
    assert_eq!(res.status_code, Some(StatusCode::REQUEST_TIMEOUT));
    assert_eq!(handler.budget.available_permits(), 1);
}
