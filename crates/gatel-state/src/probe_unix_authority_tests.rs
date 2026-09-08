use super::*;

#[tokio::test]
async fn unix_http_authority_is_valid_and_can_be_overridden() {
    tokio::time::timeout(Duration::from_secs(5), async {
        for (index, (protocol, host)) in [
            (UpstreamProtocol::Http1, None),
            (UpstreamProtocol::Http2, None),
            (UpstreamProtocol::Http1, Some("service.internal")),
            (UpstreamProtocol::Http2, Some("service.internal")),
        ]
        .into_iter()
        .enumerate()
        {
            let path = std::env::temp_dir().join(format!(
                "gatel-probe-authority-{}-{index}.sock",
                std::process::id()
            ));
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let (observed, result) = tokio::sync::oneshot::channel();
            let peer = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let observed = std::sync::Mutex::new(Some(observed));
                let service =
                    hyper::service::service_fn(move |request: http::Request<hyper::body::Incoming>| {
                        let authority = if protocol == UpstreamProtocol::Http2 {
                            assert_eq!(request.uri().scheme_str(), Some("http"));
                            request.uri().authority().unwrap().as_str().to_string()
                        } else {
                            request.headers()[http::header::HOST]
                                .to_str()
                                .unwrap()
                                .to_string()
                        };
                        observed.lock().unwrap().take().unwrap().send(authority).unwrap();
                        async {
                            Ok::<_, std::convert::Infallible>(http::Response::new(Empty::<Bytes>::new()))
                        }
                    });
                if protocol == UpstreamProtocol::Http2 {
                    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                } else {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                }
            });
            probe_unix(
                &path,
                &HealthKind::Http {
                    path: "/healthz".into(),
                    status: 200,
                    host: host.map(str::to_string),
                },
                protocol,
                Duration::from_secs(2),
            )
            .await
            .unwrap();
            assert_eq!(result.await.unwrap(), host.unwrap_or("localhost"));
            peer.await.unwrap();
            std::fs::remove_file(path).unwrap();
        }
    })
    .await
    .expect("Unix probes must complete and close their drivers");
}
