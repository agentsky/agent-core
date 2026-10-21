//! [`Throttle`]: which of a warning repeated for one key deserve logging.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

/// Decides which events of a key are worth a warning: the first, then the
/// first after each `interval`.
#[derive(Debug)]
pub(crate) struct Throttle<K> {
    interval: Duration,
    /// For each key warned about within `interval`: when, and how many of
    /// its events have gone without a warning since.
    keys: HashMap<K, (Instant, u64)>,
}

impl<K: Eq + Hash> Throttle<K> {
    pub(crate) fn new(interval: Duration) -> Self {
        Self {
            interval,
            keys: HashMap::new(),
        }
    }

    /// Records an event of `key` at `now`. Returns how many of its events
    /// went without a warning since the last one if this one deserves a
    /// warning, and `None` if it doesn't.
    pub(crate) fn record(&mut self, key: K, now: Instant) -> Option<u64> {
        if let Some((warned, quiet)) = self.keys.get_mut(&key)
            && now.duration_since(*warned) < self.interval
        {
            *quiet += 1;
            return None;
        }
        let quiet = self.keys.remove(&key).map_or(0, |(_, quiet)| quiet);
        self.keys
            .retain(|_, (warned, _)| now.duration_since(*warned) < self.interval);
        self.keys.insert(key, (now, 0));
        Some(quiet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_warns_once_per_interval() {
        let interval = Duration::from_secs(60);
        let mut throttle = Throttle::new(interval);
        let start = Instant::now();
        assert_eq!(throttle.record("a", start), Some(0));
        assert_eq!(throttle.record("a", start + Duration::from_secs(1)), None);
        assert_eq!(throttle.record("a", start + Duration::from_secs(59)), None);
        assert_eq!(
            throttle.record("b", start + Duration::from_secs(2)),
            Some(0)
        );
        assert_eq!(throttle.record("b", start + Duration::from_secs(3)), None);
        assert_eq!(throttle.record("a", start + interval), Some(2));
        assert_eq!(throttle.record("a", start + interval), None);
        assert_eq!(
            throttle.record("b", start + Duration::from_secs(200)),
            Some(1)
        );
        assert_eq!(
            throttle.keys.len(),
            1,
            "keys warned about long ago are dropped"
        );
    }
}
