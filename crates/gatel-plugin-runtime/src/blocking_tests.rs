use super::*;

fn budget(jobs: usize, memory_bytes: u64) -> Budget {
    Budget::new(Limits { jobs, memory_bytes }).unwrap()
}

#[tokio::test]
async fn counters_distinguish_admission_reasons_and_actual_work_outcomes() {
    let owner = budget(1, 100);
    let executor = owner.executor();
    assert_eq!(
        executor.run(101, || panic!("rejected")).await,
        Err(Error::Saturated)
    );
    let held = executor.reserve(10).unwrap();
    assert_eq!(
        executor.run(101, || panic!("rejected")).await,
        Err(Error::Saturated)
    );
    owner.close();
    assert_eq!(executor.run(101, || panic!("rejected")).await, Err(Error::Closed));
    drop(held);
    let counts = executor.snapshot().counters;
    assert_eq!(
        counts,
        Counters {
            admitted: 1,
            rejected_jobs: 1,
            rejected_memory: 1,
            rejected_closed: 1,
            abandoned: 1,
            ..Default::default()
        }
    );
    owner
        .reconfigure(Limits {
            jobs: 2,
            memory_bytes: 1000,
        })
        .unwrap();
    assert_eq!(executor.snapshot().counters, counts);

    let executor = budget(1, 100).executor();
    assert_eq!(executor.run(10, || false).await, Ok(false)); // Completed is not authentication success.
    assert_eq!(
        executor.run(20, || panic!("fixture")).await,
        Err(Error::ExecutionFailed)
    );
    let state = executor.snapshot();
    assert_eq!(state.counters.admitted, 2);
    assert_eq!(state.counters.completed, 1);
    assert_eq!(state.counters.panicked, 1);
    assert_eq!((state.jobs, state.running, state.memory_bytes), (0, 0, 0));
    assert_eq!(executor.metrics().panicked, 1);
}

#[tokio::test]
async fn lifetime_counters_saturate_instead_of_wrapping() {
    let owner = budget(1, 100);
    let executor = owner.executor();
    {
        let mut state = owner.0.lock().unwrap();
        state.counters.admitted = u64::MAX;
        state.counters.completed = u64::MAX;
        state.counters.rejected_memory = u64::MAX;
    }
    executor.run(10, || ()).await.unwrap();
    assert_eq!(executor.run(101, || ()).await, Err(Error::Saturated));
    let counts = executor.snapshot().counters;
    assert_eq!(counts.admitted, u64::MAX);
    assert_eq!(counts.completed, u64::MAX);
    assert_eq!(counts.rejected_memory, u64::MAX);
}

