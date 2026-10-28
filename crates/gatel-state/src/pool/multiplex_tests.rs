//! Real H2 streams: one slow exchange must not monopolize a TCP connection.
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use gatel_model::{Origin as ConfigOrigin, ResourceId, ResourceKind, SourceId};
use gatel_plan::{Pool, Timeouts, Upstream, UpstreamProtocol};
use http_body_util::{BodyExt, Empty};
use hyper::body::Frame;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};

use super::super::{Body, CheckoutError, ConnectionPool, Lease};
use crate::endpoint::{Endpoint, EndpointSet};

struct GateBody {
    gate: Option<oneshot::Receiver<()>>,
    data: Option<Bytes>,
    dropped: Option<oneshot::Sender<()>>,
}

impl hyper::body::Body for GateBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if let Some(gate) = &mut self.gate {
            if Pin::new(gate).poll(cx).is_pending() {
                return Poll::Pending;
            }
            self.gate.take();
        }
        Poll::Ready(self.data.take().map(|data| Ok(Frame::data(data))))
    }

    fn is_end_stream(&self) -> bool {
        self.data.is_none()
    }
}

impl Drop for GateBody {
    fn drop(&mut self) {
        if let Some(done) = self.dropped.take() {
            let _ = done.send(());
        }
    }
}

struct Started {
    connection: usize,
    path: String,
    release: oneshot::Sender<()>,
}

struct Origin {
    address: SocketAddr,
    started: mpsc::UnboundedReceiver<Started>,
    opened: watch::Receiver<usize>,
    goaway: watch::Sender<usize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Origin {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Origin {
    async fn new(remote_streams: u32) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (events, started) = mpsc::unbounded_channel();
        let (opened_tx, opened) = watch::channel(0usize);
        let (goaway, going) = watch::channel(0usize);
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((socket, _)) = accepted else { break };
                        let id = *opened_tx.borrow() + 1;
                        opened_tx.send_replace(id);
                        let events = events.clone();
                        let mut going = going.clone();
                        going.borrow_and_update();
                        connections.spawn(async move {
                            let service = hyper::service::service_fn(move |request: http::Request<hyper::body::Incoming>| {
                                let events = events.clone();
                                async move {
                                    let path = request.uri().path().to_owned();
                                    let (release, gate) = oneshot::channel();
                                    events.send(Started { connection: id, path: path.clone(), release }).unwrap();
                                    let body = if path == "/head" {
                                        let _ = gate.await;
                                        GateBody { gate: None, data: Some(Bytes::from_static(b"head")), dropped: None }
                                    } else if path == "/early" {
                                        // Keep the upload alive after returning an empty response.
                                        tokio::spawn(async move { let _ = gate.await; let _ = request.into_body().collect().await; });
                                        GateBody { gate: None, data: None, dropped: None }
                                    } else {
                                        GateBody { gate: Some(gate), data: Some(Bytes::from_static(b"ok")), dropped: None }
                                    };
                                    Ok::<_, Infallible>(http::Response::new(body))
                                }
                            });
                            let mut builder = hyper::server::conn::http2::Builder::new(TokioExecutor::new());
                            builder.max_concurrent_streams(remote_streams);
                            let connection = builder.serve_connection(TokioIo::new(socket), service);
                            tokio::pin!(connection);
                            tokio::select! {
                                _ = going.changed() => { connection.as_mut().graceful_shutdown(); let _ = connection.await; },
                                _ = &mut connection => {},
                            }
                        });
                    },
                    _ = connections.join_next(), if !connections.is_empty() => {},
                }
            }
        });
        Self {
            address,
            started,
            opened,
            goaway,
            task,
        }
    }

    async fn next(&mut self, path: &str) -> Started {
        let event = tokio::time::timeout(Duration::from_secs(3), self.started.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.path, path);
        event
    }

    fn count(&self) -> usize {
        *self.opened.borrow()
    }
}

fn pool(origin: &Origin, config: Pool) -> (Arc<Endpoint>, Arc<ConnectionPool>) {
    pool_at(origin.address, config)
}

