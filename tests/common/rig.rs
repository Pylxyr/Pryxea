//! Shared fakes for the station and bot tests.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use pryxea::audio::engine::{Engine, memory_source};
use pryxea::hub::StreamHub;
use pryxea::state::Shared;
use pryxea::station::{Deps, Lookup, Opener, Replier, Station, Who};
use pryxea::store::JsonStore;
use pryxea::twitch::chat::{ChatOut, Outgoing};
use pryxea::ytdlp::{MixEntry, ResolveError, Resolved};
use symphonia::core::io::MediaSource;
use tokio::sync::mpsc::UnboundedReceiver;

pub fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

pub const A: &str = "https://www.youtube.com/watch?v=aaaaaaaaaaa";
pub const B: &str = "https://www.youtube.com/watch?v=bbbbbbbbbbb";
pub const C: &str = "https://www.youtube.com/watch?v=ccccccccccc";

pub fn track(title: &str, url: &str, secs: u32) -> Resolved {
    Resolved {
        stream_url: "http://fake.invalid/stream".into(),
        headers: vec![],
        extension: "webm",
        title: title.into(),
        uploader: "Artist".into(),
        thumbnail_url: None,
        webpage_url: url.into(),
        video_id: url.rsplit('=').next().map(str::to_string),
        duration_secs: secs,
    }
}

#[derive(Default)]
pub struct FakeLookup {
    pub tracks: Mutex<HashMap<String, Result<Resolved, ResolveError>>>,
    pub delay: Mutex<Duration>,
    pub mix: Mutex<Vec<MixEntry>>,
    pub resolves: Mutex<Vec<String>>,
    pub mix_calls: Mutex<Vec<String>>,
    pub forgotten: Mutex<Vec<String>>,
}

impl FakeLookup {
    pub fn add(&self, query: &str, t: Resolved) {
        let mut m = self.tracks.lock().unwrap();
        m.insert(t.webpage_url.clone(), Ok(t.clone()));
        m.insert(query.to_string(), Ok(t));
    }
    pub fn add_err(&self, key: &str, e: ResolveError) {
        self.tracks.lock().unwrap().insert(key.to_string(), Err(e));
    }
}

impl Lookup for FakeLookup {
    fn resolve<'a>(&'a self, query: &'a str) -> BoxFuture<'a, Result<Resolved, ResolveError>> {
        Box::pin(async move {
            self.resolves.lock().unwrap().push(query.to_string());
            let delay = *self.delay.lock().unwrap();
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            self.tracks.lock().unwrap().get(query).cloned().unwrap_or(Err(ResolveError::NotFound))
        })
    }
    fn radio_mix<'a>(&'a self, video_id: &'a str) -> BoxFuture<'a, Vec<MixEntry>> {
        Box::pin(async move {
            self.mix_calls.lock().unwrap().push(video_id.to_string());
            self.mix.lock().unwrap().clone()
        })
    }
    fn forget(&self, target: &str) {
        self.forgotten.lock().unwrap().push(target.to_string());
    }
}

#[derive(Default)]
pub struct FakeOpener {
    pub opens: AtomicUsize,
    /// This many opens fail outright.
    pub fail_opens: AtomicUsize,
    /// This many opens hand back garbage that the decoder will reject.
    pub garbage_opens: AtomicUsize,
}

impl Opener for FakeOpener {
    fn open(&self, _track: &Resolved) -> Result<Box<dyn MediaSource>, String> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        if self.fail_opens.load(Ordering::SeqCst) > 0 {
            self.fail_opens.fetch_sub(1, Ordering::SeqCst);
            return Err("HTTP 403".into());
        }
        if self.garbage_opens.load(Ordering::SeqCst) > 0 {
            self.garbage_opens.fetch_sub(1, Ordering::SeqCst);
            return Ok(memory_source(vec![0x42; 3_000]));
        }
        Ok(memory_source(fixture("tone.webm")))
    }
}

pub struct Rig {
    pub station: Arc<Station>,
    pub shared: Arc<Shared>,
    pub lookup: Arc<FakeLookup>,
    pub opener: Arc<FakeOpener>,
    pub hub: Arc<StreamHub>,
    pub chat_rx: UnboundedReceiver<Outgoing>,
    pub chat: ChatOut,
    pub tunables: Arc<JsonStore>,
    pub toggles: Arc<JsonStore>,
    pub queue_file: Arc<JsonStore>,
}

pub fn temp(tag: &str, name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pryxea-station-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    let _ = std::fs::remove_file(&p);
    p
}

pub fn rig(tag: &str, pause_when_no_listeners: bool) -> Rig {
    let shared = Arc::new(Shared::new());
    let hub = StreamHub::new();
    let (engine, events) = Engine::spawn(hub.clone(), 160);
    let (chat, chat_rx) = ChatOut::channel();
    let lookup = Arc::new(FakeLookup::default());
    let opener = Arc::new(FakeOpener::default());
    let (tunables, toggles, queue_file) = (Arc::new(JsonStore::new(temp(tag, "tunables.json"))), Arc::new(JsonStore::new(temp(tag, "toggles.json"))), Arc::new(JsonStore::new(temp(tag, "queue.json"))));
    let station = Station::new(Deps {
        shared: shared.clone(),
        engine,
        hub: hub.clone(),
        lookup: lookup.clone(),
        opener: opener.clone(),
        tunables: tunables.clone(),
        toggles: toggles.clone(),
        queue_file: Some(queue_file.clone()),
        chat: chat.clone(),
        pause_when_no_listeners,
    });
    station.start(events);
    Rig { station, shared, lookup, opener, hub, chat_rx, chat, tunables, toggles, queue_file }
}

impl Rig {
    pub fn replier(&self) -> Replier {
        Replier { chat: self.chat.clone(), to: "msg-1".into() }
    }
    pub async fn out(&mut self) -> Outgoing {
        tokio::time::timeout(Duration::from_secs(8), self.chat_rx.recv()).await.expect("a chat message").expect("chat open")
    }
    /// Reads chat until a message containing `needle` shows up.
    pub async fn out_containing(&mut self, needle: &str) -> Outgoing {
        loop {
            let o = self.out().await;
            if o.text.contains(needle) {
                return o;
            }
        }
    }
    pub async fn wait_until(&self, what: &str, mut f: impl FnMut(&Rig) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !f(self) {
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    pub fn title(&self) -> Option<String> {
        self.shared.now_playing().map(|n| n.title)
    }
    pub fn set_tunables(&self, v: serde_json::Value) {
        self.tunables.write(v.as_object().unwrap().clone()).unwrap();
    }
}

pub fn who(id: u64, name: &str) -> Who {
    Who { id, name: name.into() }
}

