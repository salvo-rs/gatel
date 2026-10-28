//! Physical H2 connections and request stream reservations have separate bounds.
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use gatel_plan::Timeouts;

use super::dial::Http2Sender;
use super::{CheckoutError, ConnectionPool, EndpointPool, Lease, Sender};
use crate::endpoint::Endpoint;

pub(super) struct Connection {
    pub sender: Http2Sender,
    pub address: Option<SocketAddr>,
    pub opened: Instant,
    retired: AtomicBool,
}

struct Entry {
    connection: Arc<Connection>,
    streams: u32,
    idle_since: Instant,
    used: bool,
}

pub(super) struct Multiplex(Mutex<State>);
struct State {
    config: gatel_plan::Pool,
    connections: Vec<Entry>,
    connecting: u32,
    streams: u32,
    live: Option<Vec<SocketAddr>>,
    closed: bool,
}

enum Next {
    Lease(Stream),
    Dial(Dial),
    Wait,
    Closed,
}

impl Multiplex {
    pub fn new(config: gatel_plan::Pool, closed: bool) -> Self {
        Self(Mutex::new(State {
            config,
            connections: Vec::new(),
            connecting: 0,
            streams: 0,
            live: None,
            closed,
        }))
    }

    fn next(slot: &Arc<EndpointPool>) -> Next {
        let mut state = slot.h2.0.lock().unwrap_or_else(|p| p.into_inner());
        state.prune(slot, false);
        if state.closed {
            return Next::Closed;
        }
        if state.streams >= state.config.max_h2_streams_per_endpoint {
            return Next::Wait;
        }
        let per_connection = state.config.max_h2_streams_per_connection as usize;
        let available = state.connections.iter().position(|entry| {
            !entry.connection.retired.load(Ordering::Acquire)
                && !entry.connection.sender.is_closed()
                && (entry.streams as usize) < per_connection.min(entry.connection.sender.stream_limit())
        });
        if let Some(index) = available {
            let entry = &mut state.connections[index];
            entry.streams += 1;
            if entry.used {
                slot.counters.reused.fetch_add(1, Ordering::Relaxed);
            }
            entry.used = true;
            let connection = entry.connection.clone();
            state.streams += 1;
            return Next::Lease(Stream {
                slot: slot.clone(),
                connection,
            });
        }
        if state.connections.len() + (state.connecting as usize) < state.config.max_per_endpoint as usize {
            state.connecting += 1;
            Next::Dial(Dial {
                slot: slot.clone(),
                armed: true,
            })
        } else {
            Next::Wait
        }
    }

    pub fn adopt(&self, config: gatel_plan::Pool, slot: &EndpointPool) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.config = config;
        state.prune(slot, true);
        slot.changed.notify_waiters();
    }

    pub fn stats(&self) -> (usize, u32, u32) {
        let state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let idle = state
            .connections
            .iter()
            .filter(|entry| entry.streams == 0)
            .count();
        (
            idle,
            (state.connections.len() - idle) as u32 + state.connecting,
            state.streams,
        )
    }

    pub fn retire(&self, keep: &[SocketAddr], slot: &EndpointPool) -> usize {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.live = Some(keep.to_vec());
        let before = state.connections.len();
        state.prune(slot, true);
        slot.changed.notify_waiters();
        before - state.connections.len()
    }

    pub fn drain(&self, slot: &EndpointPool) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.closed = true;
        state.prune(slot, true);
        slot.changed.notify_waiters();
    }
}

impl State {
    fn prune(&mut self, slot: &EndpointPool, trim_idle: bool) {
        let now = Instant::now();
        let no_waiters = slot.waiting.load(Ordering::Acquire) == 0;
        let mut idle_kept = 0;
        let before = self.connections.len();
        for (index, entry) in self.connections.iter().enumerate() {
            let revoked = entry
                .connection
                .address
                .is_some_and(|address| self.live.as_ref().is_some_and(|live| !live.contains(&address)));
            if self.closed
                || revoked
                || index >= self.config.max_per_endpoint as usize
                || ((entry.used || (trim_idle && no_waiters))
                    && now.duration_since(entry.connection.opened) >= self.config.max_lifetime)
                || entry.connection.sender.is_closed()
                || ((entry.used || (trim_idle && no_waiters))
                    && entry.streams == 0
                    && now.duration_since(entry.idle_since) >= self.config.idle_timeout)
            {
                entry.connection.retired.store(true, Ordering::Release);
            }
        }
        self.connections.retain(|entry| {
            if entry.streams != 0 {
                return true;
            }
            if entry.connection.retired.load(Ordering::Acquire) {
                return false;
            }
            if !entry.used && !no_waiters {
                return true;
            }
            idle_kept += 1;
            !trim_idle || idle_kept <= self.config.max_idle_per_endpoint
        });
        slot.counters
            .discarded
            .fetch_add((before - self.connections.len()) as u64, Ordering::Relaxed);
    }
}

