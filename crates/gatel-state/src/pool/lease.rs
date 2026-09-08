//! One borrowed connection, and what letting go of it decides.
//!
//! A lease is the only place that answers "does this socket go back into the
//! pool, or does it close", and it answers with what it knows at that moment:
//! whether the connection is still open, whether it has outlived its
//! configured lifetime, whether the address it reached is one the endpoint
//! still has, and whether the pool already holds as many idle connections as
//! it is allowed. Dropping one is not a formality here — it is the decision.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::OwnedSemaphorePermit;

use super::{Body, EndpointPool, Idle, Sender};

/// A borrowed connection. Returned to the pool on drop unless something made it
/// unusable.
pub struct Lease {
    sender: Option<Sender>,
    address: Option<SocketAddr>,
    opened: Instant,
    slot: Arc<EndpointPool>,
    permit: OwnedSemaphorePermit,
    config: gatel_plan::Pool,
    reusable: bool,
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lease")
            .field("age", &self.opened.elapsed())
            .field("reusable", &self.reusable)
            .finish()
    }
}

impl Lease {
    pub(super) fn new(
        sender: Sender,
        address: Option<SocketAddr>,
        opened: Instant,
        slot: Arc<EndpointPool>,
        permit: OwnedSemaphorePermit,
        config: gatel_plan::Pool,
    ) -> Self {
        Self {
            sender: Some(sender),
            address,
            opened,
            slot,
            permit,
            config,
            reusable: true,
        }
    }

    /// Send a request. Consumes the lease's exclusive access for HTTP/1.1 and
    /// clones the multiplexed sender for HTTP/2.
    pub async fn send(
        &mut self,
        request: http::Request<Body>,
    ) -> Result<http::Response<hyper::body::Incoming>, hyper::Error> {
        let result = match self.sender.as_mut() {
            Some(Sender::Http1(sender)) => sender.send_request(request).await,
            Some(Sender::Http2(sender)) => sender.send_request(request).await,
            None => unreachable!("a lease always holds a sender until it is dropped"),
        };
        if result.is_err() {
            self.reusable = false;
        }
        result
    }

    /// Mark the connection as not reusable — after an upgrade, or when the
    /// response made further requests on it unsafe.
    pub fn poison(&mut self) {
        self.reusable = false;
    }

    /// Whether the underlying connection accepts another request now. HTTP/2
    /// senders are ready as long as the connection is open.
    pub fn is_ready(&self) -> bool {
        match self.sender.as_ref() {
            Some(Sender::Http1(sender)) => sender.is_ready(),
            Some(Sender::Http2(sender)) => !sender.is_closed(),
            None => false,
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let Some(sender) = self.sender.take() else { return };
        let closed = match &sender {
            Sender::Http1(sender) => sender.is_closed(),
            Sender::Http2(sender) => sender.is_closed(),
        };
        let now = Instant::now();
        let expired = now.duration_since(self.opened) >= self.config.max_lifetime;
        if !self.reusable || closed || expired {
            return;
        }
        // A connection to an address the name has since stopped resolving to
        // reached a container that is being replaced. Parking it would hand the
        // next request a socket to something on its way out.
        // Hold the address snapshot until parking completes, so retire cannot
        // remove an address between validation and insertion.
        let live = self.slot.live.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(address) = self.address
            && live
                .as_ref()
                .is_some_and(|addresses| !addresses.contains(&address))
        {
            return;
        }
        let mut idle = self.slot.idle.lock().unwrap_or_else(|p| p.into_inner());
        if self.slot.permits.is_closed() {
            return;
        }
        if idle.len() >= self.config.max_idle_per_endpoint as usize {
            // Keeping more idle sockets than configured wastes file
            // descriptors on the backend as much as here.
            return;
        }
        idle.push_back(Idle {
            sender,
            address: self.address,
            opened: self.opened,
            returned: now,
        });
        // The permit is released after the connection is parked, so a waiter
        // that wakes up immediately finds it.
        let _ = &self.permit;
    }
}
