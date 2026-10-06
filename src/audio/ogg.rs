//! A minimal Ogg muxer for one never-ending Opus stream (RFC 3533 / RFC 7845),
//! plus a small packet reader used by tests and tools.
//!
//! Audio pages carry whole packets only, five 20 ms packets (100 ms) per
//! page: small enough for low latency, large enough that page overhead is
//! about 1.5 % of the bitrate.

pub const PACKETS_PER_PAGE: usize = 5;
const MAX_SEGMENTS: usize = 255;
const FLAG_BOS: u8 = 0x02;
const VENDOR: &[u8] = b"Pryxea";

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut r = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            r = if r & 0x8000_0000 != 0 { (r << 1) ^ 0x04C1_1DB7 } else { r << 1 };
            bit += 1;
        }
        table[i] = r;
        i += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = crc_table();

/// Ogg's CRC-32: polynomial 0x04C11DB7, no reflection, zero init, no final xor.
pub fn crc32(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |crc, &b| (crc << 8) ^ CRC_TABLE[usize::from((crc >> 24) as u8 ^ b)])
}

/// Builds one Ogg page holding the given packets, each ending on this page.
fn build_page(out: &mut Vec<u8>, flags: u8, granule: i64, serial: u32, seq: u32, packets: &[&[u8]]) {
    let start = out.len();
    out.extend_from_slice(b"OggS");
    out.push(0); // stream structure version
    out.push(flags);
    out.extend_from_slice(&granule.to_le_bytes());
    out.extend_from_slice(&serial.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&[0; 4]); // CRC, filled in below
    let seg_count_at = out.len();
    out.push(0);
    let mut segments = 0usize;
    for p in packets {
        for _ in 0..p.len() / 255 {
            out.push(255);
            segments += 1;
        }
        out.push((p.len() % 255) as u8);
        segments += 1;
    }
    debug_assert!(segments <= MAX_SEGMENTS);
    out[seg_count_at] = segments as u8;
    for p in packets {
        out.extend_from_slice(p);
    }
    let crc = crc32(&out[start..]);
    out[start + 22..start + 26].copy_from_slice(&crc.to_le_bytes());
}

fn segments_for(len: usize) -> usize {
    len / 255 + 1
}

pub struct OpusOggWriter {
    serial: u32,
    seq: u32,
    pre_skip: u16,
    /// 48 kHz samples encoded so far, excluding pre-skip.
    samples: u64,
    pending: Vec<u8>,
    pending_lens: Vec<usize>,
    pending_segments: usize,
}

impl OpusOggWriter {
    pub fn new(serial: u32, pre_skip: u16) -> OpusOggWriter {
        OpusOggWriter { serial, seq: 0, pre_skip, samples: 0, pending: Vec::new(), pending_lens: Vec::new(), pending_segments: 0 }
    }

    /// The two header pages (OpusHead, OpusTags), as one buffer. Both have
    /// granule position 0, which is how the stream hub recognises them.
    pub fn header_pages(&mut self) -> Vec<u8> {
        let mut head = Vec::with_capacity(19);
        head.extend_from_slice(b"OpusHead");
        head.push(1); // version
        head.push(2); // channels
        head.extend_from_slice(&self.pre_skip.to_le_bytes());
        head.extend_from_slice(&48_000u32.to_le_bytes()); // original input rate
        head.extend_from_slice(&0i16.to_le_bytes()); // output gain
        head.push(0); // channel mapping family 0

        let mut tags = Vec::new();
        tags.extend_from_slice(b"OpusTags");
        tags.extend_from_slice(&(VENDOR.len() as u32).to_le_bytes());
        tags.extend_from_slice(VENDOR);
        tags.extend_from_slice(&0u32.to_le_bytes()); // no user comments

        let mut out = Vec::with_capacity(head.len() + tags.len() + 64);
        build_page(&mut out, FLAG_BOS, 0, self.serial, self.seq, &[&head]);
        self.seq += 1;
        build_page(&mut out, 0, 0, self.serial, self.seq, &[&tags]);
        self.seq += 1;
        out
    }

    /// Adds one packet covering `frames` samples at 48 kHz. Returns a finished
    /// page whenever one fills up.
    pub fn push_packet(&mut self, packet: &[u8], frames: u64) -> Option<Vec<u8>> {
        let mut finished = None;
        // A page holds at most 255 segments; flush first if this packet wouldn't fit.
        if self.pending_segments + segments_for(packet.len()) > MAX_SEGMENTS {
            finished = self.flush();
        }
        self.pending.extend_from_slice(packet);
        self.pending_lens.push(packet.len());
        self.pending_segments += segments_for(packet.len());
        self.samples += frames;
        if self.pending_lens.len() >= PACKETS_PER_PAGE {
            // (finished is None here unless the flush above happened, in which case pending was empty again)
            let page = self.flush();
            return match (finished, page) {
                (Some(mut a), Some(b)) => {
                    a.extend_from_slice(&b);
                    Some(a)
                }
                (a, b) => a.or(b),
            };
        }
        finished
    }

    fn flush(&mut self) -> Option<Vec<u8>> {
        if self.pending_lens.is_empty() {
            return None;
        }
        // Granule of the *page* is the position after its last packet; the flushed-early page
        // must not include the packet that triggered the flush, so account for it only on push.
        let mut packets: Vec<&[u8]> = Vec::with_capacity(self.pending_lens.len());
        let mut at = 0;
        for &len in &self.pending_lens {
            packets.push(&self.pending[at..at + len]);
            at += len;
        }
        let granule = i64::try_from(self.samples + u64::from(self.pre_skip)).unwrap_or(i64::MAX);
        let mut out = Vec::with_capacity(27 + self.pending_segments + self.pending.len());
        build_page(&mut out, 0, granule, self.serial, self.seq, &packets);
        self.seq += 1;
        self.pending.clear();
        self.pending_lens.clear();
        self.pending_segments = 0;
        Some(out)
    }
}

