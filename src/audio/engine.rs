//! The real-time engine: strings tracks together without gaps, encodes the
//! result to Ogg/Opus and publishes it to the stream hub.
//!
//! Each track is decoded on its own short-lived thread (network reads are
//! blocking) into a bounded queue of 100 ms blocks. The engine task wakes
//! every 100 ms, pulls one block from whichever track is current (rolling
//! straight into the next track mid-block when one ends), and, only while
//! someone is listening, encodes it as five 20 ms Opus packets in one Ogg
//! page. With no tracks and no listeners it sleeps entirely.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use symphonia::core::io::MediaSource;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::{MissedTickBehavior, interval};

use super::decode::{TrackDecoder, to_i16};
use super::ogg::OpusOggWriter;
use super::opus::{Encoder, FRAME_SIZE};
use crate::hub::StreamHub;

/// One engine tick: 100 ms of audio, five Opus packets, one Ogg page.
pub const BLOCK_FRAMES: usize = FRAME_SIZE * 5;
pub const BLOCK_SAMPLES: usize = BLOCK_FRAMES * 2;
const BLOCK_PERIOD: Duration = Duration::from_millis(100);
/// How far ahead of playback a track may decode (about 1.2 s, ~230 KB of i16).
const FEED_BLOCKS_AHEAD: usize = 12;
/// A current track that delivers nothing for this long is declared stuck.
const STALL_LIMIT: Duration = Duration::from_secs(20);
const STALL_SAMPLES: usize = 48_000 * 2 * STALL_LIMIT.as_secs() as usize;
const DECODE_THREAD_STACK: usize = 512 * 1024;

pub type TrackId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Played to its end.
    Finished,
    /// Skipped or cancelled before it ended.
    Skipped,
    /// Could not be opened, decoded, or stalled.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The track's first audio just went into the stream.
    Started(TrackId),
    Ended(TrackId, Outcome),
}

// -------------------------------------------------------------------- feeds

enum Msg {
    Block(Vec<i16>),
    End,
    Error(String),
}

/// One track's decoded audio, as seen by the engine.
struct Feed {
    id: TrackId,
    rx: Receiver<Msg>,
    cur: Vec<i16>,
    pos: usize,
    ended: bool,
    failed: Option<String>,
    started: bool,
    stalled_samples: usize,
}

impl Feed {
    fn new(id: TrackId, rx: Receiver<Msg>) -> Feed {
        Feed { id, rx, cur: Vec::new(), pos: 0, ended: false, failed: None, started: false, stalled_samples: 0 }
    }

    /// Copies whatever is available right now into `out`; never blocks. When the
    /// buffered data runs out exactly at the end of `out`, it still peeks once at
    /// the channel so a track ending on a block boundary is reported on time.
    fn pull(&mut self, out: &mut [i16]) -> usize {
        let mut n = 0;
        loop {
            if self.pos < self.cur.len() {
                if n == out.len() {
                    break;
                }
                let take = (self.cur.len() - self.pos).min(out.len() - n);
                out[n..n + take].copy_from_slice(&self.cur[self.pos..self.pos + take]);
                self.pos += take;
                n += take;
                continue;
            }
            if self.ended || self.failed.is_some() {
                break;
            }
            match self.rx.try_recv() {
                Ok(Msg::Block(b)) => {
                    self.cur = b;
                    self.pos = 0;
                }
                Ok(Msg::End) => self.ended = true,
                Ok(Msg::Error(e)) => self.failed = Some(e),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => self.failed = Some("the decoder stopped unexpectedly".into()),
            }
        }
        n
    }

    fn drained(&self) -> bool {
        self.pos >= self.cur.len()
    }
}

fn spawn_feed(id: TrackId, source: Box<dyn MediaSource>, extension: Option<String>) -> Feed {
    let (tx, rx) = sync_channel(FEED_BLOCKS_AHEAD);
    let spawned = std::thread::Builder::new().name(format!("decode-{id}")).stack_size(DECODE_THREAD_STACK).spawn({
        let tx = tx.clone();
        move || decode_thread(source, extension, tx)
    });
    if let Err(e) = spawned {
        let _ = tx.send(Msg::Error(format!("cannot start a decoder thread: {e}")));
    }
    Feed::new(id, rx)
}

