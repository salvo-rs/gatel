//! Circuit breaker pattern for upstream failure tolerance.
//!
//! States:
//! - **Closed** — requests flow normally; failures are counted.
//! - **Open** — all requests are rejected immediately; a timer runs.
//! - **HalfOpen** — a single probe request is allowed through. If it succeeds the breaker closes;
//!   if it fails the breaker opens again.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

/// Per-backend circuit breaker.
pub struct CircuitBreaker {
    /// Current state: 0=Closed, 1=Open, 2=HalfOpen.
    state: AtomicU32,
    /// Consecutive failure count (reset on success).
    failure_count: AtomicU32,
    /// Timestamp (epoch millis) when the breaker was opened.
    opened_at: AtomicU64,
    /// Number of failures before opening the circuit.
    threshold: u32,
    /// How long to stay open before transitioning to half-open.
    cooldown: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

impl CircuitBreaker {
    /// Create a new circuit breaker.
    ///
    /// - `threshold`: number of consecutive failures before opening.
    /// - `cooldown`: time to wait in Open state before allowing a probe.
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            state: AtomicU32::new(0),
            failure_count: AtomicU32::new(0),
            opened_at: AtomicU64::new(0),
            threshold,
            cooldown,
        }
    }

    /// Current state of the breaker.
    pub fn state(&self) -> BreakerState {
        match self.state.load(Ordering::Relaxed) {
            1 => BreakerState::Open,
            2 => BreakerState::HalfOpen,
            _ => BreakerState::Closed,
        }
    }

    /// Check whether a request should be allowed through.
    ///
    /// Returns `true` if the request may proceed, `false` if the circuit is
    /// open and the cooldown has not yet elapsed.
    pub fn allow_request(&self) -> bool {
        match self.state.load(Ordering::Relaxed) {
            0 => true, // Closed
            2 => true, // HalfOpen — allow the probe
            1 => {
                // Open — check if cooldown has elapsed
                let opened = self.opened_at.load(Ordering::Relaxed);
                let now = now_millis();
                if now.saturating_sub(opened) >= self.cooldown.as_millis() as u64 {
                    // Transition to half-open
                    self.state.store(2, Ordering::Relaxed);
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Record a successful request. Resets the breaker to Closed.
    pub fn record_success(&self) {
        self.failure_count.store(0, Ordering::Relaxed);
        self.state.store(0, Ordering::Relaxed);
    }

    /// Record a failed request. May transition the breaker to Open.
    pub fn record_failure(&self) {
        let count = self.failure_count.fetch_add(1, Ordering::Relaxed) + 1;

        if self.state.load(Ordering::Relaxed) == 2 {
            // Was half-open, probe failed — reopen
            self.state.store(1, Ordering::Relaxed);
            self.opened_at.store(now_millis(), Ordering::Relaxed);
            return;
        }

        if count >= self.threshold {
            self.state.store(1, Ordering::Relaxed);
            self.opened_at.store(now_millis(), Ordering::Relaxed);
        }
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_closed() {
        let cb = CircuitBreaker::new(3, Duration::from_secs(5));
        assert_eq!(cb.state(), BreakerState::Closed);
        assert!(cb.allow_request());
    }

    #[test]
    fn opens_after_threshold() {
        let cb = CircuitBreaker::new(3, Duration::from_secs(60));
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), BreakerState::Closed);
        cb.record_failure();
        assert_eq!(cb.state(), BreakerState::Open);
        assert!(!cb.allow_request());
    }

    #[test]
    fn success_resets() {
        let cb = CircuitBreaker::new(3, Duration::from_secs(60));
        cb.record_failure();
        cb.record_failure();
        cb.record_success();
        assert_eq!(cb.state(), BreakerState::Closed);
        // Need 3 more failures to open again
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), BreakerState::Closed);
    }

    #[test]
    fn half_open_after_cooldown() {
        let cb = CircuitBreaker::new(2, Duration::from_millis(1));
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), BreakerState::Open);
        std::thread::sleep(Duration::from_millis(10));
        assert!(cb.allow_request());
        assert_eq!(cb.state(), BreakerState::HalfOpen);
    }

    #[test]
    fn half_open_failure_reopens() {
        let cb = CircuitBreaker::new(2, Duration::from_millis(1));
        cb.record_failure();
        cb.record_failure();
        std::thread::sleep(Duration::from_millis(10));
        cb.allow_request(); // transitions to half-open
        cb.record_failure(); // probe failed
        assert_eq!(cb.state(), BreakerState::Open);
    }

    #[test]
    fn half_open_success_closes() {
        let cb = CircuitBreaker::new(2, Duration::from_millis(1));
        cb.record_failure();
        cb.record_failure();
        std::thread::sleep(Duration::from_millis(10));
        cb.allow_request();
        cb.record_success();
        assert_eq!(cb.state(), BreakerState::Closed);
    }
}
