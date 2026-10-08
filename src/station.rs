//! The radio station: the request queue and the player that works through it.
//!
//! Chat commands call the small synchronous methods here (limits, queue
//! changes, skip). A single player task does the slow work: look the next song
//! up, open its stream, hand it to the audio engine, follow the engine's
//! events, and, shortly before a song ends, get the next one ready so the
//! change is gapless. When the queue runs dry it can ask YouTube's Mix for a
//! related song (radio autoplay).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use symphonia::core::io::MediaSource;
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{sleep, sleep_until};

use crate::audio::engine::{Engine, Event, Outcome, TrackId};
use crate::commands;
use crate::hub::StreamHub;
use crate::state::{NowPlaying, PlayerState, QueueItem, Shared};
use crate::store::JsonStore;
use crate::tunables::Tunables;
use crate::twitch::chat::ChatOut;
use crate::youtube;
use crate::ytdlp::{MixEntry, ResolveError, Resolved};

/// Start looking the next song up this long before the current one ends.
const PREFETCH_LEAD: Duration = Duration::from_secs(20);
/// Open its stream (and hand it to the engine) this long before the end.
const OPEN_LEAD: Duration = Duration::from_secs(3);
/// Don't hammer a broken or empty radio mix.
const RADIO_RETRY: Duration = Duration::from_secs(30);
/// How far back "don't repeat" looks when picking radio songs.
const RECENT_HISTORY: usize = 40;
/// The overlay changes this long after the song does: OBS plays the stream a few seconds late.
pub const OVERLAY_SYNC_DELAY: Duration = Duration::from_secs(4);
const IDLE_TICK: Duration = Duration::from_secs(2);
pub const RADIO_NAME: &str = "\u{1F4FB} Radio Mix";

// ------------------------------------------------------------- collaborators

/// How the station finds songs. The real one is yt-dlp; tests use a fake.
pub trait Lookup: Send + Sync + 'static {
    fn resolve<'a>(&'a self, query: &'a str) -> BoxFuture<'a, Result<Resolved, ResolveError>>;
    fn radio_mix<'a>(&'a self, video_id: &'a str) -> BoxFuture<'a, Vec<MixEntry>>;
    /// Drops any cached answer for this target (its stream URL went stale).
    fn forget(&self, target: &str);
}

/// How the station gets at a song's bytes. Blocking; called from a worker thread.
pub trait Opener: Send + Sync + 'static {
    fn open(&self, track: &Resolved) -> Result<Box<dyn MediaSource>, String>;
}

pub struct ResolverLookup(pub Arc<crate::ytdlp::Resolver>);

impl Lookup for ResolverLookup {
    fn resolve<'a>(&'a self, query: &'a str) -> BoxFuture<'a, Result<Resolved, ResolveError>> {
        Box::pin(self.0.resolve(query))
    }
    fn radio_mix<'a>(&'a self, video_id: &'a str) -> BoxFuture<'a, Vec<MixEntry>> {
        Box::pin(self.0.radio_mix(video_id))
    }
    fn forget(&self, target: &str) {
        self.0.forget(target);
    }
}

pub struct HttpOpener(pub Arc<crate::net::http::Client>);

impl Opener for HttpOpener {
    fn open(&self, track: &Resolved) -> Result<Box<dyn MediaSource>, String> {
        crate::net::source::HttpSource::open(self.0.clone(), &track.stream_url, &track.headers).map(|s| Box::new(s) as Box<dyn MediaSource>).map_err(|e| e.to_string())
    }
}

pub struct Deps {
    pub shared: Arc<Shared>,
    pub engine: Engine,
    pub hub: Arc<StreamHub>,
    pub lookup: Arc<dyn Lookup>,
    pub opener: Arc<dyn Opener>,
    pub tunables: Arc<JsonStore>,
    pub toggles: Arc<JsonStore>,
    /// Where the real (non-radio) queue is saved between runs.
    pub queue_file: Option<Arc<JsonStore>>,
    pub chat: ChatOut,
    /// Hold off starting new songs while nobody is connected to the stream.
    pub pause_when_no_listeners: bool,
}