fn decode_thread(source: Box<dyn MediaSource>, extension: Option<String>, tx: SyncSender<Msg>) {
    let mut decoder = match TrackDecoder::open(source, extension.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            let _ = tx.send(Msg::Error(e.to_string()));
            return;
        }
    };
    let (mut pcm, mut block) = (Vec::new(), Vec::<i16>::with_capacity(BLOCK_SAMPLES * 2));
    loop {
        let more = match decoder.decode_more(&mut pcm) {
            Ok(more) => more,
            Err(e) => {
                let _ = tx.send(Msg::Error(e.to_string()));
                return;
            }
        };
        to_i16(&pcm, &mut block);
        pcm.clear();
        while block.len() >= BLOCK_SAMPLES {
            let rest = block.split_off(BLOCK_SAMPLES);
            let full = std::mem::replace(&mut block, rest);
            if tx.send(Msg::Block(full)).is_err() {
                return; // the engine dropped this track
            }
        }
        if !more {
            if !block.is_empty() && tx.send(Msg::Block(block)).is_err() {
                return;
            }
            let _ = tx.send(Msg::End);
            return;
        }
    }
}

// ----------------------------------------------------------------- pipeline

/// The synchronous heart: which track is current, what is queued behind it,
/// and how a 100 ms block is assembled across track boundaries.
#[derive(Default)]
struct Pipeline {
    current: Option<Feed>,
    pending: VecDeque<Feed>,
}

impl Pipeline {
    fn has_tracks(&self) -> bool {
        self.current.is_some() || !self.pending.is_empty()
    }

    fn enqueue(&mut self, feed: Feed) {
        self.pending.push_back(feed);
    }

    /// Ends the current track (if any).
    fn skip(&mut self, events: &mut Vec<Event>) {
        if let Some(feed) = self.current.take() {
            events.push(Event::Ended(feed.id, Outcome::Skipped));
        }
    }

    /// Removes one track wherever it is.
    fn cancel(&mut self, id: TrackId, events: &mut Vec<Event>) {
        if self.current.as_ref().is_some_and(|f| f.id == id) {
            self.skip(events);
        } else if let Some(at) = self.pending.iter().position(|f| f.id == id) {
            self.pending.remove(at);
            events.push(Event::Ended(id, Outcome::Skipped));
        }
    }

    /// Fills `out` (silence where nothing is playing), pushing events as tracks start and end.
    fn fill(&mut self, out: &mut [i16], events: &mut Vec<Event>) {
        let mut filled = 0;
        while filled < out.len() {
            if self.current.is_none() {
                self.current = self.pending.pop_front();
            }
            let Some(feed) = self.current.as_mut() else { break };
            let got = feed.pull(&mut out[filled..]);
            if got > 0 {
                if !feed.started {
                    feed.started = true;
                    events.push(Event::Started(feed.id));
                }
                feed.stalled_samples = 0;
                filled += got;
            }
            if let Some(reason) = feed.failed.take() {
                events.push(Event::Ended(feed.id, Outcome::Failed(reason)));
                self.current = None;
            } else if feed.ended && feed.drained() {
                events.push(Event::Ended(feed.id, Outcome::Finished));
                self.current = None;
            } else if filled < out.len() {
                // Underrun: the decoder or network is behind. Play silence for the rest of this block.
                feed.stalled_samples += out.len() - filled;
                if feed.stalled_samples > STALL_SAMPLES {
                    events.push(Event::Ended(feed.id, Outcome::Failed("the stream stalled for 20 seconds".into())));
                    self.current = None;
                    continue;
                }
                break;
            }
        }
        out[filled..].fill(0);
    }
}

// ------------------------------------------------------------------ session

/// One logical Ogg/Opus stream: created when a listener appears, dropped when the last one leaves.
struct Session {
    encoder: Encoder,
    ogg: OpusOggWriter,
    packet: Vec<u8>,
}

impl Session {
    fn start(bitrate_bps: i32, hub: &StreamHub) -> Result<Session, super::opus::OpusError> {
        let encoder = Encoder::new(bitrate_bps)?;
        let serial = SystemTime::now().duration_since(UNIX_EPOCH).map_or(1, |d| d.subsec_nanos() ^ (d.as_secs() as u32).rotate_left(7));
        let mut ogg = OpusOggWriter::new(serial, encoder.lookahead());
        hub.begin_session();
        hub.publish(Bytes::from(ogg.header_pages()));
        Ok(Session { encoder, ogg, packet: Vec::with_capacity(1_500) })
    }

    fn encode_block(&mut self, pcm: &[i16], hub: &StreamHub) {
        for frame in pcm.chunks_exact(FRAME_SIZE * 2) {
            if let Err(e) = self.encoder.encode(frame, &mut self.packet) {
                crate::warn!("Opus encode failed: {e}");
                continue;
            }
            if let Some(page) = self.ogg.push_packet(&self.packet, FRAME_SIZE as u64) {
                hub.publish(Bytes::from(page));
            }
        }
    }
}

// ------------------------------------------------------------------- engine

enum Cmd {
    Play(TrackId, Box<dyn MediaSource>, Option<String>),
    Skip,
    Cancel(TrackId),
}

/// Handle to the engine task. Cheap to clone.
#[derive(Clone)]
pub struct Engine {
    cmds: UnboundedSender<Cmd>,
}

