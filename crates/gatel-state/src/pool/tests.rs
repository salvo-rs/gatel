//! What the pool must keep true.
//!
//! Every one of these is about a decision that is invisible from outside: a
//! connection reused rather than reopened, a limit that lands on a pool a
//! reload kept, a socket to a replaced container dropped instead of parked.
//! None of it shows up in a response, which is exactly why it needs tests.

use std::net::SocketAddr;

use gatel_model::{Origin, ResourceId, ResourceKind, SourceId};
use gatel_plan::Upstream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use crate::endpoint::EndpointSet;

/// A backend that answers a fixed response and keeps the connection open.
async fn backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buffer = [0u8; 1024];
                while let Ok(read) = stream.read(&mut buffer).await {
                    if read == 0 {
                        return;
                    }
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                        .await;
                }
            });
        }
    });
    address
}

fn set(address: SocketAddr, config: gatel_plan::Pool) -> (EndpointSet, ConnectionPool) {
    let upstream = Upstream {
        id: ResourceId::new(ResourceKind::Upstream, "api"),
        owner: SourceId::file("main"),
        endpoints: vec![gatel_plan::Endpoint {
            id: "a".into(),
            target: gatel_plan::Target::Address(address),
            weight: 1,
            priority: 0,
            origin: Origin::synthetic("test"),
        }],
        balance: Default::default(),
        timeouts: Default::default(),
        pool: config,
        retry: None,
        health: None,
        passive_health: None,
        protocol: Default::default(),
        tls: None,
        resolve: Default::default(),
        origin: Origin::synthetic("test"),
    };
    let pool = ConnectionPool::new(config, UpstreamProtocol::Http1, Default::default(), None);
    (EndpointSet::new(&upstream), pool)
}

fn request() -> http::Request<Body> {
    use http_body_util::{BodyExt, Empty};
    http::Request::builder()
        .uri("/")
        .header("host", "backend")
        .body(Empty::<Bytes>::new().map_err(|never| match never {}).boxed())
        .unwrap()
}

#[tokio::test]
async fn draining_an_unused_pool_refuses_its_first_endpoint() {
    let (set, pool) = set("127.0.0.1:9".parse().unwrap(), Default::default());
    pool.drain();
    assert!(matches!(
        pool.checkout(&set.all()[0], &Timeouts::default()).await,
        Err(CheckoutError::Draining)
    ));
    assert!(pool.stats().is_empty());
}

#[tokio::test]
async fn returning_a_live_lease_after_drain_does_not_park_it() {
    let (set, pool) = set(backend().await, Default::default());
    let lease = pool.checkout(&set.all()[0], &Timeouts::default()).await.unwrap();
    pool.drain();
    drop(lease);
    assert_eq!(pool.stats()[0].idle, 0);
    assert_eq!(pool.stats()[0].in_use, 0);
}

#[tokio::test]
async fn withdrawing_all_addresses_prevents_a_live_lease_from_returning() {
    let (set, pool) = set(backend().await, Default::default());
    let lease = pool.checkout(&set.all()[0], &Timeouts::default()).await.unwrap();
    pool.retire("a", &[]);
    drop(lease);
    assert_eq!(pool.stats()[0].idle, 0);
}

#[tokio::test]
async fn dialling_requests_are_not_counted_as_queue_waiters() {
    let (set, pool) = set(backend().await, Default::default());
    let lease = pool.checkout(&set.all()[0], &Timeouts::default()).await.unwrap();
    assert_eq!(pool.stats()[0].waiting, 0);
    assert_eq!(pool.stats()[0].in_use, 1);
    drop(lease);
}

#[tokio::test]
async fn lowering_capacity_does_not_underreport_live_leases() {
    let config = gatel_plan::Pool {
        max_per_endpoint: 2,
        ..Default::default()
    };
    let (set, pool) = set(backend().await, config);
    let first = pool.checkout(&set.all()[0], &Timeouts::default()).await.unwrap();
    let second = pool.checkout(&set.all()[0], &Timeouts::default()).await.unwrap();
    pool.adopt(
        gatel_plan::Pool {
            max_per_endpoint: 1,
            ..config
        },
        Default::default(),
    );
    assert_eq!(pool.stats()[0].in_use, 2);
    drop((first, second));
    assert_eq!(pool.stats()[0].in_use, 0);
}

