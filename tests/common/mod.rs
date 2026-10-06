//! A tiny HTTP/HTTPS test server (blocking, one thread per connection) with
//! the awkward behaviours the client has to survive: Range support, servers
//! that ignore Range, connections dropped mid-body, transient 503s,
//! redirects, chunked bodies.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// What the fake release server serves for every tool asset: a script that reports a version.
pub const TOOL_PAYLOAD: &[u8] = b"#!/bin/sh\necho 2026.09.01\n";
pub const TOOL_ASSETS: [&str; 8] = ["yt-dlp.exe", "yt-dlp_arm64.exe", "yt-dlp_x86.exe", "yt-dlp_linux", "yt-dlp_linux_aarch64", "yt-dlp_macos", "yt-dlp", "qjs-test"];

pub fn tool_sha() -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(TOOL_PAYLOAD).iter().map(|b| format!("{b:02x}")).collect()
}

pub const BIG_LEN: usize = 10 * 1024 * 1024;
pub const SMALL_LEN: usize = 300_000;

/// Deterministic, non-repeating-looking content.
pub fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 7) ^ (i / 251) ^ (i >> 13)) as u8).collect()
}

trait Io: Read + Write {}
impl<T: Read + Write> Io for T {}

pub struct Server {
    pub addr: SocketAddr,
    pub https: bool,
    /// One entry per request: "METHOD /path [Range]".
    pub log: Arc<Mutex<Vec<String>>>,
    pub client_tls: Arc<rustls::ClientConfig>,
}

impl Server {
    pub fn url(&self, path: &str) -> String {
        format!("{}://localhost:{}{path}", if self.https { "https" } else { "http" }, self.addr.port())
    }
    pub fn url_ip(&self, path: &str) -> String {
        format!("{}://127.0.0.1:{}{path}", if self.https { "https" } else { "http" }, self.addr.port())
    }
    pub fn requests(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
    pub fn count(&self, prefix: &str) -> usize {
        self.requests().iter().filter(|r| r.starts_with(prefix)).count()
    }
}

fn fixture_path(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

pub fn spawn(https: bool) -> Server {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));

    let cert_bytes = std::fs::read(fixture_path("test-cert.pem")).unwrap();
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_bytes).collect::<Result<_, _>>().unwrap();
    let key = PrivateKeyDer::from_pem_slice(&std::fs::read(fixture_path("test-key.pem")).unwrap()).unwrap();
    let server_cfg = Arc::new(rustls::ServerConfig::builder().with_no_client_auth().with_single_cert(certs.clone(), key).unwrap());
    // Clients trust the test CA; the server presents a leaf signed by it.
    let ca_bytes = std::fs::read(fixture_path("test-ca.pem")).unwrap();
    let ca: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&ca_bytes).collect::<Result<_, _>>().unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca[0].clone()).unwrap();
    let client_tls = Arc::new(rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth());

    let big = Arc::new(pattern(BIG_LEN));
    let small = Arc::new(pattern(SMALL_LEN));
    let counters: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
    let live = Arc::new(AtomicUsize::new(0));
    let thread_log = log.clone();
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let (log, big, small, counters, cfg, live) = (thread_log.clone(), big.clone(), small.clone(), counters.clone(), server_cfg.clone(), live.clone());
            thread::spawn(move || {
                live.fetch_add(1, Ordering::Relaxed);
                let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(10)));
                let mut io: Box<dyn Io> = if https {
                    let sc = rustls::ServerConnection::new(cfg).unwrap();
                    Box::new(rustls::StreamOwned::new(sc, conn))
                } else {
                    Box::new(conn)
                };
                let _ = handle(&mut *io, &log, &big, &small, &counters);
                let _ = io.flush();
                live.fetch_sub(1, Ordering::Relaxed);
            });
        }
    });
    Server { addr, https, log, client_tls }
}

fn read_request(io: &mut dyn Io) -> std::io::Result<(String, String, Vec<(String, String)>, Vec<u8>)> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if io.read(&mut byte)? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head).to_string();
    let mut lines = text.lines();
    let mut first = lines.next().unwrap_or("").split(' ');
    let (method, path) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
    let len: usize = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0u8; len];
    io.read_exact(&mut body)?;
    Ok((method, path, headers, body))
}

fn respond(io: &mut dyn Io, status: &str, headers: &[(&str, String)], body: &[u8]) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status}\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()));
    io.write_all(head.as_bytes())?;
    io.write_all(body)
}

fn parse_range(v: &str, len: usize) -> Option<(usize, usize)> {
    let spec = v.strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    let start: usize = a.parse().ok()?;
    let end = if b.is_empty() { len - 1 } else { b.parse::<usize>().ok()?.min(len - 1) };
    Some((start, end))
}

