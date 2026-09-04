//! Tunnelling an upgraded connection.
//!
//! Once a connection has been upgraded it is no longer HTTP, so nothing above
//! this module can see it. Two things still matter and are handled here: the
//! connection leaves the pool for good, and it is bounded — by an idle gap and
//! by a total lifetime — so a forgotten WebSocket cannot hold a slot forever.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gatel_state::InFlight;
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
// Tokio's clock rather than the standard one: this is async timing, and it is
// the clock a test can control.
use tokio::time::Instant;

/// The header tokens that make a request an upgrade request.
///
/// Both are required: `Upgrade` alone is a hint, and `Connection: upgrade`
/// alone names nothing to upgrade to.
pub fn requested_protocol(headers: &http::HeaderMap) -> Option<String> {
    let connection = headers
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    if !connection {
        return None;
    }
    headers
        .get(http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
}

/// Why a tunnel ended, for the log line that closes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// One side closed cleanly.
    Closed,
    /// Nothing was sent either way for longer than allowed.
    Idle,
    /// The tunnel outlived its maximum duration.
    Expired,
    Error,
}

impl Ended {
    pub const fn as_str(self) -> &'static str {
        match self {
            Ended::Closed => "closed",
            Ended::Idle => "idle",
            Ended::Expired => "expired",
            Ended::Error => "error",
        }
    }
}

/// Copy bytes between a client and a backend until one of them stops.
///
/// The in-flight guard is held for the whole tunnel, so a long-lived WebSocket
/// counts as work for `least_conn` rather than disappearing the moment the
/// response head was written.
pub async fn tunnel(
    client: Upgraded,
    backend: Upgraded,
    idle: Duration,
    lifetime: Duration,
    _in_flight: InFlight,
) -> Ended {
    let mut client = TokioIo::new(client);
    let mut backend = TokioIo::new(backend);
    let (mut client_read, mut client_write) = tokio::io::split(&mut client);
    let (mut backend_read, mut backend_write) = tokio::io::split(&mut backend);

    let started = Instant::now();
    let activity = Arc::new(AtomicU64::new(0));

    let to_backend = pump(
        &mut client_read,
        &mut backend_write,
        Arc::clone(&activity),
        started,
    );
    let to_client = pump(
        &mut backend_read,
        &mut client_write,
        Arc::clone(&activity),
        started,
    );
    let watchdog = watchdog(Arc::clone(&activity), started, idle, lifetime);

    tokio::select! {
        // Either direction ending means the exchange is over: a WebSocket peer
        // that closed its half is not going to read the other one.
        result = to_backend => if result.is_ok() { Ended::Closed } else { Ended::Error },
        result = to_client => if result.is_ok() { Ended::Closed } else { Ended::Error },
        reason = watchdog => reason,
    }
}

async fn pump<R, W>(
    reader: &mut R,
    writer: &mut W,
    activity: Arc<AtomicU64>,
    started: Instant,
) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    // Large enough that a video frame is one write, small enough that ten
    // thousand idle tunnels do not cost a gigabyte.
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            let _ = writer.shutdown().await;
            return Ok(());
        }
        writer.write_all(&buffer[..read]).await?;
        activity.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
    }
}

/// Ends the tunnel when it goes quiet or outlives its limit.
async fn watchdog(activity: Arc<AtomicU64>, started: Instant, idle: Duration, lifetime: Duration) -> Ended {
    // Checking more often than the idle limit means the tunnel is cut close to
    // when it should be, not up to a whole limit late.
    let tick = (idle / 4).clamp(Duration::from_millis(100), Duration::from_secs(15));
    loop {
        tokio::time::sleep(tick).await;
        let elapsed = started.elapsed();
        if elapsed >= lifetime {
            return Ended::Expired;
        }
        let last = Duration::from_millis(activity.load(Ordering::Relaxed));
        if elapsed.saturating_sub(last) >= idle {
            return Ended::Idle;
        }
    }
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn both_headers_are_needed_to_be_an_upgrade() {
        assert_eq!(
            requested_protocol(&headers(&[("connection", "Upgrade"), ("upgrade", "websocket")])),
            Some("websocket".to_string())
        );
        assert_eq!(requested_protocol(&headers(&[("upgrade", "websocket")])), None);
        assert_eq!(requested_protocol(&headers(&[("connection", "upgrade")])), None);
        assert_eq!(requested_protocol(&headers(&[])), None);
    }

    #[test]
    fn the_connection_token_is_found_among_others() {
        assert!(
            requested_protocol(&headers(&[
                ("connection", "keep-alive, Upgrade"),
                ("upgrade", "WebSocket"),
            ]))
            .is_some()
        );
    }

    #[test]
    fn the_protocol_is_normalised_so_comparisons_are_stable() {
        assert_eq!(
            requested_protocol(&headers(&[
                ("connection", "upgrade"),
                ("upgrade", "  WebSocket  ")
            ])),
            Some("websocket".to_string())
        );
    }

    #[test]
    fn an_empty_upgrade_token_is_not_an_upgrade() {
        assert_eq!(
            requested_protocol(&headers(&[("connection", "upgrade"), ("upgrade", "")])),
            None
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_watchdog_ends_a_quiet_tunnel() {
        let activity = Arc::new(AtomicU64::new(0));
        let reason = watchdog(
            activity,
            Instant::now(),
            Duration::from_secs(30),
            Duration::from_secs(3600),
        )
        .await;
        assert_eq!(reason, Ended::Idle);
    }

    #[tokio::test(start_paused = true)]
    async fn the_watchdog_ends_a_tunnel_that_outlives_its_limit() {
        let activity = Arc::new(AtomicU64::new(0));
        let started = Instant::now();
        // Keep it looking busy, so only the lifetime can end it.
        let busy = Arc::clone(&activity);
        tokio::spawn(async move {
            loop {
                busy.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });

        let reason = watchdog(
            activity,
            started,
            Duration::from_secs(30),
            Duration::from_secs(60),
        )
        .await;
        assert_eq!(reason, Ended::Expired);
    }

    #[test]
    fn reasons_have_stable_names() {
        assert_eq!(Ended::Closed.as_str(), "closed");
        assert_eq!(Ended::Idle.as_str(), "idle");
        assert_eq!(Ended::Expired.as_str(), "expired");
    }
}