#[tokio::test]
async fn stalled_tls_checkout_releases_capacity_for_the_next_attempt() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let config = gatel_plan::Pool {
        max_per_endpoint: 1,
        ..Default::default()
    };
    let (endpoints, _) = set(address, config);
    let tls = gatel_tls::ClientTls::new(
        &gatel_plan::UpstreamTls {
            server_name: "localhost".into(),
            ca: None,
            client_cert: None,
            client_key: None,
        },
        UpstreamProtocol::Http1,
        std::path::Path::new("."),
    )
    .unwrap();
    let pool = ConnectionPool::new(
        config,
        UpstreamProtocol::Http1,
        Default::default(),
        Some(Arc::new(tls)),
    );
    let endpoint = Arc::clone(&endpoints.all()[0]);
    let timeouts = Timeouts {
        connect: Duration::from_millis(50),
        ..Default::default()
    };
    // Consume each ClientHello without answering; read EOF when cancellation
    // drops the socket. A second connection proves the sole permit is returned.
    let peer = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut hello = Vec::new();
            stream.read_to_end(&mut hello).await.unwrap();
            assert!(!hello.is_empty());
        }
    });
    for _ in 0..2 {
        let result = tokio::time::timeout(Duration::from_secs(2), pool.checkout(&endpoint, &timeouts))
            .await
            .expect("connect must terminate before the outer test deadline");
        assert!(matches!(result, Err(CheckoutError::ConnectTimeout(_))));
        assert_eq!(pool.stats()[0].idle, 0);
    }
    tokio::time::timeout(Duration::from_secs(2), peer)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn a_connection_is_reused_rather_than_reopened() {
    let address = backend().await;
    let (set, pool) = set(address, gatel_plan::Pool::default());
    let endpoint = Arc::clone(&set.all()[0]);
    let timeouts = Timeouts::default();

    for _ in 0..3 {
        let mut lease = pool.checkout(&endpoint, &timeouts).await.unwrap();
        let response = lease.send(request()).await.unwrap();
        assert_eq!(response.status(), 200);
    }
    let stats = &pool.stats()[0];
    assert_eq!(
        stats.opened, 1,
        "one socket should have served all three requests"
    );
    assert_eq!(stats.reused, 2);
}

#[tokio::test]
async fn capacity_is_bounded_and_waiting_times_out() {
    let address = backend().await;
    let config = gatel_plan::Pool {
        max_per_endpoint: 1,
        ..gatel_plan::Pool::default()
    };
    let (set, pool) = set(address, config);
    let endpoint = Arc::clone(&set.all()[0]);
    let timeouts = Timeouts {
        queue: Duration::from_millis(50),
        ..Timeouts::default()
    };

    let held = pool.checkout(&endpoint, &timeouts).await.unwrap();
    let blocked = pool.checkout(&endpoint, &timeouts).await;
    assert!(
        matches!(blocked, Err(CheckoutError::QueueTimeout(_))),
        "{blocked:?}"
    );

    drop(held);
    assert!(
        pool.checkout(&endpoint, &timeouts).await.is_ok(),
        "the slot must be released"
    );
}

#[tokio::test]
async fn too_many_waiters_are_refused_rather_than_queued_without_limit() {
    let address = backend().await;
    let config = gatel_plan::Pool {
        max_per_endpoint: 1,
        max_waiters: 1,
        ..gatel_plan::Pool::default()
    };
    let (set, pool) = set(address, config);
    let endpoint = Arc::clone(&set.all()[0]);
    let timeouts = Timeouts {
        queue: Duration::from_millis(200),
        ..Timeouts::default()
    };

    let _held = pool.checkout(&endpoint, &timeouts).await.unwrap();
    let first = pool.checkout(&endpoint, &timeouts);
    let second = pool.checkout(&endpoint, &timeouts);
    let (a, b) = tokio::join!(first, second);
    let refused = [a, b]
        .into_iter()
        .filter(|result| matches!(result, Err(CheckoutError::QueueFull { .. })))
        .count();
    assert_eq!(refused, 1, "exactly one of the two waiters is over the limit");
}

