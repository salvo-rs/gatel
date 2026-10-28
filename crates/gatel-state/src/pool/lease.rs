//! One borrowed connection, and what letting go of it decides.
//!
//! A lease is the only place that answers "does this socket go back into the
//! pool, or does it close", and it answers with what it knows at that moment:
//! whether the connection is still open, whether it has outlived its
//! configured lifetime, whether the address it reached is one the endpoint
//! still has, and whether the pool already holds as many idle connections as
//! it is allowed. Dropping one is not a formality here — it is the decision.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use http_body_util::BodyExt;

use super::{Body, EndpointPool, Idle, Sender};

/// A borrowed connection. Returned to the pool on drop unless something made it
/// unusable.
pub struct Lease {
    exclusive: Option<Exclusive>,
    multiplexed: Option<(super::dial::Http2Sender, Arc<super::multiplex::Stream>)>,
    sent: bool,
}

/// A send failure, including reuse of a single H2 stream reservation.
#[derive(Debug)]
pub enum SendError {
    Connection(hyper::Error),
    AlreadySent,
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connection(error) => error.fmt(f),
            Self::AlreadySent => f.write_str("an HTTP/2 lease permits one request"),
        }
    }
}
impl std::error::Error for SendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Connection(error) => Some(error),
            Self::AlreadySent => None,
        }
    }
}

// An early response can finish before Hyper's independent upload task. Keep
// the reservation until both the response lease and the upload body are gone.
struct Upload {
    body: Body,
    _reservation: Arc<super::multiplex::Stream>,
}

impl hyper::body::Body for Upload {
    type Data = bytes::Bytes;
    type Error = super::BoxError;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.body).poll_frame(cx)
    }
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> hyper::body::SizeHint {
        self.body.size_hint()
    }
}

struct Exclusive {
    sender: Option<Sender>,
    address: Option<SocketAddr>,
    opened: Instant,
    slot: Arc<EndpointPool>,
    permit: super::admission::Permit,
    reusable: bool,
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lease")
            .field(
                "age",
                &self
                    .exclusive
                    .as_ref()
                    .map(|lease| lease.opened.elapsed())
                    .or_else(|| {
                        self.multiplexed
                            .as_ref()
                            .map(|(_, lease)| lease.opened().elapsed())
                    }),
            )
            .finish()
    }
}

impl Lease {
    pub(super) fn new(
        sender: Sender,
        address: Option<SocketAddr>,
        opened: Instant,
        slot: Arc<EndpointPool>,
        permit: super::admission::Permit,
    ) -> Self {
        Self {
            exclusive: Some(Exclusive {
                sender: Some(sender),
                address,
                opened,
                slot,
                permit,
                reusable: true,
            }),
            multiplexed: None,
            sent: false,
        }
    }

    pub(super) fn multiplexed(stream: super::multiplex::Stream) -> Self {
        Self {
            exclusive: None,
            multiplexed: Some((stream.sender(), Arc::new(stream))),
            sent: false,
        }
    }

    /// Send a request. Consumes the lease's exclusive access for HTTP/1.1 and
    /// reserves one exchange for HTTP/2. Obtain a new lease for each H2 request.
    pub async fn send(
        &mut self,
        request: http::Request<Body>,
    ) -> Result<http::Response<hyper::body::Incoming>, SendError> {
        if let Some((sender, stream)) = self.multiplexed.as_mut() {
            if self.sent {
                return Err(SendError::AlreadySent);
            }
            self.sent = true;
            let request = request.map(|body| {
                Upload {
                    body,
                    _reservation: stream.clone(),
                }
                .boxed()
            });
            return sender.send_request(request).await.map_err(SendError::Connection);
        }
        let exclusive = self.exclusive.as_mut().expect("a lease owns its reservation");
        let result = match exclusive.sender.as_mut() {
            Some(Sender::Http1(sender)) => sender.send_request(request).await,
            Some(Sender::Http2(sender)) => sender.send_request(request).await,
            None => unreachable!("a lease always holds a sender until it is dropped"),
        };
        if result.is_err() {
            exclusive.reusable = false;
        }
        result.map_err(SendError::Connection)
    }

    /// Mark the connection as not reusable — after an upgrade, or when the
    /// response made further requests on it unsafe.
    pub fn poison(&mut self) {
        if let Some((_, stream)) = &self.multiplexed {
            stream.poison();
        }
        if let Some(lease) = &mut self.exclusive {
            lease.reusable = false;
        }
    }

    /// Whether the underlying connection accepts another request now. HTTP/2
    /// senders are ready as long as the connection is open.
    pub fn is_ready(&self) -> bool {
        if let Some((sender, _)) = &self.multiplexed {
            return !self.sent && !sender.is_closed();
        }
        match self.exclusive.as_ref().and_then(|lease| lease.sender.as_ref()) {
            Some(Sender::Http1(sender)) => sender.is_ready(),
            Some(Sender::Http2(sender)) => !sender.is_closed(),
            None => false,
        }
    }
}

impl Drop for Exclusive {
    fn drop(&mut self) {
        let Some(sender) = self.sender.take() else { return };
        let closed = match &sender {
            Sender::Http1(sender) => sender.is_closed(),
            Sender::Http2(sender) => sender.is_closed(),
        };
        let now = Instant::now();
        let policy = self.slot.admission.policy();
        let config = policy.config;
        let expired = now.duration_since(self.opened) >= config.max_lifetime;
        if !self.reusable
            || closed
            || expired
            || config.idle_timeout.is_zero()
            || policy.allocated > config.max_per_endpoint
        {
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
        if self.slot.admission.is_closed() {
            return;
        }
        if idle.len() >= config.max_idle_per_endpoint as usize {
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