fn pool_at(address: SocketAddr, config: Pool) -> (Arc<Endpoint>, Arc<ConnectionPool>) {
    let upstream = Upstream {
        id: ResourceId::new(ResourceKind::Upstream, "h2"),
        owner: SourceId::file("main"),
        endpoints: vec![gatel_plan::Endpoint {
            id: "a".into(),
            target: gatel_plan::Target::Address(address),
            weight: 1,
            priority: 0,
            origin: ConfigOrigin::synthetic("test"),
        }],
        balance: Default::default(),
        timeouts: Default::default(),
        pool: config,
        retry: None,
        health: None,
        passive_health: None,
        protocol: UpstreamProtocol::Http2,
        tls: None,
        resolve: Default::default(),
        origin: ConfigOrigin::synthetic("test"),
    };
    let endpoints = EndpointSet::new(&upstream);
    (
        endpoints.all()[0].clone(),
        Arc::new(ConnectionPool::new(
            config,
            UpstreamProtocol::Http2,
            Default::default(),
            None,
        )),
    )
}

fn request(path: &str) -> http::Request<Body> {
    http::Request::builder()
        .uri(format!("http://backend{path}"))
        .body(Empty::<Bytes>::new().map_err(|never| match never {}).boxed())
        .unwrap()
}

async fn headers(
    pool: &ConnectionPool,
    endpoint: &Arc<Endpoint>,
    path: &str,
) -> (Lease, http::Response<hyper::body::Incoming>) {
    let mut lease = pool.checkout(endpoint, &Timeouts::default()).await.unwrap();
    let response = lease.send(request(path)).await.unwrap();
    assert_eq!(response.version(), http::Version::HTTP_2);
    (lease, response)
}

#[tokio::test]
async fn a_single_physical_connection_serves_overlapping_streams() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut origin = Origin::new(128).await;
        let (endpoint, pool) = pool(
            &origin,
            Pool {
                max_per_endpoint: 1,
                ..Default::default()
            },
        );
        let (slow, slow_response) = headers(&pool, &endpoint, "/slow").await;
        let slow_event = origin.next("/slow").await;
        let (fast, fast_response) = headers(&pool, &endpoint, "/fast").await;
        let fast_event = origin.next("/fast").await;
        assert_eq!(slow_event.connection, fast_event.connection);
        assert_eq!(origin.count(), 1);
        assert_eq!(pool.stats()[0].streams_in_use, 2);
        fast_event.release.send(()).unwrap();
        assert_eq!(
            fast_response.into_body().collect().await.unwrap().to_bytes(),
            "ok"
        );
        drop(fast);
        assert_eq!(pool.stats()[0].streams_in_use, 1);
        slow_event.release.send(()).unwrap();
        assert_eq!(
            slow_response.into_body().collect().await.unwrap().to_bytes(),
            "ok"
        );
        drop(slow);
        assert_eq!(pool.stats()[0].opened, 1);
        assert_eq!(pool.stats()[0].streams_in_use, 0);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn local_endpoint_connection_and_remote_settings_bounds_apply_before_send() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for (physical, per_connection, per_endpoint, remote) in [(2, 128, 1, 128), (1, 1, 128, 128), (1, 128, 128, 1)] {
            let mut origin = Origin::new(remote).await;
            let (endpoint, pool) = pool(&origin, Pool { max_per_endpoint: physical, max_h2_streams_per_connection: per_connection, max_h2_streams_per_endpoint: per_endpoint, ..Default::default() });
            let (first, response) = headers(&pool, &endpoint, "/first").await;
            let event = origin.next("/first").await;
            let timeouts = Timeouts { queue: Duration::from_millis(50), ..Default::default() };
            let result = pool.checkout(&endpoint, &timeouts).await;
            assert!(matches!(result, Err(CheckoutError::QueueTimeout(_))), "physical={physical} per_connection={per_connection} per_endpoint={per_endpoint} remote={remote}: {result:?}");
            assert_eq!(origin.count(), 1, "a stream cap must not create extra TCP connections");
            event.release.send(()).unwrap();
            response.into_body().collect().await.unwrap();
            drop(first);
            let (next, response) = headers(&pool, &endpoint, "/next").await;
            let event = origin.next("/next").await;
            assert_eq!(event.connection, 1);
            event.release.send(()).unwrap();
            response.into_body().collect().await.unwrap();
            drop(next);
        }
    }).await.unwrap();
}

