//! A seekable, self-healing byte source over HTTP(S) for the audio decoder.
//!
//! It reads the file in bounded `Range` requests (4 MiB each, the way yt-dlp
//! itself downloads from YouTube to avoid throttling), reconnects on dropped
//! connections and transient server errors, and turns small forward seeks
//! into plain reads. A server that ignores `Range` is handled too, just
//! without seeking.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::Duration;

use symphonia::core::io::MediaSource;

use super::http::{Client, HttpError, Request, Response};

/// Size of one range request.
const CHUNK: u64 = 4 * 1024 * 1024;
/// A forward seek shorter than this is done by reading and discarding.
const SKIP_LIMIT: u64 = 256 * 1024;
/// Consecutive failures tolerated before giving up (any progress resets the count).
const MAX_RETRIES: usize = 3;
const RETRY_DELAYS: [Duration; MAX_RETRIES] = [Duration::from_millis(500), Duration::from_secs(1), Duration::from_secs(2)];

struct Open {
    body: Response,
    /// Offset of the next byte `body` will deliver.
    pos: u64,
    /// Offset one past the last byte this response will deliver.
    end: u64,
}

pub struct HttpSource {
    client: Arc<Client>,
    url: String,
    headers: Vec<(String, String)>,
    len: Option<u64>,
    seekable: bool,
    pos: u64,
    open: Option<Open>,
    retries: usize,
    delays: [Duration; MAX_RETRIES],
}

fn to_io(e: HttpError) -> io::Error {
    io::Error::other(e)
}

/// `bytes 0-99/1234` -> (0, 99, Some(1234)); a total of `*` gives None.
fn parse_content_range(v: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = v.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = rest.split_once('/')?;
    let (a, b) = range.split_once('-')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?, total.trim().parse().ok()))
}

