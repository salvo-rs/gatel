//! One instrument group per label set, resolved once and then held.
//!
//! This is where the promise about cardinality is kept. A series is created by
//! [`Series::resolve`], which is only ever called with a name that exists in the
//! plan — never from a request. What the request path holds is an `Arc<T>` with
//! no way back here, so no amount of traffic, however hostile, can add a
//! series.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

/// The label values identifying one series, in the order the family declares.
pub type Labels = Vec<String>;

#[derive(Debug)]
pub struct Series<T> {
    entries: RwLock<Vec<(Labels, Arc<T>)>>,
}

impl<T> Default for Series<T> {
    fn default() -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
        }
    }
}

impl<T: Default> Series<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// The instruments for these labels, creating them the first time.
    ///
    /// Reusing an existing series is what makes counters survive a reload: the
    /// same route in generation four continues the count it had in generation
    /// three, which is what anyone reading a rate over a deploy expects.
    pub fn resolve(&self, labels: Labels) -> Arc<T> {
        {
            let entries = self.entries.read().unwrap_or_else(|p| p.into_inner());
            if let Some((_, existing)) = entries.iter().find(|(known, _)| *known == labels) {
                return Arc::clone(existing);
            }
        }
        let mut entries = self.entries.write().unwrap_or_else(|p| p.into_inner());
        // Another thread may have won the race between the two locks.
        if let Some((_, existing)) = entries.iter().find(|(known, _)| *known == labels) {
            return Arc::clone(existing);
        }
        let created = Arc::new(T::default());
        entries.push((labels, Arc::clone(&created)));
        created
    }

    /// Forget every series whose labels are not in `live`.
    ///
    /// Called after a commit, so a route deleted from the configuration stops
    /// being reported rather than sitting at a value that will never change
    /// again. Returns what was forgotten.
    pub fn retain(&self, live: &HashSet<Labels>) -> Vec<Labels> {
        let mut entries = self.entries.write().unwrap_or_else(|p| p.into_inner());
        let mut forgotten = Vec::new();
        entries.retain(|(labels, _)| {
            let keep = live.contains(labels);
            if !keep {
                forgotten.push(labels.clone());
            }
            keep
        });
        forgotten
    }

    /// Every series, sorted by labels so a scrape is byte-for-byte stable.
    pub fn snapshot(&self) -> Vec<(Labels, Arc<T>)> {
        let mut entries = self
            .entries
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(labels, value)| (labels.clone(), Arc::clone(value)))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        entries
    }

    pub fn len(&self) -> usize {
        self.entries.read().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Counter;

    fn labels(values: &[&str]) -> Labels {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn the_same_labels_resolve_to_the_same_instruments() {
        let series: Series<Counter> = Series::new();
        let first = series.resolve(labels(&["api"]));
        first.add(7);
        let again = series.resolve(labels(&["api"]));
        assert_eq!(again.get(), 7, "a reload continues the count");
        assert_eq!(series.len(), 1);
    }

    #[test]
    fn different_labels_are_different_series() {
        let series: Series<Counter> = Series::new();
        series.resolve(labels(&["api"])).add(1);
        series.resolve(labels(&["web"])).add(2);
        assert_eq!(series.len(), 2);
    }

    #[test]
    fn retaining_forgets_what_configuration_no_longer_names() {
        let series: Series<Counter> = Series::new();
        series.resolve(labels(&["api"]));
        series.resolve(labels(&["web"]));
        let forgotten = series.retain(&HashSet::from([labels(&["api"])]));
        assert_eq!(forgotten, vec![labels(&["web"])]);
        assert_eq!(series.len(), 1);
    }

    #[test]
    fn a_snapshot_is_ordered_so_two_scrapes_agree() {
        let series: Series<Counter> = Series::new();
        series.resolve(labels(&["web"]));
        series.resolve(labels(&["api"]));
        let names: Vec<_> = series.snapshot().into_iter().map(|(labels, _)| labels).collect();
        assert_eq!(names, vec![labels(&["api"]), labels(&["web"])]);
    }
}
