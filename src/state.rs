//! The player's public face: what is playing and what is queued, as seen by
//! the HTTP layer and the chat commands. The player (later) writes here; the
//! overlay, /nowplaying.json and the WebSocket read from here. Every write
//! bumps a version on a `watch` channel so WebSocket clients wake exactly
//! when something changed.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{Value, json};
use tokio::sync::watch;

use crate::telemetry::Counters;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerState {
    Idle,
    /// A request was picked and is being looked up / loaded.
    Resolving,
    Playing,
}

impl PlayerState {
    pub fn as_str(self) -> &'static str {
        match self {
            PlayerState::Idle => "idle",
            PlayerState::Resolving => "resolving",
            PlayerState::Playing => "playing",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NowPlaying {
    pub title: String,
    pub uploader: String,
    pub thumbnail_url: Option<String>,
    pub requester_name: String,
    pub webpage_url: String,
    pub started_at: Instant,
    pub duration_secs: u32,
}

#[derive(Debug, Clone)]
pub struct QueueItem {
    pub title: String,
    pub requester_name: String,
}

struct View {
    state: PlayerState,
    now_playing: Option<NowPlaying>,
    queue: Vec<QueueItem>,
}

pub struct Shared {
    pub started_at: Instant,
    pub counters: Counters,
    view: Mutex<View>,
    changed: watch::Sender<u64>,
}

impl Default for Shared {
    fn default() -> Self {
        Shared::new()
    }
}

impl Shared {
    pub fn new() -> Shared {
        Shared {
            started_at: Instant::now(),
            counters: Counters::default(),
            view: Mutex::new(View { state: PlayerState::Idle, now_playing: None, queue: Vec::new() }),
            changed: watch::channel(0).0,
        }
    }

    fn view(&self) -> std::sync::MutexGuard<'_, View> {
        self.view.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bump(&self) {
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// A receiver that resolves `changed()` after any state/now-playing/queue update.
    pub fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub fn set_player_state(&self, state: PlayerState) {
        let mut v = self.view();
        if v.state != state {
            v.state = state;
            drop(v);
            self.bump();
        }
    }

    pub fn set_now_playing(&self, np: Option<NowPlaying>) {
        self.view().now_playing = np;
        self.bump();
    }

    /// Updates what is playing without waking WebSocket clients yet; pair with [`Shared::notify_after`].
    pub fn set_now_playing_quiet(&self, np: Option<NowPlaying>) {
        self.view().now_playing = np;
    }

    /// Wakes WebSocket clients after `delay`. OBS plays the stream a few seconds behind real
    /// time, so the overlay should change when the audio does, not when we switch tracks.
    pub fn notify_after(self: &Arc<Self>, delay: std::time::Duration) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            me.bump();
        });
    }

    pub fn set_queue(&self, queue: Vec<QueueItem>) {
        self.view().queue = queue;
        self.bump();
    }

    pub fn player_state(&self) -> PlayerState {
        self.view().state
    }

    pub fn queue_len(&self) -> usize {
        self.view().queue.len()
    }

    pub fn now_playing(&self) -> Option<NowPlaying> {
        self.view().now_playing.clone()
    }

    pub fn queue_titles(&self) -> Vec<String> {
        self.view().queue.iter().map(|i| i.title.clone()).collect()
    }

    /// Same shape as the Python bot's /nowplaying.json, which the overlay reads.
    pub fn nowplaying_json(&self) -> Value {
        let v = self.view();
        let queue: Vec<Value> = v
            .queue
            .iter()
            .map(|i| {
                let title = if i.title.is_empty() { "Unknown title" } else { i.title.as_str() };
                json!({ "title": title, "requester_name": i.requester_name })
            })
            .collect();
        let size = queue.len();
        match &v.now_playing {
            None => json!({ "playing": false, "state": v.state.as_str(), "queue_size": size, "queue": queue }),
            Some(np) => json!({
                "playing": true,
                "state": v.state.as_str(),
                "title": np.title,
                "uploader": np.uploader,
                "thumbnail_url": np.thumbnail_url,
                "requester_name": np.requester_name,
                "webpage_url": np.webpage_url,
                "elapsed_seconds": np.started_at.elapsed().as_secs_f64(),
                "duration_seconds": np.duration_secs,
                "queue_size": size,
                "queue": queue,
            }),
        }
    }

    pub fn health_json(&self) -> Value {
        let v = self.view();
        json!({
            "uptime_seconds": (self.started_at.elapsed().as_secs_f64() * 10.0).round() / 10.0,
            "player_state": v.state.as_str(),
            "queue_size": v.queue.len(),
            "resolves_last_hour": {
                "success": self.counters.count_last_hour("resolve_success"),
                "failure": self.counters.count_last_hour("resolve_failure"),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn np() -> NowPlaying {
        NowPlaying {
            title: "Song".into(),
            uploader: "Artist".into(),
            thumbnail_url: None,
            requester_name: "Ann".into(),
            webpage_url: "https://example.test/v".into(),
            started_at: Instant::now(),
            duration_secs: 200,
        }
    }

    #[test]
    fn idle_payload_has_the_overlays_fields() {
        let s = Shared::new();
        s.set_queue(vec![QueueItem { title: String::new(), requester_name: "Bo".into() }]);
        let j = s.nowplaying_json();
        assert_eq!(j["playing"], false);
        assert_eq!(j["state"], "idle");
        assert_eq!(j["queue_size"], 1);
        assert_eq!(j["queue"][0]["title"], "Unknown title");
        assert!(j.get("title").is_none());
    }

    #[test]
    fn playing_payload_matches_the_python_shape() {
        let s = Shared::new();
        s.set_player_state(PlayerState::Playing);
        s.set_now_playing(Some(np()));
        let j = s.nowplaying_json();
        for key in ["playing", "state", "title", "uploader", "thumbnail_url", "requester_name", "webpage_url", "elapsed_seconds", "duration_seconds", "queue_size", "queue"] {
            assert!(j.get(key).is_some(), "missing {key}");
        }
        assert_eq!(j["state"], "playing");
        assert!(j["thumbnail_url"].is_null());
        assert_eq!(j["duration_seconds"], 200);
    }

    #[tokio::test]
    async fn every_write_wakes_subscribers_but_an_unchanged_state_does_not() {
        let s = Shared::new();
        let mut rx = s.subscribe_changes();
        s.set_player_state(PlayerState::Idle); // already idle: no wake
        assert!(!rx.has_changed().unwrap());
        s.set_player_state(PlayerState::Resolving);
        assert!(rx.has_changed().unwrap());
        rx.borrow_and_update();
        s.set_queue(Vec::new());
        rx.changed().await.unwrap();
    }
}
