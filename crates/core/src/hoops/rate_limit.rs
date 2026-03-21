use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use salvo::http::StatusCode;
use salvo::{Depot, FlowCtrl, Request, Response, async_trait};
use tracing::debug;

/// Per-IP rate limiting middleware using a token bucket algorithm.
///
/// Each client IP gets a bucket that starts at `burst` tokens and is
/// replenished at a rate of `max_tokens / window` per second, capped at
/// `burst`. If a request arrives and the bucket is empty, a 429 Too Many
/// Requests response is returned.
///
/// `burst` defaults to `max_tokens` when not specified, which matches the
/// previous behaviour.
pub struct RateLimitHoop {
    buckets: Arc<DashMap<IpAddr, TokenBucket>>,
    max_tokens: u64,
    burst: u64,
    window: Duration,
}

struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimitHoop {
    /// Create a new rate limiter.
    ///
    /// - `window`       — the time window over which `max_requests` applies.
    /// - `max_requests` — steady-state refill rate (tokens per window).
    /// - `burst`        — maximum token bucket capacity; `None` defaults to `max_requests`
    ///   (pre-existing behaviour).
    pub fn new(window: Duration, max_requests: u64, burst: Option<u64>) -> Self {
        let burst = burst.unwrap_or(max_requests);
        let buckets = Arc::new(DashMap::new());

        // Spawn a background task to clean up expired entries periodically.
        let cleanup_buckets = Arc::clone(&buckets);
        let cleanup_window = window;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(cleanup_window.max(Duration::from_secs(60)));
            loop {
                interval.tick().await;
                cleanup_expired(&cleanup_buckets, cleanup_window);
            }
        });

        Self {
            buckets,
            max_tokens: max_requests,
            burst,
            window,
        }
    }
}

#[async_trait]
impl salvo::Handler for RateLimitHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let ip = super::client_addr(req).ip();
        let now = Instant::now();
        let refill_rate = self.max_tokens as f64 / self.window.as_secs_f64();

        let allowed = {
            let mut bucket = self.buckets.entry(ip).or_insert_with(|| TokenBucket {
                tokens: self.burst as f64,
                last_refill: now,
            });

            // Refill tokens based on elapsed time, capped at burst capacity.
            let elapsed = now.duration_since(bucket.last_refill);
            bucket.tokens += elapsed.as_secs_f64() * refill_rate;
            if bucket.tokens > self.burst as f64 {
                bucket.tokens = self.burst as f64;
            }
            bucket.last_refill = now;

            // Try to consume one token.
            if bucket.tokens >= 1.0 {
                bucket.tokens -= 1.0;
                true
            } else {
                false
            }
        };

        if !allowed {
            debug!(client_ip = %ip, "rate limit exceeded, returning 429");
            let retry_after = (1.0 / refill_rate).ceil() as u64;
            res.status_code(StatusCode::TOO_MANY_REQUESTS);
            let _ = res.add_header("Retry-After", retry_after, true);
            res.body("Too Many Requests");
            ctrl.skip_rest();
            return;
        }

        ctrl.call_next(req, depot, res).await;
    }
}

/// Remove entries that have been idle for longer than the window.
fn cleanup_expired(buckets: &DashMap<IpAddr, TokenBucket>, window: Duration) {
    let now = Instant::now();
    // Retain only entries that have been active recently.
    // An entry is considered expired if it hasn't been refilled in 2× the window.
    let expiry = window * 2;
    buckets.retain(|_ip, bucket| now.duration_since(bucket.last_refill) < expiry);
    debug!(remaining = buckets.len(), "rate limiter cleanup complete");
}