// -------------------------------------------------------------------- types

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    pub seq: u64,
    pub webpage_url: String,
    /// Twitch user id; 0 means radio-mix filler.
    pub requester_id: u64,
    pub requester_name: String,
    pub title: String,
    pub uploader: String,
}

impl QueueEntry {
    pub fn is_radio(&self) -> bool {
        self.requester_id == 0
    }
}

#[derive(Debug, Clone)]
pub struct Who {
    pub id: u64,
    pub name: String,
}

/// Where the slow half of a request (found it / failed) reports back to.
#[derive(Clone)]
pub struct Replier {
    pub chat: ChatOut,
    pub to: String,
}

impl Replier {
    fn say(&self, text: impl Into<String>) {
        self.chat.reply(&self.to, text);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipOutcome {
    Skipped,
    NothingPlaying,
    NotYours,
}

struct Active {
    entry: QueueEntry,
    /// Set once the song has been handed to the engine.
    engine_id: Option<TrackId>,
}

struct Prepared {
    entry: QueueEntry,
    track: Resolved,
    engine_id: TrackId,
}

#[derive(Default)]
struct Inner {
    queue: VecDeque<QueueEntry>,
    active: Option<Active>,
    pending_by_chatter: HashMap<u64, u32>,
    last_request_at: HashMap<u64, Instant>,
    inflight: HashMap<u64, String>,
    last_played_url: Option<String>,
    recent: VecDeque<String>,
    radio_failed_at: Option<Instant>,
    radio_fill_running: bool,
    skip_pending: bool,
    /// A radio song already loaded into the engine behind the current one (engine id, queue seq).
    prepared_radio: Option<(TrackId, u64)>,
    next_seq: u64,
    next_engine_id: u64,
}

pub struct Station {
    d: Deps,
    inner: Mutex<Inner>,
    wake: Notify,
}

enum LoadFail {
    /// Skipped by a mod while loading: nothing to say.
    Silent,
    Say(String),
}

// ------------------------------------------------------------------ the API

impl Station {
    pub fn new(deps: Deps) -> Arc<Station> {
        Arc::new(Station { d: deps, inner: Mutex::new(Inner { next_engine_id: 1, ..Inner::default() }), wake: Notify::new() })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn tunables(&self) -> Tunables {
        Tunables::from_map(&self.d.tunables.read())
    }

    fn radio_enabled(&self) -> bool {
        crate::toggles::Toggles::from_map(&self.d.toggles.read()).radio_autoplay_enabled
    }

    /// Starts the player. `events` is the engine's event stream.
    pub fn start(self: &Arc<Self>, events: UnboundedReceiver<Event>) -> tokio::task::JoinHandle<()> {
        let me = Arc::clone(self);
        tokio::spawn(async move { me.run(events).await })
    }

    pub fn queue_len(&self) -> usize {
        self.lock().queue.len()
    }

    pub fn queued_titles(&self) -> Vec<String> {
        self.lock().queue.iter().map(|e| e.title.clone()).collect()
    }

    pub fn active_requester_id(&self) -> Option<u64> {
        self.lock().active.as_ref().map(|a| a.entry.requester_id)
    }

    pub fn now_playing(&self) -> Option<NowPlaying> {
        self.d.shared.now_playing()
    }

    pub fn radio_status(&self) -> bool {
        self.radio_enabled()
    }

    pub fn set_radio(&self, on: bool) {
        let _ = self.d.toggles.update(|mut m| {
            m.extend(crate::toggles::Toggles { radio_autoplay_enabled: on }.to_map());
            Some(m)
        });
        self.wake.notify_one();
    }

    /// Puts a song in the queue. Real requests go ahead of waiting radio filler, behind other real ones.
    pub fn enqueue(&self, mut entry: QueueEntry) {
        let cancel = {
            let mut inner = self.lock();
            inner.next_seq += 1;
            entry.seq = inner.next_seq;
            if entry.is_radio() {
                inner.queue.push_back(entry);
                None
            } else {
                let at = inner.queue.iter().position(QueueEntry::is_radio).unwrap_or(inner.queue.len());
                inner.queue.insert(at, entry);
                // A radio song already loaded behind the current track must make way for a real request.
                match inner.prepared_radio {
                    Some((id, seq)) if inner.queue.front().is_some_and(|f| f.seq != seq) => {
                        inner.prepared_radio = None;
                        Some(id)
                    }
                    _ => None,
                }
            }
        };
        if let Some(id) = cancel {
            self.d.engine.cancel(id);
        }
        self.queue_changed();
    }

    fn queue_changed(&self) {
        let items: Vec<QueueItem> = self.lock().queue.iter().map(|e| QueueItem { title: e.title.clone(), requester_name: e.requester_name.clone() }).collect();
        self.d.shared.set_queue(items);
        self.persist();
        self.wake.notify_one();
    }

    fn persist(&self) {
        let Some(store) = &self.d.queue_file else { return };
        let items: Vec<Value> = self
            .lock()
            .queue
            .iter()
            .filter(|e| !e.is_radio())
            .map(|e| json!({"webpage_url": e.webpage_url, "requester_id": e.requester_id, "requester_name": e.requester_name, "title": e.title, "uploader": e.uploader}))
            .collect();
        if let Err(e) = store.update(|mut m| {
            m.insert("items".into(), Value::Array(items));
            Some(m)
        }) {
            crate::warn!("cannot save the queue: {e}");
        }
    }

    /// Reloads the queue saved by the last run. Returns how many songs came back.
    pub fn restore(&self) -> usize {
        let Some(store) = &self.d.queue_file else { return 0 };
        let items = store.read().get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut restored = 0;
        for item in items {
            let text = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let (Some(url), Some(id)) = (item.get("webpage_url").and_then(Value::as_str), item.get("requester_id").and_then(Value::as_u64)) else { continue };
            self.enqueue(QueueEntry {
                seq: 0,
                webpage_url: url.to_string(),
                requester_id: id,
                requester_name: Some(text("requester_name")).filter(|n| !n.is_empty()).unwrap_or_else(|| "a viewer".into()),
                title: text("title"),
                uploader: text("uploader"),
            });
            restored += 1;
        }
        restored
    }

    // ------------------------------------------------------------ requests

    /// Handles `!sr`. Returns the immediate reply; the slow lookup continues in the background and
    /// reports through `replier` ("Queued: ..." or why not).
    pub fn request(self: &Arc<Self>, who: &Who, query: &str, replier: Replier) -> String {
        let query = query.trim();
        if query.is_empty() {
            return commands::USAGE_SR.to_string();
        }
        let normalized = query.to_lowercase();
        let tunables = self.tunables();
        let now = Instant::now();
        {
            let mut inner = self.lock();
            if inner.inflight.get(&who.id) == Some(&normalized) {
                return "Still looking that up \u{2014} hang tight!".into();
            }
            // Check-and-reserve with no await in between, so rapid-fire !sr can't race past the caps.
            let cooldown = Duration::from_secs(u64::from(tunables.request_cooldown_seconds));
            if let Some(last) = inner.last_request_at.get(&who.id) {
                if !cooldown.is_zero() && now.duration_since(*last) < cooldown {
                    return format!("Slow down \u{2014} try again in {:.0}s.", (cooldown - now.duration_since(*last)).as_secs_f64());
                }
            }
            let pending = inner.pending_by_chatter.get(&who.id).copied().unwrap_or(0);
            if pending >= tunables.max_pending_per_chatter {
                return format!("You already have {pending} request(s) queued \u{2014} wait for one to play first.");
            }
            if inner.queue.len() >= tunables.queue_cap {
                return "Queue's full right now \u{2014} try again in a bit.".into();
            }
            inner.last_request_at.insert(who.id, now);
            inner.pending_by_chatter.insert(who.id, pending + 1);
            inner.inflight.insert(who.id, normalized.clone());
        }
        let ack = commands::looking_up(query);
        let (me, who, query) = (Arc::clone(self), who.clone(), query.to_string());
        tokio::spawn(async move {
            let queued = me.resolve_and_queue(&who, &query, &replier).await;
            let mut inner = me.lock();
            if !queued {
                Self::release_pending(&mut inner, who.id);
            }
            if inner.inflight.get(&who.id) == Some(&normalized) {
                inner.inflight.remove(&who.id);
            }
        });
        ack
    }

    fn release_pending(inner: &mut Inner, id: u64) {
        match inner.pending_by_chatter.get(&id).copied() {
            Some(n) if n > 1 => {
                inner.pending_by_chatter.insert(id, n - 1);
            }
            _ => {
                inner.pending_by_chatter.remove(&id);
            }
        }
    }

    /// True when the song was queued (the pending slot then stays reserved until it starts playing).
    async fn resolve_and_queue(&self, who: &Who, query: &str, reply: &Replier) -> bool {
        let track = match self.d.lookup.resolve(query).await {
            Ok(t) => t,
            Err(ResolveError::UnsupportedSource) => {
                reply.say("Only YouTube links and searches are supported.");
                return false;
            }
            Err(ResolveError::NotFound) => {
                reply.say("No results for that.");
                return false;
            }
            Err(ResolveError::Live) => {
                reply.say("Can't queue a livestream \u{2014} sorry!");
                return false;
            }
            Err(e) => {
                crate::warn!("song request {query:?} failed: {e}");
                reply.say("Couldn't fetch that \u{2014} try a different search or link.");
                return false;
            }
        };
        // Limits are re-read: a mod may have changed them during the lookup.
        let tunables = self.tunables();
        if tunables.max_request_duration_seconds > 0 && track.duration_secs > tunables.max_request_duration_seconds {
            reply.say(format!("That's too long to queue \u{2014} max is {} minute(s).", tunables.max_request_duration_seconds / 60));
            return false;
        }
        let queued_ok = {
            let inner = self.lock();
            let same = |url: &str| url == track.webpage_url;
            if inner.active.as_ref().is_some_and(|a| same(&a.entry.webpage_url)) || inner.queue.iter().any(|e| same(&e.webpage_url)) {
                Err(format!("{} is already queued.", track.title))
            } else if inner.queue.len() >= tunables.queue_cap {
                Err("Queue's full right now \u{2014} try again in a bit.".to_string())
            } else {
                Ok(())
            }
        };
        if let Err(msg) = queued_ok {
            reply.say(msg);
            return false;
        }
        self.enqueue(QueueEntry { seq: 0, webpage_url: track.webpage_url.clone(), requester_id: who.id, requester_name: who.name.clone(), title: track.title.clone(), uploader: track.uploader.clone() });
        self.d.shared.counters.record("requests_queued");
        reply.say(commands::queued(&track.title, self.queue_len()));
        true
    }

    // ---------------------------------------------------------------- skip

    pub fn skip(&self, chatter_id: u64, is_mod: bool) -> SkipOutcome {
        let (engine_id, requester) = {
            let inner = self.lock();
            match &inner.active {
                None => return SkipOutcome::NothingPlaying,
                Some(a) => (a.engine_id, a.entry.requester_id),
            }
        };
        if !is_mod && requester != chatter_id {
            return SkipOutcome::NotYours;
        }
        match engine_id {
            Some(_) => self.d.engine.skip(),
            None => self.lock().skip_pending = true, // still being looked up or opened
        }
        SkipOutcome::Skipped
    }

    // ------------------------------------------------------- the player task

    async fn run(self: Arc<Self>, mut events: UnboundedReceiver<Event>) {
        let mut carry: Option<Prepared> = None;
        loop {
            let (entry, prepared) = match carry.take() {
                Some(p) => (p.entry.clone(), Some(p)),
                None => (self.next_entry().await, None),
            };
            carry = self.play_entry(entry, prepared, &mut events).await;
        }
    }

    /// Waits until a song may start, and takes it off the queue.
    async fn next_entry(self: &Arc<Self>) -> QueueEntry {
        loop {
            let waiting_for_listeners = self.d.pause_when_no_listeners && self.d.hub.listener_count() == 0;
            let entry = if waiting_for_listeners { None } else { self.lock().queue.pop_front() };
            if let Some(entry) = entry {
                {
                    let mut inner = self.lock();
                    Self::release_pending(&mut inner, entry.requester_id);
                    inner.active = Some(Active { entry: entry.clone(), engine_id: None });
                    inner.skip_pending = false;
                }
                self.queue_changed();
                return entry;
            }
            self.maybe_start_radio_fill();
            let mut listeners = self.d.hub.listeners();
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = listeners.changed() => {}
                _ = sleep(IDLE_TICK) => {}
            }
        }
    }

    fn take_skip_pending(&self) -> bool {
        std::mem::take(&mut self.lock().skip_pending)
    }

    fn fresh_engine_id(&self) -> TrackId {
        let mut inner = self.lock();
        inner.next_engine_id += 1;
        inner.next_engine_id
    }

    /// Looks a song up and checks it may play.
    async fn resolve_checked(&self, entry: &QueueEntry) -> Result<Resolved, LoadFail> {
        let track = match self.d.lookup.resolve(&entry.webpage_url).await {
            Ok(t) => t,
            Err(ResolveError::Live) => return Err(LoadFail::Say(format!("Skipped {}'s song \u{2014} it's a livestream now.", entry.requester_name))),
            Err(e) => {
                crate::warn!("could not load {}: {e}", entry.webpage_url);
                return Err(LoadFail::Say(format!("Couldn't load {}'s song \u{2014} skipping it.", entry.requester_name)));
            }
        };
        let limit = self.tunables().max_request_duration_seconds;
        if limit > 0 && track.duration_secs > limit {
            return Err(LoadFail::Say(format!("Skipped {}'s song \u{2014} it's too long to play now.", entry.requester_name)));
        }
        Ok(track)
    }

    async fn open(&self, track: &Resolved) -> Result<Box<dyn MediaSource>, String> {
        let (opener, t) = (Arc::clone(&self.d.opener), track.clone());
        tokio::task::spawn_blocking(move || opener.open(&t)).await.unwrap_or_else(|_| Err("the stream opener crashed".into()))
    }

    /// Resolve, open and hand to the engine. `fresh` re-resolves instead of trusting a cached lookup.
    async fn load(&self, entry: &QueueEntry, fresh: bool) -> Result<(Resolved, TrackId), LoadFail> {
        if fresh {
            self.d.lookup.forget(&entry.webpage_url);
        }
        let track = self.resolve_checked(entry).await?;
        if self.take_skip_pending() {
            return Err(LoadFail::Silent);
        }
        let source = match self.open(&track).await {
            Ok(s) => s,
            Err(why) => {
                crate::warn!("could not open {}: {why}", entry.webpage_url);
                return Err(LoadFail::Say(format!("Couldn't load {}'s song \u{2014} skipping it.", entry.requester_name)));
            }
        };
        if self.take_skip_pending() {
            return Err(LoadFail::Silent);
        }
        let id = self.fresh_engine_id();
        if let Some(a) = self.lock().active.as_mut() {
            a.engine_id = Some(id);
        }
        self.d.engine.play(id, source, Some(track.extension));
        Ok((track, id))
    }

    fn deactivate(&self) {
        let mut inner = self.lock();
        inner.active = None;
        inner.skip_pending = false;
    }

    /// Plays one entry start to finish. Returns the next song if it was already loaded into the engine.
    async fn play_entry(&self, entry: QueueEntry, prepared: Option<Prepared>, events: &mut UnboundedReceiver<Event>) -> Option<Prepared> {
        let (mut track, mut engine_id) = match prepared {
            Some(p) => {
                self.activate_prepared(&p);
                (p.track, p.engine_id)
            }
            None => {
                self.d.shared.set_player_state(PlayerState::Resolving);
                match self.load(&entry, false).await {
                    Ok(loaded) => loaded,
                    Err(fail) => return self.give_up(&entry, fail),
                }
            }
        };

        // Wait for the engine to actually start it; one fresh retry if it fails before that.
        let mut retried = false;
        loop {
            match events.recv().await? {
                Event::Started(id) if id == engine_id => break,
                Event::Ended(id, outcome) if id == engine_id => match outcome {
                    Outcome::Failed(why) if !retried => {
                        crate::info!("{} failed to start ({why}); trying once more with a fresh lookup.", entry.title);
                        retried = true;
                        match self.load(&entry, true).await {
                            Ok(loaded) => (track, engine_id) = loaded,
                            Err(fail) => return self.give_up(&entry, fail),
                        }
                    }
                    Outcome::Failed(why) => {
                        crate::warn!("{} could not be played: {why}", entry.title);
                        return self.give_up(&entry, LoadFail::Say(format!("Couldn't load {}'s song \u{2014} skipping it.", entry.requester_name)));
                    }
                    _ => return self.give_up(&entry, LoadFail::Silent),
                },
                _ => {}
            }
        }

        let started = Instant::now();
        self.mark_started(&entry, &track);
        let mut next: Option<Prepared> = None;
        let mut prefetch: Option<BoxFuture<'_, Option<Prepared>>> = (track.duration_secs > 0).then(|| Box::pin(self.prefetch(started, track.duration_secs)) as BoxFuture<'_, Option<Prepared>>);
        loop {
            tokio::select! {
                event = events.recv() => match event? {
                    Event::Ended(id, outcome) if id == engine_id => {
                        if let Outcome::Failed(why) = outcome {
                            let msg = if why.contains("stalled") { "playback stalled" } else { "playback failed" };
                            crate::warn!("{} stopped early: {why}", entry.title);
                            self.d.chat.announce(format!("Skipped {}'s song \u{2014} {msg}.", entry.requester_name));
                        }
                        break;
                    }
                    // The loaded-ahead song was cancelled (a real request took its place).
                    Event::Ended(id, _) if next.as_ref().is_some_and(|p| p.engine_id == id) => next = None,
                    _ => {}
                },
                loaded = async { prefetch.as_mut().expect("guarded").await }, if prefetch.is_some() => {
                    prefetch = None;
                    next = loaded;
                }
            }
        }
        drop(prefetch);
        if next.is_none() {
            self.finish_track();
        }
        next
    }

    fn give_up(&self, entry: &QueueEntry, fail: LoadFail) -> Option<Prepared> {
        if let LoadFail::Say(msg) = fail {
            self.d.chat.announce(msg);
        }
        crate::debug!("gave up on {}", entry.webpage_url);
        self.deactivate();
        self.d.shared.set_player_state(PlayerState::Idle);
        None
    }

    fn activate_prepared(&self, p: &Prepared) {
        let mut inner = self.lock();
        if inner.queue.front().is_some_and(|f| f.seq == p.entry.seq) {
            inner.queue.pop_front();
        }
        if inner.prepared_radio.is_some_and(|(_, seq)| seq == p.entry.seq) {
            inner.prepared_radio = None;
        }
        Self::release_pending(&mut inner, p.entry.requester_id);
        inner.active = Some(Active { entry: p.entry.clone(), engine_id: Some(p.engine_id) });
        inner.skip_pending = false;
        drop(inner);
        self.queue_changed();
    }

    fn mark_started(&self, entry: &QueueEntry, track: &Resolved) {
        let np = NowPlaying {
            title: track.title.clone(),
            uploader: track.uploader.clone(),
            thumbnail_url: track.thumbnail_url.clone(),
            requester_name: entry.requester_name.clone(),
            webpage_url: track.webpage_url.clone(),
            started_at: Instant::now(),
            duration_secs: track.duration_secs,
        };
        {
            let mut inner = self.lock();
            inner.last_played_url = Some(track.webpage_url.clone());
            if let Some(id) = youtube::video_id(&track.webpage_url) {
                if inner.recent.len() == RECENT_HISTORY {
                    inner.recent.pop_front();
                }
                inner.recent.push_back(id);
            }
        }
        self.d.shared.set_player_state(PlayerState::Playing);
        self.d.shared.set_now_playing_quiet(Some(np));
        self.d.shared.notify_after(OVERLAY_SYNC_DELAY);
        self.d.shared.counters.record("tracks_played");
        crate::info!("Now playing: {} (requested by {})", track.title, entry.requester_name);
        self.d.chat.announce(if entry.is_radio() { format!("Now Playing: {}", track.title) } else { format!("{}'s song request is Now Playing: {}", entry.requester_name, track.title) });
    }

    fn finish_track(&self) {
        self.deactivate();
        self.d.shared.set_player_state(PlayerState::Idle);
        self.d.shared.set_now_playing_quiet(None);
        self.d.shared.notify_after(OVERLAY_SYNC_DELAY);
    }

    // ------------------------------------------------- gapless prefetching

    fn front(&self) -> Option<QueueEntry> {
        self.lock().queue.front().cloned()
    }

    fn is_front(&self, seq: u64) -> bool {
        self.lock().queue.front().is_some_and(|f| f.seq == seq)
    }

    /// Gets the next song fully ready just before the current one ends. Never fails loudly:
    /// if anything goes wrong the normal path simply loads it afresh when its turn comes.
    async fn prefetch(&self, started: Instant, duration_secs: u32) -> Option<Prepared> {
        let end = started + Duration::from_secs(u64::from(duration_secs));
        sleep_until((end.checked_sub(PREFETCH_LEAD)).unwrap_or(started).into()).await;
        let candidate = match self.front() {
            Some(c) => c,
            None => {
                if let Some(pick) = self.radio_pick().await {
                    self.enqueue(pick);
                }
                self.front()?
            }
        };
        let track = self.resolve_checked(&candidate).await.ok()?;
        sleep_until((end.checked_sub(OPEN_LEAD)).unwrap_or(started).into()).await;
        if !self.is_front(candidate.seq) {
            return None; // a real request took the front spot, or it was removed
        }
        let source = self.open(&track).await.ok()?;
        if !self.is_front(candidate.seq) {
            return None;
        }
        // From here to the return there is no await, so this can't be cancelled half-done.
        let id = self.fresh_engine_id();
        if candidate.is_radio() {
            self.lock().prepared_radio = Some((id, candidate.seq));
        }
        self.d.engine.play(id, source, Some(track.extension));
        Some(Prepared { entry: candidate, track, engine_id: id })
    }

    // ---------------------------------------------------------------- radio

    fn maybe_start_radio_fill(self: &Arc<Self>) {
        {
            let mut inner = self.lock();
            let backed_off = inner.radio_failed_at.is_some_and(|t| t.elapsed() < RADIO_RETRY);
            if inner.radio_fill_running || !inner.queue.is_empty() || inner.active.is_some() || inner.last_played_url.is_none() || backed_off {
                return;
            }
            inner.radio_fill_running = true;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            if let Some(pick) = me.radio_pick().await {
                crate::info!("Radio autoplay queued: {}", pick.title);
                me.enqueue(pick);
            }
            me.lock().radio_fill_running = false;
        });
    }

    /// One related song from YouTube's Mix for whatever played last, or `None`.
    async fn radio_pick(&self) -> Option<QueueEntry> {
        if !self.radio_enabled() {
            self.lock().radio_failed_at = Some(Instant::now());
            return None;
        }
        let seed = self.lock().last_played_url.clone()?;
        let seed_id = youtube::video_id(&seed)?;
        let entries = self.d.lookup.radio_mix(&seed_id).await;
        let mut inner = self.lock();
        let pick = entries.into_iter().find(|e| e.id != seed_id && !inner.recent.contains(&e.id));
        match pick {
            Some(e) => {
                inner.recent.push_back(e.id.clone());
                Some(QueueEntry { seq: 0, webpage_url: e.url, requester_id: 0, requester_name: RADIO_NAME.into(), title: e.title, uploader: e.uploader })
            }
            None => {
                inner.radio_failed_at = Some(Instant::now());
                None
            }
        }
    }
}
