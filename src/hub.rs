//! Fan-out of the encoded Ogg/Opus stream to every connected listener.
//!
//! Each listener gets a bounded queue. A listener that falls behind is
//! dropped rather than buffered forever (OBS reconnects on its own). A
//! listener that joins mid-stream first receives the current session's Ogg
//! header pages (OpusHead + OpusTags), so its decoder starts cleanly.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::{Bytes, BytesMut};
use tokio::sync::{mpsc, watch};

/// ~5-10 s of Opus at typical bitrates; a stalled listener is dropped, not buffered.
pub const SUBSCRIBER_QUEUE: usize = 50;
/// Sanity ceiling on captured header bytes.
const MAX_HEADER_BYTES: usize = 65_536;

// ---------------------------------------------------------------- ogg pages

/// Length of the complete Ogg page at the front of `buf`, if there is one.
/// `Err(())` means the front isn't an Ogg page at all.
pub fn page_len(buf: &[u8]) -> Result<Option<usize>, ()> {
    if buf.len() < 4 {
        return Ok(None);
    }
    if &buf[..4] != b"OggS" {
        return Err(());
    }
    if buf.len() < 27 {
        return Ok(None);
    }
    let segments = usize::from(buf[26]);
    let header = 27 + segments;
    if buf.len() < header {
        return Ok(None);
    }
    let body: usize = buf[27..header].iter().map(|&b| usize::from(b)).sum();
    let total = header + body;
    Ok((buf.len() >= total).then_some(total))
}

pub fn page_granule(page: &[u8]) -> i64 {
    i64::from_le_bytes(page[6..14].try_into().expect("page has a 27-byte header"))
}

/// Collects the leading pages whose granule position is 0 (OpusHead, OpusTags).
#[derive(Default)]
struct HeaderCapture {
    pending: Vec<u8>,
    header: Vec<u8>,
}

impl HeaderCapture {
    /// Feeds more stream bytes. Returns true once the header is complete
    /// (first audio page seen, the size ceiling hit, or the stream isn't Ogg).
    fn feed(&mut self, chunk: &[u8]) -> bool {
        self.pending.extend_from_slice(chunk);
        loop {
            match page_len(&self.pending) {
                Ok(Some(n)) => {
                    if page_granule(&self.pending[..n]) == 0 {
                        self.header.extend_from_slice(&self.pending[..n]);
                        self.pending.drain(..n);
                    } else {
                        return true;
                    }
                }
                Ok(None) => break,
                Err(()) => return true,
            }
        }
        self.header.len() > MAX_HEADER_BYTES
    }
}

// ---------------------------------------------------------------------- hub

struct Sub {
    id: u64,
    tx: mpsc::Sender<Bytes>,
}

#[derive(Default)]
struct Inner {
    subs: Vec<Sub>,
    capture: HeaderCapture,
    header: Option<Bytes>,
    /// Chunks produced while the header was still being captured.
    prehistory: Vec<Bytes>,
}

pub struct StreamHub {
    inner: Mutex<Inner>,
    next_id: AtomicU64,
    listeners: watch::Sender<usize>,
}

impl Default for StreamHub {
    fn default() -> Self {
        StreamHub { inner: Mutex::default(), next_id: AtomicU64::new(1), listeners: watch::channel(0).0 }
    }
}

pub struct Subscription {
    /// Header pages to send before anything from `rx` (empty if the header is
    /// still being captured, in which case `rx` is pre-seeded instead).
    pub header: Bytes,
    pub rx: mpsc::Receiver<Bytes>,
    id: u64,
    hub: Arc<StreamHub>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.hub.unsubscribe(self.id);
    }
}

