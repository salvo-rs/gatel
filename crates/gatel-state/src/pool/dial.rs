//! Making a connection that does not exist yet.
//!
//! Everything between "the pool has nothing idle for this endpoint" and "there
//! is a sender to hand out": which addresses to try and in what order, over a
//! socket path or TCP, with or without TLS, speaking HTTP/1.1 or HTTP/2.
//!
//! One deadline covers all of it. `timeouts.connect` bounds resolving the name
//! as well as every address tried, because a budget spent per attempt would
//! multiply by however many replicas somebody happened to deploy.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use gatel_plan::{Timeouts, UpstreamProtocol};
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpStream;

use super::{Body, CheckoutError, ConnectionPool};
use crate::endpoint::Endpoint;

/// A way to put a request on an open connection.
pub enum Sender {
    Http1(http1::SendRequest<Body>),
    /// HTTP/2 multiplexes, so one sender serves many concurrent requests and
    /// is cloned rather than checked out exclusively.
    Http2(http2::SendRequest<Body>),
}

impl ConnectionPool {
    /// Reach the backend, and say which of its addresses answered.
    ///
    /// `timeouts.connect` bounds the whole of this - resolving the name if it
    /// has not been resolved yet, and every address tried - so a name with four
    /// dead addresses cannot take four times as long as one dead address does.
    pub(super) async fn dial(
        &self,
        endpoint: &Endpoint,
        timeouts: &Timeouts,
    ) -> Result<(Sender, Option<SocketAddr>), CheckoutError> {
        // One timer owns the entire operation, including HTTP handshakes and
        // Unix sockets. Per-stage timers below may fail earlier, but can never
        // grant another connect budget after an earlier stage used it up.
        connect_with_budget(timeouts.connect, self.dial_inner(endpoint, timeouts)).await
    }

    async fn dial_inner(
        &self,
        endpoint: &Endpoint,
        timeouts: &Timeouts,
    ) -> Result<(Sender, Option<SocketAddr>), CheckoutError> {
        // A Unix socket is not reached by address: there is nothing to
        // resolve, nothing to rotate, and nothing that can go stale.
        if let Some(path) = endpoint.target.unix_path() {
            let sender = self.dial_unix(path, timeouts.connect).await?;
            return Ok((sender, None));
        }

        let deadline = Instant::now() + timeouts.connect;
        let addresses = self.addresses_for(endpoint, deadline).await?;

        let mut last: Option<CheckoutError> = None;
        for address in addresses {
            let Some(budget) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            match self.dial_one(address, budget).await {
                Ok(sender) => return Ok((sender, Some(address))),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or(CheckoutError::ConnectTimeout(timeouts.connect)))
    }

    /// The addresses to try, resolving on demand when a name has none yet.
    ///
    /// The resolver task fills this in the background, so this path is only
    /// reached in the moments after a start - but it has to exist, or the first
    /// request after a start would fail for a reason that has nothing to do
    /// with the backend.
    async fn addresses_for(
        &self,
        endpoint: &Endpoint,
        deadline: Instant,
    ) -> Result<Vec<SocketAddr>, CheckoutError> {
        let ordered = endpoint.dial_order();
        if !ordered.is_empty() {
            return Ok(ordered);
        }
        let budget = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| CheckoutError::Resolve("no time left to look the name up".to_string()))?;
        let resolve = self.policy().resolve;
        let policy = gatel_plan::Resolve {
            timeout: budget.min(resolve.timeout),
            ..resolve
        };
        match crate::resolve::lookup(&endpoint.target, policy).await {
            Ok(addresses) => {
                endpoint.set_addresses(addresses);
                Ok(endpoint.dial_order())
            }
            Err(error) => Err(CheckoutError::Resolve(error)),
        }
    }

