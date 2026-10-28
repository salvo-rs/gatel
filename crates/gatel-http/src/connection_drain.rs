//! Explicit retirement of connection-pinned generations. A later reload cannot
//! extend an already announced deadline, even if it disables retirement.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use gatel_model::Generation;
use gatel_plan::Committed;
use hyper_util::server::graceful::GracefulConnection;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::connection_work::ConnectionWork;
use crate::prepare::Runtime;
use crate::serve::Store;

type Signal = watch::Sender<Option<Instant>>;

#[derive(Default)]
pub(crate) struct Retirements(Mutex<BTreeMap<Generation, Weak<Signal>>>);

impl Retirements {
    /// Read publication and register under the retirement lock: sync cannot
    /// announce a new generation between these two steps and miss this reader.
    pub fn pin(&self, store: &Store) -> Option<(Arc<Committed<Arc<Runtime>>>, Retirement)> {
        let mut entries = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let committed = store.current()?;
        entries.retain(|_, entry| entry.strong_count() > 0);
        let signal = entries
            .get(&committed.number)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let signal = Arc::new(watch::channel(None).0);
                entries.insert(committed.number, Arc::downgrade(&signal));
                signal
            });
        Some((committed, Retirement(signal)))
    }

    pub fn retire_before(&self, generation: Generation, grace: Option<Duration>) {
        let mut entries = self.0.lock().unwrap_or_else(|p| p.into_inner());
        entries.retain(|number, entry| {
            let Some(signal) = entry.upgrade() else {
                return false;
            };
            if *number < generation
                && let Some(grace) = grace
            {
                let deadline = Instant::now() + grace;
                signal.send_if_modified(|previous| {
                    if previous.is_none() {
                        *previous = Some(deadline);
                        true
                    } else {
                        false
                    }
                });
            }
            true
        });
    }
}

#[derive(Clone)]
pub(crate) struct Retirement(Arc<Signal>);

impl Retirement {
    pub async fn deadline(&self) -> Instant {
        let mut receiver = self.0.subscribe();
        loop {
            if let Some(deadline) = *receiver.borrow_and_update() {
                return deadline;
            }
            if receiver.changed().await.is_err() {
                return Instant::now();
            }
        }
    }
}

/// Gracefully stop accepting HTTP requests/streams when retirement or listener
/// shutdown starts. The connection owner enforces the hard deadline, including
/// TLS/PROXY handshakes and the upgrade handoff after HTTP has finished.
pub(crate) async fn http<C>(
    connection: C,
    mut stop: watch::Receiver<bool>,
    retirement: &Retirement,
    work: &ConnectionWork,
    activity: &crate::connection_activity::Activity,
    idle: Duration,
) -> Result<(), String>
where
    C: GracefulConnection,
    C::Error: std::fmt::Display,
{
    activity.begin_http();
    let close = async {
        if *stop.borrow_and_update() {
            return;
        }
        tokio::select! {
            _ = stop.changed() => {},
            _ = retirement.deadline() => {},
        }
    };
    tokio::pin!(connection, close);
    let result = tokio::select! {
        biased;
        _ = &mut close => {
            connection.as_mut().graceful_shutdown();
            connection.await
        },
        result = &mut connection => result,
        _ = activity.expired(idle) => return Err("HTTP connection idle deadline elapsed".into()),
    };
    result.map_err(|error| error.to_string())?;
    work.finish().await;
    Ok(())
}