/// A reservation includes the response body lifetime, not just its headers.
pub(super) struct Stream {
    slot: Arc<EndpointPool>,
    connection: Arc<Connection>,
}

impl Stream {
    pub fn sender(&self) -> Http2Sender {
        self.connection.sender.clone()
    }
    pub fn opened(&self) -> Instant {
        self.connection.opened
    }
    pub fn poison(&self) {
        // A reset or per-request timeout only cancels its stream. Hyper owns
        // connection-level failure/GOAWAY; do not evict healthy sibling streams.
        if self.connection.sender.is_closed() {
            self.connection.retired.store(true, Ordering::Release);
            self.slot.changed.notify_waiters();
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let mut state = self.slot.h2.0.lock().unwrap_or_else(|p| p.into_inner());
        let entry = state
            .connections
            .iter_mut()
            .find(|entry| Arc::ptr_eq(&entry.connection, &self.connection))
            .expect("an active stream keeps its physical connection registered");
        entry.streams -= 1;
        if entry.streams == 0 {
            entry.idle_since = Instant::now();
        }
        state.streams -= 1;
        state.prune(&self.slot, true);
        self.slot.changed.notify_waiters();
    }
}

/// Dial reservations are cancellation safe even before a sender exists.
struct Dial {
    slot: Arc<EndpointPool>,
    armed: bool,
}
impl Dial {
    fn finish(mut self, sender: Http2Sender, address: Option<SocketAddr>) {
        let mut state = self.slot.h2.0.lock().unwrap_or_else(|p| p.into_inner());
        state.connecting -= 1;
        self.armed = false;
        let revoked =
            address.is_some_and(|address| state.live.as_ref().is_some_and(|live| !live.contains(&address)));
        if !state.closed && !revoked && state.connections.len() < state.config.max_per_endpoint as usize {
            state.connections.push(Entry {
                connection: Arc::new(Connection {
                    sender,
                    address,
                    opened: Instant::now(),
                    retired: AtomicBool::new(false),
                }),
                streams: 0,
                idle_since: Instant::now(),
                used: false,
            });
        } else {
            self.slot.counters.discarded.fetch_add(1, Ordering::Relaxed);
        }
        self.slot.changed.notify_waiters();
    }
}
impl Drop for Dial {
    fn drop(&mut self) {
        if self.armed {
            self.slot
                .h2
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .connecting -= 1;
            self.slot.changed.notify_waiters();
        }
    }
}

impl ConnectionPool {
    pub(super) async fn checkout_h2(
        &self,
        endpoint: &Arc<Endpoint>,
        slot: Arc<EndpointPool>,
        timeouts: &Timeouts,
    ) -> Result<Lease, CheckoutError> {
        let mut queue_deadline = tokio::time::Instant::now() + timeouts.queue;
        let mut connect_budget = timeouts.connect;
        let mut dials = 0;
        let max_dials = self.policy().config.max_per_endpoint;
        loop {
            let notified = self.h2_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match Multiplex::next(&slot) {
                Next::Lease(stream) => return Ok(Lease::multiplexed(stream)),
                Next::Closed => return Err(CheckoutError::Draining),
                Next::Dial(dial) => {
                    if dials >= max_dials {
                        return Err(CheckoutError::Handshake(
                            "HTTP/2 connection closed before admission".into(),
                        ));
                    }
                    if connect_budget.is_zero() {
                        return Err(CheckoutError::ConnectTimeout(timeouts.connect));
                    }
                    dials += 1;
                    let started = tokio::time::Instant::now();
                    let result = self
                        .dial(
                            endpoint,
                            &Timeouts {
                                connect: connect_budget,
                                ..*timeouts
                            },
                        )
                        .await;
                    let elapsed = started.elapsed();
                    connect_budget = connect_budget.saturating_sub(elapsed);
                    queue_deadline += elapsed;
                    let (sender, address) = result?;
                    let Sender::Http2(sender) = sender else {
                        unreachable!("H2 pool dials H2")
                    };
                    slot.counters.opened.fetch_add(1, Ordering::Relaxed);
                    dial.finish(sender, address);
                }
                Next::Wait => {
                    // Do not leave every idle H2 driver on a periodic timer.
                    // A queued checkout owns observation demand until this
                    // wait completes, times out or is cancelled.
                    let _demand = self.h2_demand.wait();
                    if tokio::time::Instant::now() >= queue_deadline {
                        return Err(CheckoutError::QueueTimeout(timeouts.queue));
                    }
                    if tokio::time::timeout_at(queue_deadline, notified).await.is_err() {
                        return Err(CheckoutError::QueueTimeout(timeouts.queue));
                    }
                }
            }
        }
    }
}

impl Multiplex {
    pub fn finish_wait(&self, slot: &EndpointPool) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).prune(slot, true);
        slot.changed.notify_waiters();
    }
}

#[cfg(test)]
#[path = "multiplex_tests.rs"]
mod tests;