impl Engine {
    /// Starts the engine on the current tokio runtime. `bitrate_kbps` is the Opus bitrate.
    pub fn spawn(hub: Arc<StreamHub>, bitrate_kbps: u32) -> (Engine, UnboundedReceiver<Event>) {
        let (cmds, cmd_rx) = unbounded_channel();
        let (event_tx, event_rx) = unbounded_channel();
        tokio::spawn(run(hub, bitrate_kbps as i32 * 1000, cmd_rx, event_tx));
        (Engine { cmds }, event_rx)
    }

    /// Queues a track behind whatever is playing; it starts the instant the current one ends.
    /// `extension` ("webm", "m4a") is only a format hint.
    pub fn play(&self, id: TrackId, source: Box<dyn MediaSource>, extension: Option<&str>) {
        let _ = self.cmds.send(Cmd::Play(id, source, extension.map(str::to_string)));
    }

    /// Ends the current track now; the next queued one (if any) takes over at once.
    pub fn skip(&self) {
        let _ = self.cmds.send(Cmd::Skip);
    }

    /// Drops one track, playing or queued.
    pub fn cancel(&self, id: TrackId) {
        let _ = self.cmds.send(Cmd::Cancel(id));
    }
}

async fn run(hub: Arc<StreamHub>, bitrate_bps: i32, mut cmds: UnboundedReceiver<Cmd>, events: UnboundedSender<Event>) {
    let mut pipe = Pipeline::default();
    let mut session: Option<Session> = None;
    let mut listeners = hub.listeners();
    // A listener may have connected before this task first ran: treat the current count as news.
    listeners.mark_changed();
    let mut tick = interval(BLOCK_PERIOD);
    // After a long stall (laptop sleep) don't burst-catch-up: just carry on from here.
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut was_active = false;
    let mut block = vec![0i16; BLOCK_SAMPLES];
    let mut pending_events = Vec::new();

    loop {
        let active = pipe.has_tracks() || session.is_some();
        if active && !was_active {
            tick.reset();
        }
        was_active = active;

        tokio::select! {
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    Cmd::Play(id, source, ext) => pipe.enqueue(spawn_feed(id, source, ext)),
                    Cmd::Skip => pipe.skip(&mut pending_events),
                    Cmd::Cancel(id) => pipe.cancel(id, &mut pending_events),
                }
            }
            changed = listeners.changed() => {
                if changed.is_err() { break }
                let count = *listeners.borrow_and_update();
                if count > 0 && session.is_none() {
                    match Session::start(bitrate_bps, &hub) {
                        Ok(s) => session = Some(s),
                        Err(e) => crate::error!("cannot start the Opus encoder: {e}"),
                    }
                } else if count == 0 {
                    session = None;
                }
            }
            _ = tick.tick(), if active => {
                pipe.fill(&mut block, &mut pending_events);
                if let Some(s) = session.as_mut() {
                    s.encode_block(&block, &hub);
                }
            }
        }
        for event in pending_events.drain(..) {
            if events.send(event).is_err() {
                // Nobody is listening for events any more; keep streaming regardless.
                crate::debug!("engine event receiver dropped");
            }
        }
    }
}