impl StreamHub {
    pub fn new() -> Arc<StreamHub> {
        Arc::new(StreamHub::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Current listener count; `changed()` fires on every join/leave. The
    /// player starts the encoder on 0 -> 1 and retires it a while after 1 -> 0.
    pub fn listeners(&self) -> watch::Receiver<usize> {
        self.listeners.subscribe()
    }

    pub fn listener_count(&self) -> usize {
        *self.listeners.borrow()
    }

    pub fn subscribe(self: &Arc<Self>) -> Subscription {
        let (tx, rx) = mpsc::channel(SUBSCRIBER_QUEUE);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (header, count) = {
            let mut inner = self.lock();
            let header = match &inner.header {
                Some(h) => h.clone(),
                None => {
                    for chunk in &inner.prehistory {
                        let _ = tx.try_send(chunk.clone());
                    }
                    Bytes::new()
                }
            };
            inner.subs.push(Sub { id, tx });
            (header, inner.subs.len())
        };
        self.listeners.send_replace(count);
        Subscription { header, rx, id, hub: Arc::clone(self) }
    }

    fn unsubscribe(&self, id: u64) {
        let count = {
            let mut inner = self.lock();
            inner.subs.retain(|s| s.id != id);
            inner.subs.len()
        };
        self.listeners.send_replace(count);
    }

    /// A new encoder session starts a new logical bitstream: forget the old header.
    pub fn begin_session(&self) {
        let mut inner = self.lock();
        inner.capture = HeaderCapture::default();
        inner.header = None;
        inner.prehistory.clear();
    }

    /// Sends one chunk of encoder output to every listener.
    pub fn publish(&self, chunk: Bytes) {
        let (dropped, count) = {
            let mut inner = self.lock();
            if inner.header.is_none() {
                if inner.capture.feed(&chunk) {
                    let header = BytesMut::from(&inner.capture.header[..]).freeze();
                    inner.header = Some(header);
                    inner.prehistory.clear();
                    inner.capture = HeaderCapture::default();
                } else {
                    inner.prehistory.push(chunk.clone());
                }
            }
            let before = inner.subs.len();
            // Full (stalled) or closed (gone): drop the sender, which ends that listener's stream.
            inner.subs.retain(|s| s.tx.try_send(chunk.clone()).is_ok());
            (before != inner.subs.len(), inner.subs.len())
        };
        if dropped {
            self.listeners.send_replace(count);
        }
    }

    /// The current header pages, if captured (used by tests and diagnostics).
    pub fn header_snapshot(&self) -> Option<Bytes> {
        self.lock().header.clone()
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    /// Builds a valid Ogg page with a single segment-run body.
    pub fn page(granule: i64, serial: u32, seq: u32, flags: u8, body: &[u8]) -> Vec<u8> {
        let mut segs = Vec::new();
        let mut left = body.len();
        while left >= 255 {
            segs.push(255u8);
            left -= 255;
        }
        segs.push(left as u8);
        let mut out = Vec::new();
        out.extend_from_slice(b"OggS");
        out.push(0);
        out.push(flags);
        out.extend_from_slice(&granule.to_le_bytes());
        out.extend_from_slice(&serial.to_le_bytes());
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]); // CRC: not validated by the hub
        out.push(segs.len() as u8);
        out.extend_from_slice(&segs);
        out.extend_from_slice(body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::page;
    use super::*;

    #[test]
    fn page_len_handles_partial_complete_and_foreign_data() {
        let p = page(0, 1, 0, 2, &[7u8; 300]); // 300 bytes -> segments 255 + 45
        assert_eq!(page_len(&p), Ok(Some(p.len())));
        assert_eq!(page_len(&p[..p.len() - 1]), Ok(None));
        assert_eq!(page_len(&p[..3]), Ok(None));
        assert_eq!(page_len(&p[..30]), Ok(None));
        assert_eq!(page_len(b"RIFFxxxx"), Err(()));
        // A body that is an exact multiple of 255 ends with a zero-length segment.
        let q = page(0, 1, 0, 0, &[1u8; 255]);
        assert_eq!(page_len(&q), Ok(Some(q.len())));
    }

    #[test]
    fn header_capture_stops_at_the_first_audio_page_even_across_odd_chunk_boundaries() {
        let head = page(0, 9, 0, 2, b"OpusHead........");
        let tags = page(0, 9, 1, 0, b"OpusTags");
        let audio = page(960, 9, 2, 0, &[3u8; 40]);
        let mut stream = head.clone();
        stream.extend(&tags);
        stream.extend(&audio);
        let mut cap = HeaderCapture::default();
        let mut done = false;
        for chunk in stream.chunks(7) {
            done = cap.feed(chunk);
            if done {
                break;
            }
        }
        assert!(done);
        let mut want = head;
        want.extend(&tags);
        assert_eq!(cap.header, want);
    }

    #[tokio::test]
    async fn late_listener_gets_the_header_first_then_live_chunks() {
        let hub = StreamHub::new();
        let head = page(0, 1, 0, 2, b"OpusHead........");
        let tags = page(0, 1, 1, 0, b"OpusTags");
        let audio1 = page(960, 1, 2, 0, &[1u8; 20]);
        let audio2 = page(1920, 1, 3, 0, &[2u8; 20]);
        hub.begin_session();
        hub.publish(Bytes::from([head.clone(), tags.clone()].concat()));
        hub.publish(Bytes::from(audio1.clone()));
        assert!(hub.header_snapshot().is_some());

        let mut late = hub.subscribe();
        assert_eq!(&late.header[..], &[head, tags].concat()[..]);
        hub.publish(Bytes::from(audio2.clone()));
        assert_eq!(&late.rx.recv().await.unwrap()[..], &audio2[..]);
    }

    #[tokio::test]
    async fn listener_joining_during_capture_is_seeded_with_what_came_before() {
        let hub = StreamHub::new();
        let head = page(0, 1, 0, 2, b"OpusHead........");
        hub.begin_session();
        hub.publish(Bytes::from(head.clone())); // header incomplete: no audio page yet
        let mut sub = hub.subscribe();
        assert!(sub.header.is_empty());
        assert_eq!(&sub.rx.recv().await.unwrap()[..], &head[..]);
    }

    #[tokio::test]
    async fn stalled_listener_is_dropped_and_others_keep_flowing() {
        let hub = StreamHub::new();
        hub.begin_session();
        let mut stalled = hub.subscribe();
        let mut healthy = hub.subscribe();
        assert_eq!(hub.listener_count(), 2);
        for i in 0..(SUBSCRIBER_QUEUE + 5) {
            hub.publish(Bytes::from(vec![i as u8; 4]));
            while healthy.rx.try_recv().is_ok() {}
        }
        assert_eq!(hub.listener_count(), 1, "stalled listener should have been evicted");
        // Its queue still drains, then ends instead of hanging forever.
        let mut n = 0;
        while stalled.rx.recv().await.is_some() {
            n += 1;
        }
        assert_eq!(n, SUBSCRIBER_QUEUE);
    }

    #[tokio::test]
    async fn dropping_a_subscription_updates_the_listener_count() {
        let hub = StreamHub::new();
        let mut watch = hub.listeners();
        let a = hub.subscribe();
        watch.changed().await.unwrap();
        assert_eq!(*watch.borrow_and_update(), 1);
        drop(a);
        watch.changed().await.unwrap();
        assert_eq!(*watch.borrow_and_update(), 0);
    }

    #[test]
    fn a_new_session_forgets_the_old_header() {
        let hub = StreamHub::new();
        hub.begin_session();
        hub.publish(Bytes::from([page(0, 1, 0, 2, b"h"), page(5, 1, 1, 0, b"a")].concat()));
        assert!(hub.header_snapshot().is_some());
        hub.begin_session();
        assert!(hub.header_snapshot().is_none());
    }
}
