use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

struct Dropped(Arc<AtomicUsize>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn policy() -> Resolve {
    Resolve {
        interval: Duration::from_secs(60),
        timeout: Duration::from_secs(60),
        family: Family::Any,
    }
}

#[tokio::test]
async fn stopping_an_inflight_round_drops_it_without_waiting_for_dns_timeout() {
    tokio::time::timeout(Duration::from_secs(2), async {
        for close_sender in [false, true] {
            let (sender, receiver) = watch::channel(false);
            let dropped = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&dropped);
            let (ready, mut started) = tokio::sync::mpsc::unbounded_channel();
            let task = tokio::spawn(async move {
                run_loop(policy(), receiver, || async {
                    let _guard = Dropped(Arc::clone(&counter));
                    ready.send(()).unwrap();
                    std::future::pending::<Outcome>().await
                })
                .await;
            });
            started.recv().await.unwrap();
            if close_sender {
                drop(sender);
            } else {
                sender.send(true).unwrap();
            }
            task.await.unwrap();
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
        }
    })
    .await
    .expect("resolver shutdown must not wait for the 60-second round");
}

#[tokio::test]
async fn a_stopped_or_closed_worker_never_starts_another_round() {
    for initial_stop in [false, true] {
        let (sender, receiver) = watch::channel(initial_stop);
        if !initial_stop {
            drop(sender);
        }
        run_loop(policy(), receiver, || async {
            panic!("a stopped worker must not start DNS work")
        })
        .await;
    }
}

#[tokio::test]
async fn stopping_between_rounds_does_not_wait_for_the_refresh_interval() {
    tokio::time::timeout(Duration::from_secs(2), async {
        let (sender, receiver) = watch::channel(false);
        let (ready, mut started) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            run_loop(policy(), receiver, || async {
                ready.send(()).unwrap();
                Outcome::Settled
            })
            .await;
        });
        started.recv().await.unwrap();
        sender.send(true).unwrap();
        task.await.unwrap();
        assert!(started.recv().await.is_none());
    })
    .await
    .expect("resolver shutdown must interrupt the 60-second refresh delay");
}