#[test]
fn prepared_limit_changes_are_inert_and_publication_can_roll_back() {
    let owner = budget(3, 100);
    let executor = owner.executor();
    let held = executor.reserve(80).unwrap();
    let next = Limits {
        jobs: 1,
        memory_bytes: 40,
    };
    assert!(owner.prepare(&budget(3, 100).executor(), next).is_err());
    assert!(
        owner
            .prepare(
                &executor,
                Limits {
                    jobs: 0,
                    memory_bytes: 1
                }
            )
            .is_err()
    );
    let change = owner.prepare(&executor, next).unwrap();
    assert_eq!(executor.snapshot().limits.memory_bytes, 100);
    {
        let _publication = change.begin_publication();
        assert!(matches!(
            executor.0.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
    }
    assert_eq!(executor.snapshot().limits.memory_bytes, 100);
    change.begin_publication().publish();
    assert_eq!(executor.snapshot().limits, next);
    assert_eq!(executor.snapshot().jobs, 1);
    assert_eq!(executor.snapshot().memory_bytes, 80);
    assert_eq!(executor.snapshot().running, 0);
    assert_eq!(executor.snapshot().counters.completed, 0);
    drop(held);
    assert_eq!(executor.snapshot().memory_bytes, 0);
}

#[test]
fn atomic_admission_resize_and_close_preserve_real_usage() {
    let owner = budget(3, 100);
    let executor = owner.executor();
    let first = executor.reserve(60).unwrap();
    assert!(matches!(executor.reserve(41), Err(Error::Saturated)));
    assert_eq!(executor.snapshot().jobs, 1); // Failed admission charges neither dimension.
    let second = executor.reserve(40).unwrap();
    owner
        .reconfigure(Limits {
            jobs: 1,
            memory_bytes: 50,
        })
        .unwrap();
    assert_eq!(executor.snapshot().memory_bytes, 100);
    assert!(matches!(executor.reserve(0), Err(Error::Saturated)));
    drop(first);
    assert!(matches!(executor.reserve(0), Err(Error::Saturated)));
    drop(second);
    assert_eq!(executor.snapshot().memory_bytes, 0);
    let last = executor.reserve(50).unwrap();
    owner.close();
    owner
        .reconfigure(Limits {
            jobs: 3,
            memory_bytes: 100,
        })
        .unwrap();
    assert!(matches!(executor.reserve(0), Err(Error::Closed)));
    drop(last);
    assert_eq!(executor.snapshot().jobs, 0);
}

#[test]
fn invalid_limits_and_overflow_do_not_mutate_usage() {
    for limits in [
        Limits {
            jobs: 0,
            memory_bytes: 1,
        },
        Limits {
            jobs: 1025,
            memory_bytes: 1,
        },
        Limits {
            jobs: 1,
            memory_bytes: 0,
        },
    ] {
        assert!(Budget::new(limits).is_err());
        let owner = budget(2, 100);
        let before = owner.executor().snapshot();
        assert!(owner.reconfigure(limits).is_err());
        assert_eq!(owner.executor().snapshot(), before);
    }
    let owner = budget(2, u64::MAX);
    let executor = owner.executor();
    let held = executor.reserve(u64::MAX).unwrap();
    assert!(matches!(executor.reserve(1), Err(Error::Saturated)));
    assert_eq!(executor.snapshot().memory_bytes, u64::MAX);
    drop(held);
    assert_eq!(executor.snapshot().memory_bytes, 0);
    assert!(Arc::ptr_eq(
        &Executor::process_default().0,
        &Executor::process_default().0
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_waiter_keeps_both_reservations_until_work_finishes() {
    let owner = budget(2, 100);
    let executor = owner.executor();
    let worker = executor.clone();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let running = tokio::spawn(async move {
        worker
            .run(80, move || {
                started.send(()).unwrap();
                released.recv().unwrap();
            })
            .await
    });
    ready.await.unwrap();
    running.abort();
    let _ = running.await;
    assert_eq!(executor.snapshot().running, 1);
    assert_eq!(executor.snapshot().counters.completed, 0);
    assert_eq!(executor.snapshot().jobs, 1);
    assert_eq!(executor.snapshot().memory_bytes, 80);
    // There is a free job slot, but not enough memory.
    assert_eq!(
        executor.run(21, || panic!("must not run")).await,
        Err(Error::Saturated)
    );
    owner
        .reconfigure(Limits {
            jobs: 1,
            memory_bytes: 40,
        })
        .unwrap();
    assert_eq!(executor.snapshot().memory_bytes, 80);
    release.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while executor.snapshot().jobs != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(executor.run(40, || 42).await, Ok(42));
    assert_eq!(executor.snapshot().counters.completed, 2);
    assert_eq!(executor.snapshot().counters.panicked, 0);
    assert_eq!(executor.snapshot().memory_bytes, 0);
}

#[tokio::test]
async fn panic_and_rejection_do_not_leak_or_run_work() {
    let owner = budget(1, 100);
    let executor = owner.executor();
    assert_eq!(
        executor.run(100, || panic!("fixture")).await,
        Err(Error::ExecutionFailed)
    );
    assert_eq!(executor.snapshot().jobs, 0);
    assert_eq!(executor.snapshot().memory_bytes, 0);
    owner.close();
    assert_eq!(
        executor.run(0, || panic!("must not run")).await,
        Err(Error::Closed)
    );
}

#[test]
fn concurrent_admission_never_exceeds_either_dimension() {
    let owner = budget(3, 100);
    let executor = owner.executor();
    let admitted = std::sync::atomic::AtomicUsize::new(0);
    let barrier = std::sync::Barrier::new(16);
    std::thread::scope(|scope| {
        for _ in 0..16 {
            let executor = &executor;
            let admitted = &admitted;
            let barrier = &barrier;
            scope.spawn(move || {
                let reservation = executor.reserve(40);
                if reservation.is_ok() {
                    admitted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                barrier.wait(); // Nobody releases before every attempt finishes.
                assert!(executor.snapshot().memory_bytes <= 100);
                drop(reservation);
            });
        }
    });
    assert_eq!(admitted.load(std::sync::atomic::Ordering::Relaxed), 2);
    assert_eq!(executor.snapshot().jobs, 0);
    assert_eq!(executor.snapshot().memory_bytes, 0);
}

#[test]
fn queued_closure_keeps_reservation_after_waiter_is_cancelled() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let (occupied, ready) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            occupied.send(()).unwrap();
            released.recv().unwrap();
        });
        ready.await.unwrap();
        let owner = budget(1, 10);
        let executor = owner.executor();
        let worker = executor.clone();
        let (finished, done) = tokio::sync::oneshot::channel();
        let waiting = tokio::spawn(async move {
            worker
                .run(10, move || {
                    finished.send(()).unwrap();
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while executor.snapshot().jobs == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        waiting.abort();
        let _ = waiting.await;
        assert_eq!(executor.snapshot().memory_bytes, 10);
        assert_eq!(executor.snapshot().running, 0);
        assert_eq!(executor.snapshot().counters.admitted, 1);
        assert_eq!(executor.snapshot().counters.abandoned, 0);
        assert_eq!(executor.snapshot().counters.completed, 0);
        owner.close();
        release.send(()).unwrap();
        blocker.await.unwrap();
        done.await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while executor.snapshot().jobs != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(executor.snapshot().memory_bytes, 0);
        assert_eq!(executor.snapshot().running, 0);
        assert_eq!(executor.snapshot().counters.completed, 1);
        assert_eq!(executor.snapshot().counters.abandoned, 0);
        assert_eq!(executor.snapshot().counters.panicked, 0);
    });
}
