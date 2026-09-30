//! [`Throttle`]: how often a log line anyone can repeat is let through.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The most keys a [`Throttle`] remembers at once.
pub const MAX_THROTTLE_KEYS: usize = 4096;

/// Lets through one event per interval for each key, and counts the rest.
///
/// A key is what one source of events can't hide another's behind, such as
/// the binding or agent a warning is about. At most [`MAX_THROTTLE_KEYS`]
/// are remembered: when that many are, those let through more than an
/// interval ago are forgotten, and while that many are still recent a new
/// key's events stay quiet.
#[derive(Debug)]
pub struct Throttle<K = ()> {
    interval: Duration,
    last: Mutex<HashMap<K, (Instant, u64)>>,
}

impl<K: Eq + Hash> Throttle<K> {
    /// Lets one event per key through every `interval`.
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: Mutex::new(HashMap::new()),
        }
    }

    /// Records an event for `key` at `now`. Returns how many of the key's
    /// events went quiet since the last one let through if this one is let
    /// through, and `None` if it should stay quiet.
    pub fn record(&self, key: K, now: Instant) -> Option<u64> {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((through, quiet)) = last.get_mut(&key) {
            if now.saturating_duration_since(*through) < self.interval {
                *quiet += 1;
                return None;
            }
            *through = now;
            return Some(std::mem::take(quiet));
        }
        if last.len() >= MAX_THROTTLE_KEYS {
            last.retain(|_, (through, _)| now.saturating_duration_since(*through) < self.interval);
            if last.len() >= MAX_THROTTLE_KEYS {
                return None;
            }
        }
        last.insert(key, (now, 0));
        Some(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_event_per_interval_is_let_through_and_the_rest_counted() {
        let throttle = Throttle::new(Duration::from_secs(60));
        let start = Instant::now();
        assert_eq!(throttle.record((), start), Some(0));
        for seconds in [1, 30, 59] {
            assert_eq!(
                throttle.record((), start + Duration::from_secs(seconds)),
                None
            );
        }
        assert_eq!(
            throttle.record((), start + Duration::from_secs(60)),
            Some(3)
        );
        assert_eq!(throttle.record((), start + Duration::from_secs(61)), None);
        assert_eq!(
            throttle.record((), start + Duration::from_secs(200)),
            Some(1)
        );
        assert_eq!(
            throttle.record((), start + Duration::from_secs(300)),
            Some(0)
        );
    }

    #[test]
    fn each_key_is_throttled_on_its_own() {
        let throttle = Throttle::new(Duration::from_secs(60));
        let start = Instant::now();
        assert_eq!(throttle.record("flood", start), Some(0));
        for seconds in 1..10 {
            assert_eq!(
                throttle.record("flood", start + Duration::from_secs(seconds)),
                None
            );
        }
        assert_eq!(
            throttle.record("genuine", start + Duration::from_secs(10)),
            Some(0)
        );
        assert_eq!(
            throttle.record("flood", start + Duration::from_secs(60)),
            Some(9)
        );
    }

    #[test]
    fn new_keys_stay_quiet_while_the_remembered_ones_are_all_recent() {
        let throttle = Throttle::new(Duration::from_secs(60));
        let start = Instant::now();
        for key in 0..MAX_THROTTLE_KEYS {
            assert_eq!(throttle.record(key, start), Some(0));
        }
        assert_eq!(throttle.record(MAX_THROTTLE_KEYS, start), None);
        let later = start + Duration::from_secs(60);
        assert_eq!(throttle.record(MAX_THROTTLE_KEYS, later), Some(0));
        assert_eq!(throttle.record(0, later), Some(0));
    }
}
