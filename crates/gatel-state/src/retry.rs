//! How many of an upstream's in-flight requests may be retries.
//!
//! A retry is the right thing to do for one request and the wrong thing to do
//! for all of them at once. When a backend starts failing, every request in
//! flight fails at roughly the same moment, and a gateway that retries each of
//! them turns a partial outage into twice the load on whatever is left — which
//! is how a recoverable failure becomes an unrecoverable one.
//!
//! So retries are capped as a *fraction of live traffic* rather than as a rate.
//! A fraction needs no tuning when traffic changes, and it degrades in the
//! right direction on its own: the busier the upstream is, the more retries it
//! will absorb, and the moment failures become widespread the budget is already
//! spent on the first few.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Retries allowed regardless of the percentage.
///
/// Without a floor a percentage is useless at low traffic: one live request at
/// twenty percent is a budget of zero, so nothing would ever be retried on a
/// quiet gateway — which is exactly where a retry costs least and helps most.
const FLOOR: u64 = 3;

/// One upstream's live retry count.
///
/// Belongs to a generation rather than to the upstream's long-lived state: it
/// counts what is happening right now, and a number that only means anything
/// for the next few milliseconds has nothing to carry across a reload.
#[derive(Debug, Default)]
pub struct RetryBudget {
    live: AtomicU64,
}

impl RetryBudget {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Take a retry, if this upstream can currently afford one.
    ///
    /// `in_flight` is the upstream's live request count, and `percent` the
    /// share of it that may be retries. `None` means the budget is spent and
    /// the caller must give up rather than trying elsewhere.
    pub fn take(self: &Arc<Self>, in_flight: u64, percent: u32) -> Option<RetryPermit> {
        // Zero is not a small percentage, it is an instruction. An operator who
        // writes it is turning retries off for this upstream, and the floor
        // below must not quietly hand back three anyway.
        if percent == 0 {
            return None;
        }
        let allowed = FLOOR.max(in_flight.saturating_mul(percent as u64) / 100);
        // Claim first and check after, so two requests racing cannot both be
        // told they were the last one that fitted.
        let taken = self.live.fetch_add(1, Ordering::AcqRel) + 1;
        if taken > allowed {
            self.live.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(RetryPermit {
            budget: Arc::clone(self),
        })
    }

    /// Retries in flight right now, for tests and for `explain`.
    pub fn live(&self) -> u64 {
        self.live.load(Ordering::Relaxed)
    }
}

/// A retry that is under way. Gives its place back wherever it is dropped,
/// including when the request it belongs to is cancelled.
#[derive(Debug)]
pub struct RetryPermit {
    budget: Arc<RetryBudget>,
}

impl Drop for RetryPermit {
    fn drop(&mut self) {
        self.budget.live.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quiet_upstream_can_still_retry() {
        // One request in flight at twenty percent is a budget of zero if the
        // arithmetic is taken literally, which would mean `retry` did nothing
        // at all on a gateway that is not busy.
        let budget = RetryBudget::new();
        let permits: Vec<_> = (0..3).filter_map(|_| budget.take(1, 20)).collect();
        assert_eq!(permits.len(), 3);
        assert!(budget.take(1, 20).is_none());
    }

    #[test]
    fn a_busy_upstream_gets_a_proportional_allowance() {
        let budget = RetryBudget::new();
        // Two hundred live requests at ten percent is twenty retries.
        let permits: Vec<_> = (0..20).filter_map(|_| budget.take(200, 10)).collect();
        assert_eq!(permits.len(), 20);
        assert!(budget.take(200, 10).is_none());
    }

    #[test]
    fn a_finished_retry_gives_its_place_back() {
        let budget = RetryBudget::new();
        let permits: Vec<_> = (0..3).filter_map(|_| budget.take(1, 20)).collect();
        assert!(budget.take(1, 20).is_none());
        drop(permits);
        assert_eq!(budget.live(), 0);
        assert!(budget.take(1, 20).is_some());
    }

    #[test]
    fn zero_percent_turns_retries_off_rather_than_rounding_up_to_the_floor() {
        let budget = RetryBudget::new();
        assert!(budget.take(1_000, 0).is_none());
        assert_eq!(budget.live(), 0);
    }

    #[test]
    fn nothing_is_left_claimed_by_a_refusal() {
        // A refused retry that forgot to give its slot back would shrink the
        // budget a little every time, until nothing could retry at all.
        let budget = RetryBudget::new();
        let _held: Vec<_> = (0..3).filter_map(|_| budget.take(0, 50)).collect();
        for _ in 0..100 {
            assert!(budget.take(0, 50).is_none());
        }
        assert_eq!(budget.live(), 3);
    }
}