/// Used by tests and tools to build a source from bytes already in memory.
pub fn memory_source(bytes: Vec<u8>) -> Box<dyn MediaSource> {
    Box::new(std::io::Cursor::new(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    /// A feed whose blocks the test controls directly.
    fn manual_feed(id: TrackId) -> (Feed, SyncSender<Msg>) {
        let (tx, rx) = sync_channel(64);
        (Feed::new(id, rx), tx)
    }

    fn block(value: i16, samples: usize) -> Msg {
        Msg::Block(vec![value; samples])
    }

    fn fill(pipe: &mut Pipeline, samples: usize) -> (Vec<i16>, Vec<Event>) {
        let (mut out, mut events) = (vec![7i16; samples], Vec::new());
        pipe.fill(&mut out, &mut events);
        (out, events)
    }

    #[test]
    fn an_empty_pipeline_is_silence_with_no_events() {
        let mut pipe = Pipeline::default();
        let (out, events) = fill(&mut pipe, 100);
        assert!(out.iter().all(|&s| s == 0) && events.is_empty());
        assert!(!pipe.has_tracks());
    }

    #[test]
    fn tracks_join_seamlessly_inside_one_block() {
        let mut pipe = Pipeline::default();
        let (a, atx) = manual_feed(1);
        let (b, btx) = manual_feed(2);
        pipe.enqueue(a);
        pipe.enqueue(b);
        atx.send(block(1, 60)).unwrap();
        atx.send(Msg::End).unwrap();
        btx.send(block(2, 100)).unwrap();
        let (out, events) = fill(&mut pipe, 100);
        assert_eq!(&out[..60], &[1; 60][..]);
        assert_eq!(&out[60..], &[2; 40][..], "B must start in the same block, with no silence between");
        assert_eq!(events, [Event::Started(1), Event::Ended(1, Outcome::Finished), Event::Started(2)]);
        // The rest of B's data carries into the next block.
        let (out, _) = fill(&mut pipe, 40);
        assert_eq!(out, vec![2; 40]);
    }

    #[test]
    fn a_track_that_ends_exactly_on_a_block_boundary_finishes_cleanly() {
        let mut pipe = Pipeline::default();
        let (a, atx) = manual_feed(1);
        pipe.enqueue(a);
        atx.send(block(5, 100)).unwrap();
        atx.send(Msg::End).unwrap();
        let (out, events) = fill(&mut pipe, 100);
        assert_eq!(out, vec![5; 100]);
        assert_eq!(events, [Event::Started(1), Event::Ended(1, Outcome::Finished)]);
        assert!(!pipe.has_tracks());
    }

    #[test]
    fn an_underrun_plays_silence_without_ending_the_track() {
        let mut pipe = Pipeline::default();
        let (a, atx) = manual_feed(1);
        pipe.enqueue(a);
        atx.send(block(3, 40)).unwrap();
        let (out, events) = fill(&mut pipe, 100);
        assert_eq!(&out[..40], &[3; 40][..]);
        assert!(out[40..].iter().all(|&s| s == 0));
        assert_eq!(events, [Event::Started(1)]);
        // Data arrives later: playback resumes.
        atx.send(block(4, 100)).unwrap();
        let (out, events) = fill(&mut pipe, 100);
        assert_eq!(out, vec![4; 100]);
        assert!(events.is_empty());
    }

    #[test]
    fn a_stalled_track_fails_after_twenty_seconds_and_the_next_one_takes_over() {
        let mut pipe = Pipeline::default();
        let (a, _atx) = manual_feed(1); // never delivers anything
        let (b, btx) = manual_feed(2);
        pipe.enqueue(a);
        pipe.enqueue(b);
        btx.send(block(9, 100_000)).unwrap();
        let mut failed_at = None;
        for tick in 0..250 {
            let (out, events) = fill(&mut pipe, BLOCK_SAMPLES);
            if let Some(Event::Ended(1, Outcome::Failed(why))) = events.first() {
                assert!(why.contains("stalled"), "{why}");
                failed_at = Some(tick);
                assert_eq!(out[BLOCK_SAMPLES - 1], 9, "the next track should already be playing in the same block");
                break;
            }
        }
        let tick = failed_at.expect("the stalled track should have been dropped");
        assert!((199..=201).contains(&tick), "after {tick} ticks (100 ms each)");
    }

    #[test]
    fn decoder_errors_and_vanished_decoders_end_the_track_as_failed() {
        let mut pipe = Pipeline::default();
        let (a, atx) = manual_feed(1);
        pipe.enqueue(a);
        atx.send(block(1, 10)).unwrap();
        atx.send(Msg::Error("boom".into())).unwrap();
        let (out, events) = fill(&mut pipe, 20);
        assert_eq!(&out[..10], &[1; 10][..]);
        assert_eq!(events, [Event::Started(1), Event::Ended(1, Outcome::Failed("boom".into()))]);

        let (b, btx) = manual_feed(2);
        pipe.enqueue(b);
        drop(btx); // thread died without saying why
        let (_, events) = fill(&mut pipe, 20);
        assert!(matches!(&events[..], [Event::Ended(2, Outcome::Failed(_))]), "{events:?}");
    }

    #[test]
    fn skip_and_cancel_end_tracks_as_skipped() {
        let mut pipe = Pipeline::default();
        let (a, atx) = manual_feed(1);
        let (b, _btx) = manual_feed(2);
        let (c, _ctx) = manual_feed(3);
        pipe.enqueue(a);
        pipe.enqueue(b);
        pipe.enqueue(c);
        atx.send(block(1, 50)).unwrap();
        fill(&mut pipe, 10);
        let mut events = Vec::new();
        pipe.cancel(3, &mut events);
        pipe.skip(&mut events);
        pipe.cancel(99, &mut events); // unknown id: nothing happens
        assert_eq!(events, [Event::Ended(3, Outcome::Skipped), Event::Ended(1, Outcome::Skipped)]);
        assert!(pipe.has_tracks(), "B is still queued");
    }

    #[test]
    fn a_dropped_feed_lets_its_decoder_thread_exit() {
        let (tx, rx) = sync_channel::<Msg>(1);
        tx.send(Msg::End).unwrap();
        drop(rx);
        assert!(tx.send(Msg::End).is_err(), "send must fail once the engine drops the feed");
    }
}