impl HttpSource {
    /// Connects and fetches the first range, so a bad URL fails here, not mid-decode.
    /// `headers` are the ones yt-dlp says the media server expects (User-Agent etc.).
    pub fn open(client: Arc<Client>, url: &str, headers: &[(String, String)]) -> Result<HttpSource, HttpError> {
        let mut src = HttpSource { client, url: url.to_string(), headers: headers.to_vec(), len: None, seekable: false, pos: 0, open: None, retries: 0, delays: RETRY_DELAYS };
        let mut attempt = 0;
        loop {
            match src.open_at(0) {
                Ok(()) => return Ok(src),
                Err(e) if e.is_retryable() && attempt < MAX_RETRIES => {
                    std::thread::sleep(src.delays[attempt]);
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Shortens the pauses between retries (tests only).
    pub fn with_fast_retries(mut self) -> HttpSource {
        self.delays = [Duration::from_millis(5); MAX_RETRIES];
        self
    }

    pub fn len(&self) -> Option<u64> {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == Some(0)
    }

    fn open_at(&mut self, start: u64) -> Result<(), HttpError> {
        self.open = None;
        if self.len.is_some_and(|l| start >= l) {
            return Ok(());
        }
        let end = match self.len {
            Some(l) => (start + CHUNK).min(l),
            None => start + CHUNK,
        };
        let request = Request::get(&self.url).headers(&self.headers).header("Range", &format!("bytes={start}-{}", end - 1));
        let mut resp = self.client.send(&request)?;
        match resp.status {
            206 => {
                let (a, b, total) = resp
                    .header("content-range")
                    .and_then(parse_content_range)
                    .ok_or_else(|| HttpError::Protocol("206 response without a usable Content-Range".into()))?;
                if a != start {
                    return Err(HttpError::Protocol(format!("asked for bytes from {start}, server started at {a}")));
                }
                if total.is_some() {
                    self.len = total;
                }
                self.seekable = true;
                self.open = Some(Open { body: resp, pos: start, end: b + 1 });
            }
            200 => {
                // The server ignored Range and is sending the whole file.
                let len = resp.header("content-length").and_then(|v| v.parse::<u64>().ok());
                if len.is_some() {
                    self.len = len;
                }
                self.seekable = false;
                if start > 0 {
                    let skipped = io::copy(&mut (&mut resp).take(start), &mut io::sink()).map_err(|e| HttpError::Io(e.to_string()))?;
                    if skipped < start {
                        return Err(HttpError::Protocol("the response ended before the requested offset".into()));
                    }
                }
                self.open = Some(Open { body: resp, pos: start, end: len.unwrap_or(u64::MAX) });
            }
            416 => {
                // Range not satisfiable: we are at or past the end.
                self.len = resp.header("content-range").and_then(|v| v.trim().strip_prefix("bytes */")?.parse().ok()).or(Some(start));
            }
            other => return Err(HttpError::Status(other)),
        }
        Ok(())
    }

    /// Reuses the open response when `pos` is inside it (skipping ahead if needed), else drops it.
    fn align(&mut self) {
        let Some(open) = &mut self.open else { return };
        if self.pos < open.pos || self.pos >= open.end || self.pos - open.pos > SKIP_LIMIT {
            self.open = None;
        } else if self.pos > open.pos {
            let skip = self.pos - open.pos;
            match io::copy(&mut (&mut open.body).take(skip), &mut io::sink()) {
                Ok(n) if n == skip => open.pos = self.pos,
                _ => self.open = None,
            }
        }
    }

    fn back_off(&mut self) -> bool {
        if self.retries >= MAX_RETRIES {
            return false;
        }
        std::thread::sleep(self.delays[self.retries]);
        self.retries += 1;
        true
    }
}

impl Read for HttpSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.len.is_some_and(|l| self.pos >= l) {
                return Ok(0);
            }
            self.align();
            if self.open.is_none() {
                match self.open_at(self.pos) {
                    Ok(()) => {}
                    Err(e) if e.is_retryable() && self.back_off() => continue,
                    Err(e) => return Err(to_io(e)),
                }
                if self.open.is_none() {
                    return Ok(0); // 416: nothing at or beyond this offset
                }
            }
            let Some(open) = self.open.as_mut() else { continue };
            let want = (open.end - open.pos).min(buf.len() as u64) as usize;
            if want == 0 {
                self.open = None; // this range is finished; the next iteration opens the next one
                continue;
            }
            match open.body.read(&mut buf[..want]) {
                Ok(0) if open.end == u64::MAX => {
                    // An unbounded body ended: that is the end of the file.
                    self.len = Some(self.pos);
                    self.open = None;
                    return Ok(0);
                }
                Ok(0) => {
                    self.open = None;
                    if !self.back_off() {
                        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the server closed the connection early"));
                    }
                }
                Ok(n) => {
                    open.pos += n as u64;
                    self.pos += n as u64;
                    self.retries = 0;
                    return Ok(n);
                }
                Err(e) => {
                    self.open = None;
                    if !self.back_off() {
                        return Err(e);
                    }
                }
            }
        }
    }
}

impl Seek for HttpSource {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(p) => i128::from(p),
            SeekFrom::Current(d) => i128::from(self.pos) + i128::from(d),
            SeekFrom::End(d) => match self.len {
                Some(l) => i128::from(l) + i128::from(d),
                None => return Err(io::Error::new(io::ErrorKind::Unsupported, "the length of this stream is unknown")),
            },
        };
        if target < 0 || target > i128::from(u64::MAX) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek outside the stream"));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}

impl MediaSource for HttpSource {
    fn is_seekable(&self) -> bool {
        self.seekable
    }

    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_parsing() {
        assert_eq!(parse_content_range("bytes 0-99/1234"), Some((0, 99, Some(1234))));
        assert_eq!(parse_content_range("bytes 100-199/*"), Some((100, 199, None)));
        assert_eq!(parse_content_range(" bytes  5-6/7 "), Some((5, 6, Some(7))));
        assert_eq!(parse_content_range("items 0-1/2"), None);
        assert_eq!(parse_content_range("bytes */5"), None);
    }
}
