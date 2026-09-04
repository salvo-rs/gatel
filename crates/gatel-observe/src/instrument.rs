//! The three things a metric can be.
//!
//! All of them are lock-free and cheap enough to touch on the request path,
//! because that is the only place they are ever written.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// A number that only goes up.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub fn increment(&self) {
        self.add(1);
    }

    pub fn add(&self, amount: u64) {
        // Relaxed is right: a counter has no ordering relationship with
        // anything else, and a scrape reading a value one increment behind is
        // indistinguishable from scraping a moment earlier.
        self.0.fetch_add(amount, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// A number that goes both ways.
#[derive(Debug, Default)]
pub struct Gauge(AtomicI64);

impl Gauge {
    pub const fn new() -> Self {
        Self(AtomicI64::new(0))
    }

    pub fn set(&self, value: i64) {
        self.0.store(value, Ordering::Relaxed);
    }

    pub fn increment(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub fn decrement(&self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn get(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Latency, in buckets fixed at compile time.
///
/// The bounds are not configurable. A histogram whose buckets differ between
/// two processes cannot be aggregated across them, and a gateway is deployed in
/// more than one copy — so the choice is made once, here, and covers what a
/// proxy actually sees: sub-millisecond cache hits through to a minute-long
/// upload.
#[derive(Debug)]
pub struct Histogram {
    /// Cumulative counts, one per bound plus the implicit `+Inf`.
    buckets: [AtomicU64; BOUNDS.len() + 1],
    sum_micros: AtomicU64,
}

/// Bucket upper bounds in seconds.
pub const BOUNDS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    pub fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            sum_micros: AtomicU64::new(0),
        }
    }

    /// Record one observation.
    pub fn record(&self, elapsed: std::time::Duration) {
        let micros = elapsed.as_micros().min(u64::MAX as u128) as u64;
        self.sum_micros.fetch_add(micros, Ordering::Relaxed);
        let seconds = elapsed.as_secs_f64();
        // Only the bucket the observation falls in is incremented; the
        // cumulative shape Prometheus wants is built when rendering. Otherwise
        // every observation would touch up to thirteen atomics.
        let index = BOUNDS
            .iter()
            .position(|bound| seconds <= *bound)
            .unwrap_or(BOUNDS.len());
        self.buckets[index].fetch_add(1, Ordering::Relaxed);
    }

    /// Cumulative counts, one per bound, ending with the total.
    pub fn cumulative(&self) -> Vec<u64> {
        let mut running = 0;
        self.buckets
            .iter()
            .map(|bucket| {
                running += bucket.load(Ordering::Relaxed);
                running
            })
            .collect()
    }

    pub fn count(&self) -> u64 {
        self.buckets
            .iter()
            .map(|bucket| bucket.load(Ordering::Relaxed))
            .sum()
    }

    pub fn sum_seconds(&self) -> f64 {
        self.sum_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_counter_only_goes_up() {
        let counter = Counter::new();
        counter.increment();
        counter.add(41);
        assert_eq!(counter.get(), 42);
    }

    #[test]
    fn a_gauge_goes_both_ways() {
        let gauge = Gauge::new();
        gauge.increment();
        gauge.increment();
        gauge.decrement();
        assert_eq!(gauge.get(), 1);
        gauge.set(-3);
        assert_eq!(gauge.get(), -3);
    }

    #[test]
    fn an_observation_lands_in_the_first_bucket_it_fits() {
        let histogram = Histogram::new();
        histogram.record(Duration::from_millis(30));
        let cumulative = histogram.cumulative();
        // 0.03s is above the 0.025 bound and at or below 0.05.
        assert_eq!(cumulative[3], 0, "not counted at 0.025");
        assert_eq!(cumulative[4], 1, "counted from 0.05 onward");
        assert_eq!(*cumulative.last().unwrap(), 1);
    }

    #[test]
    fn counts_are_cumulative_so_a_later_bucket_includes_an_earlier_one() {
        let histogram = Histogram::new();
        histogram.record(Duration::from_micros(500));
        histogram.record(Duration::from_secs(3));
        let cumulative = histogram.cumulative();
        assert_eq!(cumulative[0], 1);
        assert_eq!(cumulative[9], 1, "3s is above the 2.5s bound");
        assert_eq!(cumulative[10], 2, "and at or below the 5s one");
        assert_eq!(histogram.count(), 2);
    }

    #[test]
    fn an_observation_past_the_last_bound_is_still_counted() {
        let histogram = Histogram::new();
        histogram.record(Duration::from_secs(60));
        let cumulative = histogram.cumulative();
        assert_eq!(cumulative[BOUNDS.len() - 1], 0, "not in any bounded bucket");
        assert_eq!(*cumulative.last().unwrap(), 1, "but in +Inf");
        assert_eq!(histogram.sum_seconds(), 60.0);
    }
}
