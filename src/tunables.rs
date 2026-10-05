//! Request-limit knobs adjustable at runtime (settings page), stored in
//! `data/tunables.json`. Same keys, defaults and ranges as the Python bot,
//! minus `vote_skip_threshold` (there is no !voteskip any more). An old file
//! that still has that key is read fine; the key is dropped on next save.

use serde_json::{Value, json};

use crate::store::JsonMap;

/// (key, min, max)
pub const BOUNDS: [(&str, i64, i64); 4] = [
    ("max_pending_per_chatter", 1, 10),
    ("request_cooldown_seconds", 0, 3600),
    ("queue_cap", 1, 200),
    ("max_request_duration_seconds", 30, 3600),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tunables {
    pub max_pending_per_chatter: u32,
    pub request_cooldown_seconds: u32,
    pub queue_cap: usize,
    pub max_request_duration_seconds: u32,
}

impl Default for Tunables {
    fn default() -> Self {
        Tunables {
            max_pending_per_chatter: 2,
            request_cooldown_seconds: 0,
            queue_cap: 50,
            max_request_duration_seconds: 600,
        }
    }
}

/// Python's `int(x)` accepted ints, whole floats and numeric strings; so do we.
fn as_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn field(map: &JsonMap, name: &str, default: i64) -> i64 {
    let Some(raw) = map.get(name) else { return default };
    let Some(value) = as_int(raw) else {
        crate::warn!("tunables.json: {name:?} is not a valid number ({raw}) - using default.");
        return default;
    };
    let (_, lo, hi) = BOUNDS.iter().copied().find(|(k, ..)| *k == name).expect("known tunable");
    let clamped = value.clamp(lo, hi);
    if clamped != value {
        crate::warn!("tunables.json: {name:?}={value} is outside the allowed range {lo}-{hi} - clamping to {clamped}.");
    }
    clamped
}

impl Tunables {
    pub fn from_map(map: &JsonMap) -> Tunables {
        let d = Tunables::default();
        Tunables {
            max_pending_per_chatter: field(map, "max_pending_per_chatter", i64::from(d.max_pending_per_chatter)) as u32,
            request_cooldown_seconds: field(map, "request_cooldown_seconds", i64::from(d.request_cooldown_seconds)) as u32,
            queue_cap: field(map, "queue_cap", d.queue_cap as i64) as usize,
            max_request_duration_seconds: field(map, "max_request_duration_seconds", i64::from(d.max_request_duration_seconds)) as u32,
        }
    }

    pub fn to_map(&self) -> JsonMap {
        let Value::Object(map) = json!({
            "max_pending_per_chatter": self.max_pending_per_chatter,
            "request_cooldown_seconds": self.request_cooldown_seconds,
            "queue_cap": self.queue_cap,
            "max_request_duration_seconds": self.max_request_duration_seconds,
        }) else {
            unreachable!()
        };
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(v: Value) -> JsonMap {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn empty_file_gives_defaults() {
        assert_eq!(Tunables::from_map(&JsonMap::new()), Tunables::default());
    }

    #[test]
    fn values_are_clamped_and_garbage_falls_back_per_field() {
        let t = Tunables::from_map(&map(json!({
            "queue_cap": 9999,
            "max_pending_per_chatter": "4",
            "request_cooldown_seconds": "soon",
            "max_request_duration_seconds": 12.9,
            "vote_skip_threshold": 5
        })));
        assert_eq!(t.queue_cap, 200);
        assert_eq!(t.max_pending_per_chatter, 4);
        assert_eq!(t.request_cooldown_seconds, 0);
        assert_eq!(t.max_request_duration_seconds, 30);
    }

    #[test]
    fn round_trips_without_the_retired_key() {
        let t = Tunables { queue_cap: 77, ..Tunables::default() };
        let m = t.to_map();
        assert!(!m.contains_key("vote_skip_threshold"));
        assert_eq!(Tunables::from_map(&m), t);
    }
}