fn handle(io: &mut dyn Io, log: &Mutex<Vec<String>>, big: &[u8], small: &[u8], counters: &Mutex<HashMap<String, usize>>) -> std::io::Result<()> {
    let (method, path, headers, body) = read_request(io)?;
    let get = |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone());
    let range = get("range");
    log.lock().unwrap().push(format!("{method} {path} {}", range.clone().unwrap_or_default()).trim_end().to_string());
    let nth = {
        let mut c = counters.lock().unwrap();
        let n = c.entry(path.clone()).or_insert(0);
        *n += 1;
        *n
    };

    // Serves `data` honouring Range. `abort_after` cuts the body short (then hangs up) when set.
    let serve_ranged = |io: &mut dyn Io, data: &[u8], honour_range: bool, abort_after: Option<usize>| -> std::io::Result<()> {
        let (status, start, end) = match (honour_range, range.as_deref().and_then(|r| parse_range(r, data.len()))) {
            (true, Some((s, _))) if s >= data.len() => {
                return respond(io, "416 Range Not Satisfiable", &[("Content-Range", format!("bytes */{}", data.len()))], b"");
            }
            (true, Some((s, e))) => ("206 Partial Content", s, e),
            _ => ("200 OK", 0, data.len() - 1),
        };
        let slice = &data[start..=end];
        let mut hdrs = vec![("Content-Type", "application/octet-stream".to_string())];
        if status.starts_with("206") {
            hdrs.push(("Content-Range", format!("bytes {start}-{end}/{}", data.len())));
        }
        match abort_after {
            Some(n) => {
                let mut head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", slice.len());
                for (k, v) in &hdrs {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("Connection: close\r\n\r\n");
                io.write_all(head.as_bytes())?;
                io.write_all(&slice[..n.min(slice.len())])?;
                io.flush()?;
                Err(std::io::ErrorKind::ConnectionAborted.into()) // drop the connection mid-body
            }
            None => respond(io, status, &hdrs, slice),
        }
    };

    match path.as_str() {
        "/file" => serve_ranged(io, big, true, None),
        "/small" => serve_ranged(io, small, true, None),
        p if p.starts_with("/media/") => match std::fs::read(fixture_path(&p["/media/".len()..])) {
            Ok(data) => serve_ranged(io, &data, true, None),
            Err(_) => respond(io, "404 Not Found", &[], b"no such fixture"),
        },
        "/norange" => serve_ranged(io, small, false, None),
        // The first two requests die after 100 KB of body; later ones are fine.
        "/flaky" if nth <= 2 => serve_ranged(io, big, true, Some(100_000)),
        "/flaky" => serve_ranged(io, big, true, None),
        "/status/403" => respond(io, "403 Forbidden", &[], b"nope"),
        "/status/404" => respond(io, "404 Not Found", &[], b"missing"),
        "/transient" if nth == 1 => respond(io, "503 Service Unavailable", &[], b"try later"),
        "/transient" => serve_ranged(io, small, true, None),
        "/tools/latest" => respond(io, "302 Found", &[("Location", "/tools/tag/2026.09.01".to_string())], b""),
        "/tools/tag/2026.09.01" => respond(io, "200 OK", &[], b"release page"),
        "/tools/SHA2-256SUMS" | "/tools/bad/SHA2-256SUMS" => {
            let hash = if path.contains("/bad/") { "0".repeat(64) } else { tool_sha() };
            let text: String = TOOL_ASSETS.iter().map(|a| format!("{hash}  {a}\n")).collect();
            respond(io, "200 OK", &[], text.as_bytes())
        }
        p if p.starts_with("/tools/") || p.starts_with("/qjs/") => respond(io, "200 OK", &[], TOOL_PAYLOAD),
        "/redir" => respond(io, "302 Found", &[("Location", "/small".to_string())], b""),
        "/redir-loop" => respond(io, "302 Found", &[("Location", "/redir-loop".to_string())], b""),
        "/redir-cross" => {
            let port = io_port(&headers);
            respond(io, "302 Found", &[("Location", format!("http://127.0.0.1:{port}/echo-headers"))], b"")
        }
        "/echo-headers" => {
            let text: String = headers.iter().map(|(k, v)| format!("{k}: {v}\n")).collect();
            respond(io, "200 OK", &[], text.as_bytes())
        }
        "/echo-post" => respond(io, "200 OK", &[("X-Method", method.clone())], &body),
        "/chunked" => {
            io.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")?;
            io.write_all(b"5\r\nhello\r\n")?;
            io.write_all(b"8;ext=1\r\n chunked\r\n")?;
            io.write_all(b"6\r\n world\r\n")?;
            io.write_all(b"0\r\nX-Trailer: yes\r\n\r\n")
        }
        "/until-close" => {
            io.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nno length given")?;
            io.flush()
        }
        _ => respond(io, "404 Not Found", &[], b"unknown"),
    }
}

fn io_port(headers: &[(String, String)]) -> String {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("host")).and_then(|(_, v)| v.rsplit(':').next()).unwrap_or("0").to_string()
}