    /// Reach a backend over a Unix domain socket.
    ///
    /// Not behind a `cfg` at the call site: the configuration that names one
    /// is refused at compile time on a platform that has none, so this is the
    /// only place that has to know.
    #[cfg(unix)]
    async fn dial_unix(&self, path: &std::path::Path, budget: Duration) -> Result<Sender, CheckoutError> {
        let stream = match tokio::time::timeout(budget, tokio::net::UnixStream::connect(path)).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                return Err(CheckoutError::Connect(format!("{}: {error}", path.display())));
            }
            Err(_) => return Err(CheckoutError::ConnectTimeout(budget)),
        };
        // No `set_nodelay`: Nagle is a TCP thing, and a Unix socket has none.
        self.secure_and_handshake(stream, budget).await
    }

    #[cfg(not(unix))]
    async fn dial_unix(&self, path: &std::path::Path, _budget: Duration) -> Result<Sender, CheckoutError> {
        Err(CheckoutError::Connect(format!(
            "{}: Unix sockets need a Unix-like operating system",
            path.display()
        )))
    }

    async fn dial_one(&self, address: SocketAddr, budget: Duration) -> Result<Sender, CheckoutError> {
        let stream = match tokio::time::timeout(budget, TcpStream::connect(address)).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => return Err(CheckoutError::Connect(format!("{address}: {error}"))),
            Err(_) => return Err(CheckoutError::ConnectTimeout(budget)),
        };
        // Interactive traffic; batching small writes only adds latency.
        let _ = stream.set_nodelay(true);
        self.secure_and_handshake(stream, budget).await
    }

    /// Wrap a connected socket in TLS when the configuration asked for it, and
    /// speak HTTP over whatever came out.
    ///
    /// The handshake is bounded by the same deadline as the connection it is
    /// part of. A backend that accepts and then says nothing must not hold a
    /// slot for longer than dialling one would have.
    async fn secure_and_handshake<IO>(&self, stream: IO, budget: Duration) -> Result<Sender, CheckoutError>
    where
        IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + Sync + 'static,
    {
        match &self.tls {
            None => self.handshake(TokioIo::new(stream)).await,
            Some(tls) => {
                let secured = match tokio::time::timeout(budget, tls.connect(stream)).await {
                    Ok(Ok(secured)) => secured,
                    Ok(Err(error)) => return Err(CheckoutError::Connect(error)),
                    Err(_) => return Err(CheckoutError::ConnectTimeout(budget)),
                };
                self.handshake(TokioIo::new(secured)).await
            }
        }
    }

    /// The HTTP handshake, over whichever transport got here.
    async fn handshake<IO>(&self, io: TokioIo<IO>) -> Result<Sender, CheckoutError>
    where
        IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        match self.protocol {
            UpstreamProtocol::Http1 => {
                let (sender, connection) = http1::handshake(io)
                    .await
                    .map_err(|error| CheckoutError::Handshake(error.to_string()))?;
                // The driver owns the socket for the connection's lifetime and
                // ends when the sender is dropped and the peer closes.
                tokio::spawn(async move {
                    if let Err(error) = connection.with_upgrades().await {
                        tracing::debug!(target: "gatel::upstream", %error, "connection closed");
                    }
                });
                Ok(Sender::Http1(sender))
            }
            UpstreamProtocol::Http2 => {
                let (sender, connection) = http2::handshake(TokioExecutor::new(), io)
                    .await
                    .map_err(|error| CheckoutError::Handshake(error.to_string()))?;
                tokio::spawn(async move {
                    if let Err(error) = connection.await {
                        tracing::debug!(target: "gatel::upstream", %error, "connection closed");
                    }
                });
                Ok(Sender::Http2(sender))
            }
        }
    }
}

async fn connect_with_budget<T>(
    budget: Duration,
    operation: impl std::future::Future<Output = Result<T, CheckoutError>>,
) -> Result<T, CheckoutError> {
    tokio::time::timeout(budget, operation)
        .await
        .unwrap_or(Err(CheckoutError::ConnectTimeout(budget)))
}

#[cfg(test)]
mod deadline_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn successive_stages_share_one_budget() {
        let budget = Duration::from_secs(10);
        let started = tokio::time::Instant::now();
        let result = connect_with_budget(budget, async {
            tokio::time::sleep(Duration::from_secs(7)).await;
            tokio::time::sleep(Duration::from_secs(7)).await;
            Ok(())
        })
        .await;
        assert!(matches!(result, Err(CheckoutError::ConnectTimeout(d)) if d == budget));
        assert_eq!(started.elapsed(), budget);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_http2_handshake_is_cancelled_and_drops_transport() {
        // H2 must write its preface. A full, unread transport keeps that write
        // pending, unlike H1's locally constructed sender.
        let (client, mut server) = tokio::io::duplex(1);
        let pool = ConnectionPool::new(
            Default::default(),
            UpstreamProtocol::Http2,
            Default::default(),
            None,
        );
        let budget = Duration::from_secs(1);
        let result = connect_with_budget(budget, pool.handshake(TokioIo::new(client))).await;
        assert!(matches!(result, Err(CheckoutError::ConnectTimeout(_))));
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        tokio::time::timeout(budget, server.read_to_end(&mut bytes))
            .await
            .expect("cancelled handshake must close its transport")
            .unwrap();
    }
}
