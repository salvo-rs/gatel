use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use super::*;

#[derive(Default)]
struct ConnectionState {
    polls: AtomicUsize,
    closed: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

struct Connection(Arc<ConnectionState>);

impl Future for Connection {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        assert!(
            self.0.polls.fetch_add(1, Ordering::Relaxed) < 100,
            "driver must not busy loop"
        );
        *self.0.waker.lock().unwrap() = Some(cx.waker().clone());
        if self.0.closed.load(Ordering::Acquire) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

async fn settle() {
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
}

fn close(state: &ConnectionState) {
    state.closed.store(true, Ordering::Release);
    state.waker.lock().unwrap().take().unwrap().wake();
}

#[tokio::test(start_paused = true)]
async fn idle_driver_sleeps_demand_observes_and_cancellation_disarms_its_timer() {
    let demand = Arc::new(Demand::default());
    let state = Arc::new(ConnectionState::default());
    let events = Arc::new(tokio::sync::Notify::new());
    let driver = tokio::spawn(demand.observer().drive(Connection(state.clone()), events));
    settle().await;
    let idle_polls = state.polls.load(Ordering::Relaxed);
    tokio::time::advance(Duration::from_secs(30)).await;
    settle().await;
    assert_eq!(
        state.polls.load(Ordering::Relaxed),
        idle_polls,
        "no idle timer wakeups"
    );

    let waiting = demand.wait();
    settle().await;
    let demanded_polls = state.polls.load(Ordering::Relaxed);
    assert!(demanded_polls > idle_polls, "new waiters wake an idle driver");
    tokio::time::advance(Duration::from_millis(49)).await;
    settle().await;
    assert_eq!(state.polls.load(Ordering::Relaxed), demanded_polls);
    tokio::time::advance(Duration::from_millis(1)).await;
    settle().await;
    assert!(
        state.polls.load(Ordering::Relaxed) > demanded_polls,
        "capacity is sampled while demand persists"
    );

    drop(waiting);
    settle().await;
    let cancelled_polls = state.polls.load(Ordering::Relaxed);
    tokio::time::advance(Duration::from_secs(60)).await;
    settle().await;
    assert_eq!(
        state.polls.load(Ordering::Relaxed),
        cancelled_polls,
        "cancelled demand removes the timer"
    );

    close(&state);
    driver.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn initial_and_overlapping_waiters_keep_observation_until_the_last_leaves() {
    let demand = Arc::new(Demand::default());
    let first = demand.wait();
    let second = demand.wait();
    let state = Arc::new(ConnectionState::default());
    let events = Arc::new(tokio::sync::Notify::new());
    let driver = tokio::spawn(demand.observer().drive(Connection(state.clone()), events.clone()));
    settle().await;
    drop(first);
    settle().await;
    let polls = state.polls.load(Ordering::Relaxed);
    tokio::time::advance(Duration::from_millis(50)).await;
    settle().await;
    assert!(state.polls.load(Ordering::Relaxed) > polls);
    drop(second);
    settle().await;
    let polls = state.polls.load(Ordering::Relaxed);
    tokio::time::advance(Duration::from_secs(30)).await;
    settle().await;
    assert_eq!(state.polls.load(Ordering::Relaxed), polls);
    drop(demand);
    settle().await;
    let polls = state.polls.load(Ordering::Relaxed);
    tokio::time::advance(Duration::from_secs(30)).await;
    settle().await;
    assert_eq!(
        state.polls.load(Ordering::Relaxed),
        polls,
        "closed demand must not spin"
    );
    state.closed.store(true, Ordering::Release);
    events.notify_one();
    driver.await.unwrap();
}