// ------------------------------------------------------------------- reader

/// Splits an Ogg stream (complete pages only) into packets. Returns
/// `(granule, packet)` pairs, where granule is that of the page the packet ends on.
/// Panics on malformed input; meant for tests and tooling, not for untrusted data.
pub fn packets(stream: &[u8]) -> Vec<(i64, Vec<u8>)> {
    let mut out = Vec::new();
    let mut partial: Vec<u8> = Vec::new();
    let mut at = 0;
    while at < stream.len() {
        let page = &stream[at..];
        assert_eq!(&page[..4], b"OggS", "not at a page boundary (offset {at})");
        let segs = usize::from(page[26]);
        let table = &page[27..27 + segs];
        let granule = i64::from_le_bytes(page[6..14].try_into().unwrap());
        let mut body = 27 + segs;
        for &seg in table {
            partial.extend_from_slice(&page[body..body + usize::from(seg)]);
            body += usize::from(seg);
            if seg < 255 {
                out.push((granule, std::mem::take(&mut partial)));
            }
        }
        at += body;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// First page of a file written by ffmpeg's libopus + ogg muxer (OpusHead).
    const FFMPEG_HEAD_PAGE: [u8; 47] = [
        0x4f, 0x67, 0x67, 0x53, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc4, 0x0b, 0xc3, 0x16, 0x00, 0x00, 0x00, 0x00,
        0x52, 0xc3, 0x8a, 0xc5, 0x01, 0x13, 0x4f, 0x70, 0x75, 0x73, 0x48, 0x65, 0x61, 0x64, 0x01, 0x02, 0x38, 0x01, 0x80, 0xbb, 0x00, 0x00,
        0x00, 0x00, 0x00,
    ];

    #[test]
    fn crc_matches_a_page_written_by_ffmpeg() {
        let mut page = FFMPEG_HEAD_PAGE;
        let stored = u32::from_le_bytes(page[22..26].try_into().unwrap());
        page[22..26].copy_from_slice(&[0; 4]);
        assert_eq!(crc32(&page), stored);
    }

    #[test]
    fn our_head_page_is_byte_identical_to_ffmpegs_apart_from_serial_and_crc() {
        // ffmpeg's file used pre-skip 312 (0x0138) and a 48000 Hz input rate, like ours.
        let mut w = OpusOggWriter::new(0x16c3_0bc4, 312);
        let pages = w.header_pages();
        assert_eq!(&pages[..47], &FFMPEG_HEAD_PAGE[..]);
    }

    #[test]
    fn pages_hold_five_packets_with_correct_granules_and_lacing() {
        let mut w = OpusOggWriter::new(7, 312);
        let header = w.header_pages();
        let mut stream = header.clone();
        let mut sent = Vec::new();
        for i in 0..12u32 {
            // Sizes include 0, 254, 255, 256 and 600 to exercise lacing edge cases.
            let len = [0usize, 254, 255, 256, 600, 1, 300][(i % 7) as usize];
            let packet = vec![(i + 1) as u8; len];
            if let Some(page) = w.push_packet(&packet, 960) {
                stream.extend_from_slice(&page);
            }
            sent.push(packet);
        }
        let got = packets(&stream);
        assert_eq!(&got[0].1[..8], b"OpusHead");
        assert_eq!(&got[1].1[..8], b"OpusTags");
        // Two full pages of five; the last two packets are still pending.
        assert_eq!(got.len(), 2 + 10);
        for (i, (_, p)) in got[2..].iter().enumerate() {
            assert_eq!(p, &sent[i], "packet {i}");
        }
        assert_eq!(got[2].0, 312 + 5 * 960);
        assert_eq!(got[11].0, 312 + 10 * 960);
        assert_eq!((got[0].0, got[1].0), (0, 0));
    }

    #[test]
    fn a_page_never_exceeds_255_segments() {
        let mut w = OpusOggWriter::new(1, 0);
        let mut stream = w.header_pages();
        // Huge packets (far above real Opus sizes) force early flushes.
        for _ in 0..8 {
            if let Some(p) = w.push_packet(&vec![9u8; 20_000], 960) {
                stream.extend_from_slice(&p);
            }
        }
        let mut at = 0;
        while at < stream.len() {
            let segs = usize::from(stream[at + 26]);
            assert!(segs <= 255);
            at += 27 + segs + stream[at + 27..at + 27 + segs].iter().map(|&s| usize::from(s)).sum::<usize>();
        }
        assert_eq!(at, stream.len());
    }

    #[test]
    fn header_pages_are_recognised_by_the_stream_hub() {
        let mut w = OpusOggWriter::new(5, 312);
        let hub = crate::hub::StreamHub::new();
        hub.begin_session();
        hub.publish(bytes::Bytes::from(w.header_pages()));
        let mut audio = None;
        for _ in 0..5 {
            audio = w.push_packet(&[1, 2, 3], 960).or(audio);
        }
        hub.publish(bytes::Bytes::from(audio.unwrap()));
        let header = hub.header_snapshot().expect("header captured at the first audio page");
        assert_eq!(packets(&header).len(), 2);
    }
}
