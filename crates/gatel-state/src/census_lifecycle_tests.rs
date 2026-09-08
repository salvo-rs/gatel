use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

#[tokio::test(start_paused = true)]
async fn a_panicking_sample_recovers_and_keeps_sampling() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let (stop, receiver) = watch::channel(false);
    let (sent, mut samples) = tokio::sync::mpsc::unbounded_channel();
    let started = tokio::time::Instant::now();
    let handle = tokio::spawn(crate::worker::supervise(
        gatel_observe::workers::WorkerKind::Census,
        String::new(),
        Telemetry::new(),
        receiver,
        move |receiver| {
            let counter = Arc::clone(&counter);
            let sent = sent.clone();
            sample_loop(receiver, move || {
                let attempt = counter.fetch_add(1, Ordering::SeqCst);
                assert_ne!(attempt, 0, "injected sampling failure");
                sent.send(tokio::time::Instant::now()).unwrap();
            })
        },
    ));
    // First tick at 5s panics, restart after 1s, next sample after another 5s.
    assert_eq!(
        samples.recv().await.unwrap().duration_since(started),
        Duration::from_secs(11)
    );
    assert_eq!(
        samples.recv().await.unwrap().duration_since(started),
        Duration::from_secs(16)
    );
    stop.send(true).unwrap();
    handle.await.unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert!(samples.recv().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn stopping_before_a_sample_does_not_wait_for_the_tick() {
    for closed in [false, true] {
        let (sender, receiver) = watch::channel(false);
        let task = tokio::spawn(sample_loop(receiver, || panic!("must not sample after stop")));
        tokio::task::yield_now().await;
        let started = tokio::time::Instant::now();
        if closed {
            drop(sender);
        } else {
            sender.send(true).unwrap();
        }
        task.await.unwrap();
        assert_eq!(started.elapsed(), Duration::ZERO);
    }
}

#[tokio::test]
async fn aborted_census_is_not_live_and_follow_replaces_it() {
    let census = Census::new();
    let runtimes = Arc::new(UpstreamRuntimes::default());
    let telemetry = Telemetry::new();
    census.follow(&runtimes, &telemetry).await;
    let previous = {
        let running = census.running.lock().await;
        let task = running.as_ref().unwrap();
        task.handle.abort();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !task.handle.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.handle.id()
    };
    assert!(!census.is_running().await);
    census.follow(&runtimes, &telemetry).await;
    assert!(census.is_running().await);
    assert_ne!(
        census.running.lock().await.as_ref().unwrap().handle.id(),
        previous
    );
    census.shutdown().await;
    assert!(!census.is_running().await);
    census.follow(&runtimes, &telemetry).await;
    assert!(census.is_running().await, "standalone Census is restartable");
    census.shutdown().await;
}
