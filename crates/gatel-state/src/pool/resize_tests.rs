use super::tests::{backend, set};
use super::*;

#[tokio::test]
async fn saturated_h1_shrink_gates_new_work_until_old_leases_drain() {
    let config = gatel_plan::Pool {
        max_per_endpoint: 4,
        max_idle_per_endpoint: 4,
        ..Default::default()
    };
    let (endpoints, pool) = set(backend().await, config);
    let endpoint = &endpoints.all()[0];
    let timeouts = Timeouts {
        queue: Duration::from_millis(30),
        ..Default::default()
    };
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(pool.checkout(endpoint, &timeouts).await.unwrap());
    }
    pool.adopt(
        gatel_plan::Pool {
            max_per_endpoint: 1,
            max_idle_per_endpoint: 1,
            ..config
        },
        Default::default(),
    );
    for _ in 0..3 {
        drop(held.pop());
    }
    assert_eq!(pool.stats()[0].in_use, 1);
    assert!(matches!(
        pool.checkout(endpoint, &timeouts).await,
        Err(CheckoutError::QueueTimeout(_))
    ));
    drop(held);
    let only = pool.checkout(endpoint, &timeouts).await.unwrap();
    assert!(matches!(
        pool.checkout(endpoint, &timeouts).await,
        Err(CheckoutError::QueueTimeout(_))
    ));
    drop(only);
    pool.adopt(config, Default::default());
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(pool.checkout(endpoint, &timeouts).await.unwrap());
    }
    assert!(matches!(
        pool.checkout(endpoint, &timeouts).await,
        Err(CheckoutError::QueueTimeout(_))
    ));
}

#[tokio::test]
async fn h1_idle_resize_reclaims_parked_and_returning_old_leases() {
    let config = gatel_plan::Pool {
        max_per_endpoint: 4,
        max_idle_per_endpoint: 4,
        ..Default::default()
    };
    let (endpoints, pool) = set(backend().await, config);
    let endpoint = &endpoints.all()[0];
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(pool.checkout(endpoint, &Timeouts::default()).await.unwrap());
    }
    drop(held.pop());
    assert_eq!(pool.stats()[0].idle, 1);
    pool.adopt(
        gatel_plan::Pool {
            max_idle_per_endpoint: 0,
            ..config
        },
        Default::default(),
    );
    assert_eq!(
        pool.stats()[0].idle,
        0,
        "publication reclaims already parked sockets"
    );
    drop(held);
    assert_eq!(
        pool.stats()[0].idle,
        0,
        "old leases return under the published policy"
    );
    assert_eq!(pool.stats()[0].in_use, 0);
    pool.adopt(config, Default::default());
    let lease = pool.checkout(endpoint, &Timeouts::default()).await.unwrap();
    drop(lease);
    assert_eq!(pool.stats()[0].idle, 1, "raising the idle limit restores reuse");
}

#[tokio::test]
async fn h1_lifetime_and_idle_deadlines_apply_to_existing_connections() {
    let config = gatel_plan::Pool {
        max_per_endpoint: 2,
        max_idle_per_endpoint: 2,
        ..Default::default()
    };
    let (endpoints, pool) = set(backend().await, config);
    let endpoint = &endpoints.all()[0];
    let held = pool.checkout(endpoint, &Timeouts::default()).await.unwrap();
    let idle = pool.checkout(endpoint, &Timeouts::default()).await.unwrap();
    drop(idle);
    pool.adopt(
        gatel_plan::Pool {
            max_lifetime: Duration::ZERO,
            ..config
        },
        Default::default(),
    );
    assert_eq!(pool.stats()[0].idle, 0);
    drop(held);
    assert_eq!(pool.stats()[0].idle, 0);
    pool.adopt(
        gatel_plan::Pool {
            idle_timeout: Duration::ZERO,
            ..config
        },
        Default::default(),
    );
    let held = pool.checkout(endpoint, &Timeouts::default()).await.unwrap();
    drop(held);
    assert_eq!(pool.stats()[0].idle, 0);
}

#[tokio::test]
async fn failed_h1_dials_return_the_resized_capacity() {
    let address = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    };
    let config = gatel_plan::Pool {
        max_per_endpoint: 4,
        ..Default::default()
    };
    let (endpoints, pool) = set(address, config);
    let endpoint = &endpoints.all()[0];
    pool.adopt(
        gatel_plan::Pool {
            max_per_endpoint: 1,
            ..config
        },
        Default::default(),
    );
    for _ in 0..3 {
        assert!(matches!(
            pool.checkout(endpoint, &Timeouts::default()).await,
            Err(CheckoutError::Connect(_))
        ));
        assert_eq!(pool.stats()[0].in_use, 0);
    }
}