#[tokio::test]
async fn connecting_to_a_dead_address_reports_a_pre_send_failure() {
    // Take a port and release it, so nothing is listening on an address
    // that is certainly reachable. Using a documentation address instead
    // would depend on what the local network does with it.
    let address = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    };
    let (set, pool) = set(address, gatel_plan::Pool::default());
    let endpoint = Arc::clone(&set.all()[0]);
    let timeouts = Timeouts {
        connect: Duration::from_millis(200),
        ..Timeouts::default()
    };

    let error = pool.checkout(&endpoint, &timeouts).await.unwrap_err();
    assert!(
        error.is_pre_send(),
        "a failure to connect is always safe to retry elsewhere"
    );
    assert!(
        matches!(
            error,
            CheckoutError::ConnectTimeout(_) | CheckoutError::Connect(_)
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn idle_connections_beyond_the_limit_are_closed() {
    let address = backend().await;
    let config = gatel_plan::Pool {
        max_per_endpoint: 4,
        max_idle_per_endpoint: 1,
        ..gatel_plan::Pool::default()
    };
    let (set, pool) = set(address, config);
    let endpoint = Arc::clone(&set.all()[0]);
    let timeouts = Timeouts::default();

    let mut leases = Vec::new();
    for _ in 0..3 {
        let mut lease = pool.checkout(&endpoint, &timeouts).await.unwrap();
        lease.send(request()).await.unwrap();
        leases.push(lease);
    }
    drop(leases);
    assert_eq!(pool.stats()[0].idle, 1, "only one connection may be parked");
}

#[tokio::test]
async fn a_poisoned_lease_is_not_returned() {
    let address = backend().await;
    let (set, pool) = set(address, gatel_plan::Pool::default());
    let endpoint = Arc::clone(&set.all()[0]);
    let timeouts = Timeouts::default();

    let mut lease = pool.checkout(&endpoint, &timeouts).await.unwrap();
    lease.send(request()).await.unwrap();
    lease.poison();
    drop(lease);
    assert_eq!(pool.stats()[0].idle, 0);
}

#[tokio::test]
async fn draining_closes_idle_connections_and_refuses_new_ones() {
    let address = backend().await;
    let (set, pool) = set(address, gatel_plan::Pool::default());
    let endpoint = Arc::clone(&set.all()[0]);
    let timeouts = Timeouts::default();

    let mut lease = pool.checkout(&endpoint, &timeouts).await.unwrap();
    lease.send(request()).await.unwrap();
    drop(lease);
    assert_eq!(pool.stats()[0].idle, 1);

    pool.drain();
    assert_eq!(pool.stats()[0].idle, 0);
    assert!(matches!(
        pool.checkout(&endpoint, &timeouts).await,
        Err(CheckoutError::Draining)
    ));
}

#[tokio::test]
async fn raising_a_limit_takes_effect_on_a_pool_the_reload_kept() {
    // Sizing is not part of what makes a pool reusable, so a raised limit
    // reaches a pool that already exists. Before `adopt` it reached
    // nothing: the numbers were read once, at creation, and a reload that
    // kept the pool kept the old ones for as long as the process ran.
    let address = backend().await;
    let (set, pool) = set(
        address,
        gatel_plan::Pool {
            max_per_endpoint: 1,
            ..gatel_plan::Pool::default()
        },
    );
    let endpoint = Arc::clone(&set.all()[0]);
    let timeouts = Timeouts {
        queue: Duration::from_millis(100),
        ..Timeouts::default()
    };

    let held = pool.checkout(&endpoint, &timeouts).await.unwrap();
    assert!(
        matches!(
            pool.checkout(&endpoint, &timeouts).await,
            Err(CheckoutError::QueueTimeout(_))
        ),
        "one permit means one connection"
    );

    pool.adopt(
        gatel_plan::Pool {
            max_per_endpoint: 2,
            ..gatel_plan::Pool::default()
        },
        gatel_plan::Resolve::default(),
    );

    let second = pool.checkout(&endpoint, &timeouts).await;
    assert!(second.is_ok(), "{second:?}");
    drop(held);
}

#[tokio::test]
async fn lowering_a_limit_gives_back_what_nobody_is_holding() {
    let address = backend().await;
    let (set, pool) = set(
        address,
        gatel_plan::Pool {
            max_per_endpoint: 4,
            ..gatel_plan::Pool::default()
        },
    );
    let endpoint = Arc::clone(&set.all()[0]);
    let timeouts = Timeouts {
        queue: Duration::from_millis(100),
        ..Timeouts::default()
    };

    let held = pool.checkout(&endpoint, &timeouts).await.unwrap();
    pool.adopt(
        gatel_plan::Pool {
            max_per_endpoint: 1,
            ..gatel_plan::Pool::default()
        },
        gatel_plan::Resolve::default(),
    );

    // The one in flight is not taken away from the request holding it, but
    // nothing new may start beside it.
    assert!(
        matches!(
            pool.checkout(&endpoint, &timeouts).await,
            Err(CheckoutError::QueueTimeout(_))
        ),
        "a lowered limit must bind immediately for anything new"
    );
    drop(held);
}

#[tokio::test]
async fn what_a_pool_reports_in_use_follows_the_limit_it_now_has() {
    let address = backend().await;
    let (set, pool) = set(
        address,
        gatel_plan::Pool {
            max_per_endpoint: 4,
            ..gatel_plan::Pool::default()
        },
    );
    let endpoint = Arc::clone(&set.all()[0]);
    let timeouts = Timeouts::default();

    let held = pool.checkout(&endpoint, &timeouts).await.unwrap();
    assert_eq!(pool.stats()[0].in_use, 1);

    pool.adopt(
        gatel_plan::Pool {
            max_per_endpoint: 8,
            ..gatel_plan::Pool::default()
        },
        gatel_plan::Resolve::default(),
    );
    assert_eq!(
        pool.stats()[0].in_use,
        1,
        "raising the ceiling does not change how many are in use"
    );
    drop(held);
}

#[tokio::test]
async fn a_connection_to_an_address_a_name_no_longer_has_is_dropped() {
    // What happens when a container is replaced: the name still resolves,
    // to somewhere else. A socket to the old address reaches something on
    // its way out, and handing it to the next request is handing it a
    // connection that is about to close under them.
    let address = backend().await;
    let (set, pool) = set(address, gatel_plan::Pool::default());
    let endpoint = Arc::clone(&set.all()[0]);
    let timeouts = Timeouts::default();

    let mut lease = pool.checkout(&endpoint, &timeouts).await.unwrap();
    lease.send(request()).await.unwrap();
    drop(lease);
    assert_eq!(pool.stats()[0].idle, 1);

    let elsewhere: SocketAddr = "127.0.0.1:9".parse().unwrap();
    assert_eq!(pool.retire("a", &[elsewhere]), 1, "the parked one is gone");
    assert_eq!(pool.stats()[0].idle, 0);
}

#[tokio::test]
async fn a_connection_to_an_address_a_name_still_has_is_kept() {
    // Adding one replica must not cost every connection to the others.
    let address = backend().await;
    let (set, pool) = set(address, gatel_plan::Pool::default());
    let endpoint = Arc::clone(&set.all()[0]);
    let timeouts = Timeouts::default();

    let mut lease = pool.checkout(&endpoint, &timeouts).await.unwrap();
    lease.send(request()).await.unwrap();
    drop(lease);

    let added: SocketAddr = "127.0.0.1:9".parse().unwrap();
    assert_eq!(pool.retire("a", &[address, added]), 0);
    assert_eq!(pool.stats()[0].idle, 1, "it is still usable, so it is kept");
}

#[tokio::test]
async fn a_connection_in_flight_when_its_address_goes_is_not_parked_afterwards() {
    // The request holding it finishes — cancelling it would turn a
    // deployment into an error for whoever was mid-request — but the
    // socket is not handed to anybody else.
    let address = backend().await;
    let (set, pool) = set(address, gatel_plan::Pool::default());
    let endpoint = Arc::clone(&set.all()[0]);
    let timeouts = Timeouts::default();

    let mut lease = pool.checkout(&endpoint, &timeouts).await.unwrap();
    let elsewhere: SocketAddr = "127.0.0.1:9".parse().unwrap();
    pool.retire("a", &[elsewhere]);

    // Still works: it was already open and the request is already going.
    lease.send(request()).await.unwrap();
    drop(lease);
    assert_eq!(
        pool.stats()[0].idle,
        0,
        "a connection to somewhere that is gone must not be parked"
    );
}
