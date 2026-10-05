//! In-memory rolling event counters for /healthz. They reset on restart: this
//! is operator visibility, not an audit log.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(3600);
/// Bounds memory if some event source runs away between prunes.
const MAX_SAMPLES: usize = 2000;

#[derive(Default)]
pub struct Counters {
    events: Mutex<HashMap<&'static str, VecDeque<Instant>>>,
}

impl Counters {
    pub fn record(&self, name: &'static str) {
        self.record_at(name, Instant::now());
    }

    fn record_at(&self, name: &'static str, at: Instant) {
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        let bucket = events.entry(name).or_default();
        if bucket.len() == MAX_SAMPLES {
            bucket.pop_front();
        }
        bucket.push_back(at);
    }

    pub fn count_last_hour(&self, name: &str) -> usize {
        self.count_at(name, Instant::now())
    }

    fn count_at(&self, name: &str, now: Instant) -> usize {
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        let Some(bucket) = events.get_mut(name) else { return 0 };
        if let Some(cutoff) = now.checked_sub(WINDOW) {
            while bucket.front().is_some_and(|t| *t < cutoff) {
                bucket.pop_front();
            }
        }
        bucket.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_only_the_last_hour_and_is_bounded() {
        let c = Counters::default();
        let now = Instant::now();
        assert_eq!(c.count_at("x", now), 0);
        if let Some(old) = now.checked_sub(Duration::from_secs(4000)) {
            c.record_at("x", old);
        }
        c.record_at("x", now);
        assert_eq!(c.count_at("x", now), 1);
        for _ in 0..(MAX_SAMPLES + 10) {
            c.record_at("y", now);
        }
        assert_eq!(c.count_at("y", now), MAX_SAMPLES);
    }
}