#[tokio::test]
async fn cancelled_queued_checkout_releases_waiter_budget_and_drain_wakes_the_next() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut origin = Origin::new(128).await;
        let (endpoint, pool) = pool(
            &origin,
            Pool {
                max_per_endpoint: 1,
                max_h2_streams_per_endpoint: 1,
                max_waiters: 1,
                ..Default::default()
            },
        );
        let (first, response) = headers(&pool, &endpoint, "/first").await;
        let event = origin.next("/first").await;
        let timeouts = Timeouts::default();
        let mut queued = Box::pin(pool.checkout(&endpoint, &timeouts));
        std::future::poll_fn(|cx| {
            assert!(queued.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(pool.stats()[0].waiting, 1);
        assert!(matches!(
            pool.checkout(&endpoint, &timeouts).await,
            Err(CheckoutError::QueueFull { .. })
        ));
        drop(queued);
        assert_eq!(pool.stats()[0].waiting, 0);
        let mut queued = Box::pin(pool.checkout(&endpoint, &timeouts));
        std::future::poll_fn(|cx| {
            assert!(queued.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        pool.drain();
        assert!(matches!(queued.await, Err(CheckoutError::Draining)));
        event.release.send(()).unwrap();
        response.into_body().collect().await.unwrap();
        drop(first);
        assert_eq!(pool.stats()[0].streams_in_use, 0);
        assert_eq!(pool.stats()[0].idle, 0);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancelling_send_before_response_headers_does_not_poison_sibling_streams() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut origin = Origin::new(128).await;
        let (endpoint, pool) = pool(
            &origin,
            Pool {
                max_per_endpoint: 1,
                ..Default::default()
            },
        );
        let (slow, response) = headers(&pool, &endpoint, "/slow").await;
        let event = origin.next("/slow").await;
        let mut cancelled = pool.checkout(&endpoint, &Timeouts::default()).await.unwrap();
        let mut send = Box::pin(cancelled.send(request("/head")));
        let mut head = tokio::select! {
            started = origin.next("/head") => started,
            result = &mut send => panic!("head gate returned early: {result:?}"),
        };
        drop(send);
        cancelled.poison();
        drop(cancelled);
        tokio::time::timeout(Duration::from_secs(2), head.release.closed())
            .await
            .unwrap();
        assert_eq!(pool.stats()[0].streams_in_use, 1);
        event.release.send(()).unwrap();
        assert_eq!(response.into_body().collect().await.unwrap().to_bytes(), "ok");
        drop(slow);
        let (next, response) = headers(&pool, &endpoint, "/next").await;
        origin.next("/next").await.release.send(()).unwrap();
        response.into_body().collect().await.unwrap();
        drop(next);
        assert_eq!(pool.stats()[0].opened, 1);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn lowering_stream_capacity_waits_for_both_existing_streams_to_finish() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for endpoint_limit in [true, false] {
            let mut origin = Origin::new(128).await;
            let config = Pool {
                max_per_endpoint: 1,
                max_h2_streams_per_connection: 2,
                max_h2_streams_per_endpoint: 2,
                ..Default::default()
            };
            let (endpoint, pool) = pool(&origin, config);
            let (first, a) = headers(&pool, &endpoint, "/a").await;
            let a_event = origin.next("/a").await;
            let (second, b) = headers(&pool, &endpoint, "/b").await;
            let b_event = origin.next("/b").await;
            let next = if endpoint_limit {
                Pool {
                    max_h2_streams_per_endpoint: 1,
                    ..config
                }
            } else {
                Pool {
                    max_h2_streams_per_connection: 1,
                    ..config
                }
            };
            pool.adopt(next, Default::default());
            let timeouts = Timeouts::default();
            let mut queued = Box::pin(pool.checkout(&endpoint, &timeouts));
            std::future::poll_fn(|cx| {
                assert!(queued.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            a_event.release.send(()).unwrap();
            a.into_body().collect().await.unwrap();
            drop(first);
            std::future::poll_fn(|cx| {
                assert!(queued.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            b_event.release.send(()).unwrap();
            b.into_body().collect().await.unwrap();
            drop(second);
            let third = queued.await.unwrap();
            assert_eq!(pool.stats()[0].streams_in_use, 1);
            assert_eq!(pool.stats()[0].opened, 1);
            drop(third);
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn early_complete_response_does_not_release_a_still_uploading_stream() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut origin = Origin::new(128).await;
        let (endpoint, pool) = pool(
            &origin,
            Pool {
                max_per_endpoint: 1,
                max_h2_streams_per_endpoint: 1,
                ..Default::default()
            },
        );
        let (upload, gate) = oneshot::channel();
        let (dropped, finished) = oneshot::channel();
        let body = GateBody {
            gate: Some(gate),
            data: Some(Bytes::from_static(b"payload")),
            dropped: Some(dropped),
        };
        let body = body.map_err(|never| match never {}).boxed();
        let mut lease = pool.checkout(&endpoint, &Timeouts::default()).await.unwrap();
        let response = lease
            .send(
                http::Request::builder()
                    .method("POST")
                    .uri("http://backend/early")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        let event = origin.next("/early").await;
        assert!(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .is_empty()
        );
        drop(lease);
        assert_eq!(
            pool.stats()[0].streams_in_use,
            1,
            "the upload still owns a stream after the empty response"
        );
        let timeouts = Timeouts {
            queue: Duration::from_millis(50),
            ..Default::default()
        };
        assert!(matches!(
            pool.checkout(&endpoint, &timeouts).await,
            Err(CheckoutError::QueueTimeout(_))
        ));
        event.release.send(()).unwrap();
        upload.send(()).unwrap();
        finished.await.unwrap();
        let next = pool.checkout(&endpoint, &Timeouts::default()).await.unwrap();
        drop(next);
        assert_eq!(pool.stats()[0].streams_in_use, 0);
    })
    .await
    .unwrap();
}

async fn eventually(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("observable pool state did not converge");
}

#[tokio::test]
async fn goaway_stops_new_streams_but_allows_existing_response_to_finish() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut origin = Origin::new(128).await;
        let (endpoint, pool) = pool(
            &origin,
            Pool {
                max_per_endpoint: 2,
                ..Default::default()
            },
        );
        let (first, response) = headers(&pool, &endpoint, "/old").await;
        let old = origin.next("/old").await;
        let slot = pool.slot(&endpoint);
        let sender = slot.h2.0.lock().unwrap().connections[0].connection.sender.clone();
        origin.goaway.send_replace(1);
        eventually(|| sender.is_closed()).await;
        let (second, next) = headers(&pool, &endpoint, "/new").await;
        let new = origin.next("/new").await;
        assert_ne!(old.connection, new.connection);
        assert_eq!(pool.stats()[0].streams_in_use, 2);
        new.release.send(()).unwrap();
        next.into_body().collect().await.unwrap();
        drop(second);
        old.release.send(()).unwrap();
        assert_eq!(response.into_body().collect().await.unwrap().to_bytes(), "ok");
        drop(first);
        assert_eq!(pool.stats()[0].idle, 1);
        assert_eq!(pool.stats()[0].streams_in_use, 0);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn address_retirement_preserves_old_stream_and_routes_new_stream_to_replacement() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut old = Origin::new(128).await;
        let mut replacement = Origin::new(128).await;
        let (endpoint, pool) = pool(
            &old,
            Pool {
                max_per_endpoint: 2,
                ..Default::default()
            },
        );
        let (first, response) = headers(&pool, &endpoint, "/old").await;
        let event = old.next("/old").await;
        endpoint.set_addresses(vec![replacement.address]);
        assert_eq!(
            pool.retire("a", &[replacement.address]),
            0,
            "active connection remains registered"
        );
        let (second, next) = headers(&pool, &endpoint, "/new").await;
        replacement.next("/new").await.release.send(()).unwrap();
        next.into_body().collect().await.unwrap();
        drop(second);
        event.release.send(()).unwrap();
        response.into_body().collect().await.unwrap();
        drop(first);
        assert_eq!(old.count(), 1);
        assert_eq!(replacement.count(), 1);
        assert_eq!(pool.stats()[0].idle, 1);
        assert_eq!(pool.stats()[0].discarded, 1);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn lowering_physical_capacity_from_two_to_one_converges_without_cancelling_streams() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut origin = Origin::new(128).await;
        let config = Pool {
            max_per_endpoint: 2,
            max_h2_streams_per_connection: 1,
            ..Default::default()
        };
        let (endpoint, pool) = pool(&origin, config);
        let (first, a) = headers(&pool, &endpoint, "/a").await;
        let a_event = origin.next("/a").await;
        let (second, b) = headers(&pool, &endpoint, "/b").await;
        let b_event = origin.next("/b").await;
        assert_ne!(a_event.connection, b_event.connection);
        pool.adopt(
            Pool {
                max_per_endpoint: 1,
                ..config
            },
            Default::default(),
        );
        assert_eq!(pool.stats()[0].in_use, 2);
        b_event.release.send(()).unwrap();
        b.into_body().collect().await.unwrap();
        drop(second);
        assert_eq!(pool.stats()[0].in_use, 1);
        assert_eq!(pool.stats()[0].idle, 0);
        a_event.release.send(()).unwrap();
        a.into_body().collect().await.unwrap();
        drop(first);
        let (third, response) = headers(&pool, &endpoint, "/c").await;
        let c = origin.next("/c").await;
        assert_eq!(a_event.connection, c.connection);
        c.release.send(()).unwrap();
        response.into_body().collect().await.unwrap();
        drop(third);
        assert_eq!(origin.count(), 2);
        assert_eq!(pool.stats()[0].idle, 1);
        assert_eq!(pool.stats()[0].discarded, 1);
    })
    .await
    .unwrap();
}

/// Minimal wire peer: changing SETTINGS alone must wake admission, even when
/// no request/response can drive Hyper's dispatch future.
struct SettingsPeer {
    address: SocketAddr,
    update: mpsc::UnboundedSender<u32>,
    closed: oneshot::Receiver<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for SettingsPeer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl SettingsPeer {
    async fn new() -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (update, mut updates) = mpsc::unbounded_channel::<u32>();
        let (close, closed) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut preface = [0u8; 24];
            socket.read_exact(&mut preface).await.unwrap();
            assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
            socket
                .write_all(&[0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0])
                .await
                .unwrap();
            let (mut reader, mut writer) = socket.into_split();
            async fn frame(reader: &mut tokio::net::tcp::OwnedReadHalf) -> std::io::Result<(u8, u8)> {
                use tokio::io::AsyncReadExt;
                let mut head = [0u8; 9];
                reader.read_exact(&mut head).await?;
                let length = ((head[0] as usize) << 16) | ((head[1] as usize) << 8) | head[2] as usize;
                let mut payload = vec![0; length];
                reader.read_exact(&mut payload).await?;
                Ok((head[3], head[4]))
            }
            'frames: loop {
                // Preserve a partially read frame when a SETTINGS update is
                // sent concurrently; read_exact is not cancellation safe.
                let incoming = frame(&mut reader);
                tokio::pin!(incoming);
                loop {
                    tokio::select! {
                        value = updates.recv() => {
                            let Some(value) = value else { break 'frames };
                            let mut settings = vec![0,0,6,4,0,0,0,0,0, 0,3];
                            settings.extend_from_slice(&value.to_be_bytes());
                            if writer.write_all(&settings).await.is_err() { break 'frames; }
                        },
                        received = &mut incoming => {
                            match received {
                                Ok((4, flags)) if flags & 1 == 0 => {
                                    if writer.write_all(&[0,0,0,4,1,0,0,0,0]).await.is_err() { break 'frames; }
                                },
                                Ok(_) => {},
                                Err(_) => break 'frames,
                            }
                            break;
                        }
                    }
                }
            }
            let _ = close.send(());
        });
        Self {
            address,
            update,
            closed,
            task,
        }
    }
}

#[tokio::test]
async fn one_lease_cannot_send_multiple_h2_requests_or_bypass_stream_accounting() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut origin = Origin::new(128).await;
        let (endpoint, pool) = pool(
            &origin,
            Pool {
                max_per_endpoint: 1,
                max_h2_streams_per_endpoint: 1,
                ..Default::default()
            },
        );
        let (mut lease, response) = headers(&pool, &endpoint, "/first").await;
        let event = origin.next("/first").await;
        assert!(!lease.is_ready());
        assert!(matches!(
            lease.send(request("/second")).await,
            Err(super::super::SendError::AlreadySent)
        ));
        assert_eq!(pool.stats()[0].streams_in_use, 1);
        event.release.send(()).unwrap();
        response.into_body().collect().await.unwrap();
        assert!(matches!(
            lease.send(request("/third")).await,
            Err(super::super::SendError::AlreadySent)
        ));
        assert!(matches!(
            origin.started.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        drop(lease);
        assert_eq!(pool.stats()[0].streams_in_use, 0);
        let (next, response) = headers(&pool, &endpoint, "/fresh-lease").await;
        origin.next("/fresh-lease").await.release.send(()).unwrap();
        response.into_body().collect().await.unwrap();
        drop(next);
        assert_eq!(origin.count(), 1);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancelling_a_waiter_preserves_zero_settings_connection_until_the_last_waiter_leaves() {
    tokio::time::timeout(Duration::from_secs(15), async {
        for resume in [true, false] {
            let mut peer = SettingsPeer::new().await;
            let (endpoint, pool) = pool_at(
                peer.address,
                Pool {
                    max_per_endpoint: 1,
                    max_idle_per_endpoint: 0,
                    max_waiters: 2,
                    ..Default::default()
                },
            );
            let slot = pool.slot(&endpoint);
            let super::Next::Dial(dial) = super::Multiplex::next(&slot) else {
                panic!("empty pool must dial")
            };
            let (super::Sender::Http2(sender), address) =
                pool.dial(&endpoint, &Timeouts::default()).await.unwrap()
            else {
                panic!("expected H2")
            };
            eventually(|| sender.stream_limit() == 0).await;
            // Seed exactly the state after a completed dial whose initial
            // SETTINGS prohibit streams; no lease has used this connection.
            dial.finish(sender, address);
            let timeouts = Timeouts::default();
            let mut first = Box::pin(pool.checkout(&endpoint, &timeouts));
            std::future::poll_fn(|cx| {
                assert!(first.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            let mut second = Box::pin(pool.checkout(&endpoint, &timeouts));
            std::future::poll_fn(|cx| {
                assert!(second.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            assert!(matches!(
                pool.checkout(&endpoint, &timeouts).await,
                Err(CheckoutError::QueueFull { .. })
            ));
            drop(second);
            assert_eq!(pool.stats()[0].waiting, 1);
            assert_eq!(pool.stats()[0].idle, 1);
            assert_eq!(slot.h2.0.lock().unwrap().connections.len(), 1);
            if resume {
                peer.update.send(1).unwrap();
                let lease = tokio::time::timeout(Duration::from_secs(3), first)
                    .await
                    .expect("SETTINGS 0→1 alone must wake checkout")
                    .unwrap();
                drop(lease);
            } else {
                drop(first);
            }
            assert_eq!(pool.stats()[0].waiting, 0);
            assert_eq!(pool.stats()[0].idle, 0);
            assert!(slot.h2.0.lock().unwrap().connections.is_empty());
            tokio::time::timeout(Duration::from_secs(3), &mut peer.closed)
                .await
                .expect("last waiter must release unused socket")
                .unwrap();
        }
    })
    .await
    .unwrap();
}
