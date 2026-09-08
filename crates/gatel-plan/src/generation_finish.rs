use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, Weak};

use super::tests::*;
use super::*;
use crate::publication::{PublishGuard, PublishRuntime};

struct Runtime {
    store: Weak<GenerationStore<Arc<Runtime>>>,
    lock: Arc<Mutex<()>>,
    finishes: Arc<AtomicUsize>,
}

struct Guard<'a>(MutexGuard<'a, ()>);

impl PublishGuard for Guard<'_> {
    fn published(self) {
        drop(self.0);
    }
}

impl PublishRuntime for Runtime {
    type Guard<'a> = Guard<'a>;
    fn begin_publication(&self) -> Guard<'_> {
        Guard(self.lock.lock().unwrap())
    }
    fn finish_publication(&self) {
        let store = self.store.upgrade().unwrap();
        assert!(store.state.try_write().is_ok(), "generation lock remains held");
        assert!(
            self.lock.try_lock().is_ok(),
            "runtime publication lock remains held"
        );
        // Exercise real reentry rather than only checking lock flags.
        assert!(store.current().is_some());
        let _ = store.status();
        self.finishes.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn finish_runs_after_all_locks_and_only_for_successful_publications() {
    let store = Arc::new(GenerationStore::<Arc<Runtime>>::new());
    let registry = registry();
    let finishes = Arc::new(AtomicUsize::new(0));
    let lock = Arc::new(Mutex::new(()));
    let runtime = || {
        Arc::new(Runtime {
            store: Arc::downgrade(&store),
            lock: lock.clone(),
            finishes: finishes.clone(),
        })
    };
    let first = snapshot(1, "first", 8080);
    assert!(matches!(
        store.attempt(&first, &registry, |_| Ok(runtime())),
        Outcome::Started { .. }
    ));
    assert_eq!(finishes.load(Ordering::SeqCst), 1);
    assert!(matches!(
        store.attempt(&first, &registry, |_| panic!("NoOp")),
        Outcome::NoOp { .. }
    ));
    assert!(matches!(
        store.attempt(&snapshot(2, "failed", 8080), &registry, |_| Err("fixture".into())),
        Outcome::PrepareFailed(_)
    ));
    assert_eq!(finishes.load(Ordering::SeqCst), 1);
    let candidate = snapshot(3, "candidate", 8080);
    let outcome = store.attempt(&candidate, &registry, |_| {
        let loser = runtime();
        let winner = store.attempt(&snapshot(4, "winner", 8080), &registry, |_| Ok(runtime()));
        assert!(matches!(winner, Outcome::Reloaded { .. }));
        Ok(loser)
    });
    assert!(matches!(outcome, Outcome::Stale(_)));
    assert_eq!(
        finishes.load(Ordering::SeqCst),
        2,
        "stale candidate must not finish"
    );
}

#[test]
fn a_later_generation_can_publish_while_an_earlier_finish_is_blocked() {
    use std::sync::mpsc;
    struct Deferred {
        name: &'static str,
        gate: Option<(mpsc::SyncSender<()>, Mutex<mpsc::Receiver<()>>)>,
    }
    impl PublishRuntime for Deferred {
        type Guard<'a> = ();
        fn begin_publication(&self) {}
        fn finish_publication(&self) {
            if let Some((entered, release)) = &self.gate {
                entered.send(()).unwrap();
                release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
            }
        }
    }
    let store = Arc::new(GenerationStore::<Arc<Deferred>>::new());
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let old_store = store.clone();
    let old = std::thread::spawn(move || {
        old_store.attempt(&snapshot(1, "old", 8080), &registry(), |_| {
            Ok(Arc::new(Deferred {
                name: "old",
                gate: Some((entered_tx, Mutex::new(release_rx))),
            }))
        })
    });
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first finish entered");
    // Cleanup is paused, but the committed generation must already be visible.
    let visible = store
        .state
        .try_read()
        .ok()
        .and_then(|state| state.current.as_ref().map(|current| current.runtime.name));
    let (completed_tx, completed_rx) = mpsc::sync_channel(1);
    let next_store = store.clone();
    let next = std::thread::spawn(move || {
        let result = next_store.attempt(&snapshot(2, "next", 8080), &registry(), |_| {
            Ok(Arc::new(Deferred {
                name: "next",
                gate: None,
            }))
        });
        completed_tx.send(result).unwrap();
    });
    let result = completed_rx.recv_timeout(Duration::from_secs(2));
    // Release before asserting so a regression does not strand the old thread.
    release_tx.send(()).unwrap();
    let old_result = old.join().unwrap();
    next.join().unwrap();
    assert_eq!(
        visible,
        Some("old"),
        "publication must be readable during cleanup"
    );
    assert!(matches!(
        result.expect("later commit must not wait for old cleanup"),
        Outcome::Reloaded { .. }
    ));
    assert!(matches!(old_result, Outcome::Started { .. }));
    assert_eq!(store.current().unwrap().runtime.name, "next");
}
