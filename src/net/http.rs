//! A small blocking HTTP/1.1 client: GET/POST, redirects, chunked or
//! length-delimited bodies, http and https (rustls with the OS trust store).
//! One connection per request (`Connection: close`): the callers make a handful
//! of requests per song, so keep-alive isn't worth its complexity.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, StreamOwned};

use super::url::Url;

const MAX_REDIRECTS: usize = 5;
const MAX_HEAD_BYTES: usize = 64 * 1024;
const MAX_HEAD_LINES: usize = 200;
const DEFAULT_UA: &str = concat!("Pryxea/", env!("CARGO_PKG_VERSION"));

#[derive(Debug)]
pub enum HttpError {
    /// Bad URL, header or other caller mistake.
    Invalid(String),
    Connect(String),
    Tls(String),
    Io(String),
    Protocol(String),
    /// A non-success status the caller asked to be treated as an error.
    Status(u16),
    TooLarge,
    TooManyRedirects,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpError::Invalid(m) => write!(f, "invalid request: {m}"),
            HttpError::Connect(m) => write!(f, "cannot connect: {m}"),
            HttpError::Tls(m) => write!(f, "TLS error: {m}"),
            HttpError::Io(m) => write!(f, "network error: {m}"),
            HttpError::Protocol(m) => write!(f, "bad HTTP response: {m}"),
            HttpError::Status(code) => write!(f, "HTTP {code}"),
            HttpError::TooLarge => f.write_str("response too large"),
            HttpError::TooManyRedirects => f.write_str("too many redirects"),
        }
    }
}

impl std::error::Error for HttpError {}

impl HttpError {
    /// Worth trying again shortly (network trouble or a transient server status).
    pub fn is_retryable(&self) -> bool {
        match self {
            HttpError::Connect(_) | HttpError::Io(_) | HttpError::Protocol(_) => true,
            HttpError::Status(code) => status_is_transient(*code),
            _ => false,
        }
    }
}

pub fn status_is_transient(code: u16) -> bool {
    matches!(code, 429 | 500 | 502 | 503 | 504)
}

pub type Result<T> = std::result::Result<T, HttpError>;

fn io_err(e: io::Error) -> HttpError {
    match e.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => HttpError::Io("timed out".into()),
        _ => HttpError::Io(e.to_string()),
    }
}

// ------------------------------------------------------------------ request

#[derive(Debug, Clone)]
pub struct Request {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// When false, a 3xx answer is returned as is, so the caller can vet where it points.
    pub follow_redirects: bool,
}

impl Request {
    pub fn get(url: impl Into<String>) -> Request {
        Request { method: "GET", url: url.into(), headers: Vec::new(), body: Vec::new(), follow_redirects: true }
    }

    pub fn post(url: impl Into<String>, body: Vec<u8>) -> Request {
        Request { method: "POST", url: url.into(), headers: Vec::new(), body, follow_redirects: true }
    }

    pub fn header(mut self, name: &str, value: &str) -> Request {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn no_redirects(mut self) -> Request {
        self.follow_redirects = false;
        self
    }

    pub fn headers(mut self, extra: &[(String, String)]) -> Request {
        self.headers.extend(extra.iter().cloned());
        self
    }
}

fn valid_header(name: &str, value: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        && !value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
}

// --------------------------------------------------------------- connection

enum Conn {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

// ------------------------------------------------------------------- client

pub struct Client {
    tls: OnceLock<std::result::Result<Arc<ClientConfig>, String>>,
    pub user_agent: String,
    pub connect_timeout: Duration,
    /// Longest silence tolerated on a socket while reading or writing.
    pub io_timeout: Duration,
}

impl Default for Client {
    fn default() -> Self {
        Client::new()
    }
}

impl Client {
    /// Trusts whatever the operating system trusts. The TLS setup is built on first https use.
    pub fn new() -> Client {
        Client {
            tls: OnceLock::new(),
            user_agent: DEFAULT_UA.to_string(),
            connect_timeout: Duration::from_secs(10),
            io_timeout: Duration::from_secs(15),
        }
    }

    /// Uses a specific TLS configuration (tests trust their own certificate this way).
    pub fn with_tls(config: Arc<ClientConfig>) -> Client {
        let client = Client::new();
        let _ = client.tls.set(Ok(config));
        client
    }

    /// The shared TLS settings (OS trust store), for connections made outside this client.
    pub fn tls_config(&self) -> Result<Arc<ClientConfig>> {
        self.tls
            .get_or_init(|| {
                use rustls_platform_verifier::BuilderVerifierExt;
                ClientConfig::builder()
                    .with_platform_verifier()
                    .map(|b| Arc::new(b.with_no_client_auth()))
                    .map_err(|e| e.to_string())
            })
            .clone()
            .map_err(HttpError::Tls)
    }

    fn connect(&self, url: &Url) -> Result<Conn> {
        let addrs = (url.host.as_str(), url.port).to_socket_addrs().map_err(|e| HttpError::Connect(format!("{}: {e}", url.host)))?;
        let mut last = None;
        let mut tcp = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, self.connect_timeout) {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let tcp = tcp.ok_or_else(|| HttpError::Connect(format!("{}: {}", url.host, last.map_or("no address".to_string(), |e| e.to_string()))))?;
        tcp.set_read_timeout(Some(self.io_timeout)).map_err(io_err)?;
        tcp.set_write_timeout(Some(self.io_timeout)).map_err(io_err)?;
        let _ = tcp.set_nodelay(true);
        if !url.https {
            return Ok(Conn::Plain(tcp));
        }
        let name = ServerName::try_from(url.host.clone()).map_err(|e| HttpError::Invalid(format!("bad host name {:?}: {e}", url.host)))?;
        let conn = ClientConnection::new(self.tls_config()?, name).map_err(|e| HttpError::Tls(e.to_string()))?;
        Ok(Conn::Tls(Box::new(StreamOwned::new(conn, tcp))))
    }

    /// Sends the request, following redirects. Non-2xx statuses are returned, not turned into errors.
    pub fn send(&self, req: &Request) -> Result<Response> {
        let mut url = Url::parse(&req.url).ok_or_else(|| HttpError::Invalid(format!("not an http(s) URL: {:?}", truncate(&req.url))))?;
        let mut method = req.method;
        let mut body: &[u8] = &req.body;
        let mut headers = req.headers.clone();
        for (k, v) in &headers {
            if !valid_header(k, v) {
                return Err(HttpError::Invalid(format!("bad header {k:?}")));
            }
        }
        for _ in 0..=MAX_REDIRECTS {
            let resp = self.send_once(&url, method, &headers, body)?;
            let redirect = req.follow_redirects && matches!(resp.status, 301 | 302 | 303 | 307 | 308);
            let Some(location) = redirect.then(|| resp.header("location")).flatten() else { return Ok(resp) };
            let next = url.join(location).ok_or_else(|| HttpError::Protocol(format!("bad redirect target {:?}", truncate(location))))?;
            if url.https && !next.https {
                return Err(HttpError::Invalid("refusing a redirect from https to http".into()));
            }
            if !next.same_origin(&url) {
                // Credentials never follow a redirect to another origin.
                headers.retain(|(k, _)| !["authorization", "cookie", "client-id"].iter().any(|s| k.eq_ignore_ascii_case(s)));
            }
            if resp.status == 303 || (matches!(resp.status, 301 | 302) && method == "POST") {
                method = "GET";
                body = &[];
                headers.retain(|(k, _)| !k.eq_ignore_ascii_case("content-type"));
            }
            url = next;
        }
        Err(HttpError::TooManyRedirects)
    }

    fn send_once(&self, url: &Url, method: &str, headers: &[(String, String)], body: &[u8]) -> Result<Response> {
        let mut conn = self.connect(url)?;
        let has = |name: &str| headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
        let mut head = format!("{method} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n", url.target, url.host_header());
        if !has("user-agent") {
            head.push_str(&format!("User-Agent: {}\r\n", self.user_agent));
        }
        if !has("accept") {
            head.push_str("Accept: */*\r\n");
        }
        if !body.is_empty() || method == "POST" {
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        for (k, v) in headers {
            if !k.eq_ignore_ascii_case("host") && !k.eq_ignore_ascii_case("connection") && !k.eq_ignore_ascii_case("content-length") {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
        }
        head.push_str("\r\n");
        conn.write_all(head.as_bytes()).map_err(map_tls_io)?;
        conn.write_all(body).map_err(map_tls_io)?;
        conn.flush().map_err(map_tls_io)?;

        let mut reader = BufReader::with_capacity(16 * 1024, conn);
        let (status, resp_headers) = loop {
            let (status, resp_headers) = read_head(&mut reader)?;
            if (100..200).contains(&status) && status != 101 {
                continue; // interim response (100 Continue ...)
            }
            break (status, resp_headers);
        };
        let framing = framing_for(method, status, &resp_headers)?;
        Ok(Response { status, headers: resp_headers, body: Body { reader, framing }, url: url_string(url) })
    }
}

fn url_string(url: &Url) -> String {
    format!("{}://{}{}", if url.https { "https" } else { "http" }, url.host_header(), url.target)
}

fn truncate(s: &str) -> String {
    s.chars().take(100).collect()
}

/// rustls reports a missing TLS close_notify as UnexpectedEof; elsewhere that is just an I/O error.
fn map_tls_io(e: io::Error) -> HttpError {
    if e.kind() == io::ErrorKind::InvalidData || e.get_ref().is_some_and(|inner| inner.is::<rustls::Error>()) {
        return HttpError::Tls(e.to_string());
    }
    io_err(e)
}

fn read_head(reader: &mut BufReader<Conn>) -> Result<(u16, Vec<(String, String)>)> {
    let mut raw = Vec::with_capacity(1024);
    let mut lines = 0;
    loop {
        let before = raw.len();
        let n = reader.read_until(b'\n', &mut raw).map_err(map_tls_io)?;
        if n == 0 {
            return Err(HttpError::Protocol("the connection closed before the response headers ended".into()));
        }
        lines += 1;
        if raw.len() > MAX_HEAD_BYTES || lines > MAX_HEAD_LINES {
            return Err(HttpError::Protocol("response headers too large".into()));
        }
        let line = &raw[before..];
        if line == b"\r\n" || line == b"\n" {
            break;
        }
    }
    let mut storage = [httparse::EMPTY_HEADER; 128];
    let mut parsed = httparse::Response::new(&mut storage);
    match parsed.parse(&raw) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => return Err(HttpError::Protocol("incomplete response headers".into())),
        Err(e) => return Err(HttpError::Protocol(e.to_string())),
    }
    let status = parsed.code.ok_or_else(|| HttpError::Protocol("no status code".into()))?;
    let headers = parsed.headers.iter().map(|h| (h.name.to_string(), String::from_utf8_lossy(h.value).trim().to_string())).collect();
    Ok((status, headers))
}

fn framing_for(method: &str, status: u16, headers: &[(String, String)]) -> Result<Framing> {
    if method == "HEAD" || status == 204 || status == 304 || (100..200).contains(&status) {
        return Ok(Framing::None);
    }
    let get = |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str());
    if get("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        return Ok(Framing::Chunked(0));
    }
    let lengths: Vec<&str> = headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("content-length")).map(|(_, v)| v.as_str()).collect();
    match lengths.as_slice() {
        [] => Ok(Framing::UntilClose),
        [first, rest @ ..] if rest.iter().all(|v| v == first) => {
            first.parse().map(Framing::Length).map_err(|_| HttpError::Protocol(format!("bad Content-Length {first:?}")))
        }
        _ => Err(HttpError::Protocol("conflicting Content-Length headers".into())),
    }
}

// ----------------------------------------------------------------- response

enum Framing {
    None,
    Length(u64),
    /// Bytes left in the current chunk (0 = a chunk header comes next).
    Chunked(u64),
    UntilClose,
}

struct Body {
    reader: BufReader<Conn>,
    framing: Framing,
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// The URL that finally answered, after redirects.
    pub url: String,
    body: Body,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn error_for_status(self) -> Result<Response> {
        if self.is_success() { Ok(self) } else { Err(HttpError::Status(self.status)) }
    }

    /// Reads the whole body, refusing anything over `limit` bytes.
    pub fn bytes(mut self, limit: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        (&mut self).take(limit as u64 + 1).read_to_end(&mut out).map_err(io_err)?;
        if out.len() > limit { Err(HttpError::TooLarge) } else { Ok(out) }
    }
}

impl Read for Response {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.body.read(buf)
    }
}

fn truncated() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "the connection closed before the full body arrived")
}

impl Body {
    fn read_line(&mut self) -> io::Result<String> {
        let mut line = Vec::new();
        let mut limited = (&mut self.reader).take(4096);
        limited.read_until(b'\n', &mut line)?;
        if !line.ends_with(b"\n") {
            return Err(truncated());
        }
        Ok(String::from_utf8_lossy(&line).trim_end().to_string())
    }
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        match self.framing {
            Framing::None => Ok(0),
            Framing::Length(0) => Ok(0),
            Framing::Length(left) => {
                let want = buf.len().min(left.min(usize::MAX as u64) as usize);
                match self.reader.read(&mut buf[..want])? {
                    0 => Err(truncated()),
                    n => {
                        self.framing = Framing::Length(left - n as u64);
                        Ok(n)
                    }
                }
            }
            Framing::Chunked(0) => {
                let line = self.read_line()?;
                let size_text = line.split(';').next().unwrap_or("").trim();
                let size = u64::from_str_radix(size_text, 16).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("bad chunk size {size_text:?}")))?;
                if size == 0 {
                    // Trailers, then the terminating blank line.
                    while !self.read_line()?.is_empty() {}
                    self.framing = Framing::None;
                    return Ok(0);
                }
                self.framing = Framing::Chunked(size);
                self.read(buf)
            }
            Framing::Chunked(left) => {
                let want = buf.len().min(left.min(usize::MAX as u64) as usize);
                match self.reader.read(&mut buf[..want])? {
                    0 => Err(truncated()),
                    n => {
                        let left = left - n as u64;
                        if left == 0 && !self.read_line()?.is_empty() {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "missing CRLF after a chunk"));
                        }
                        self.framing = Framing::Chunked(left);
                        Ok(n)
                    }
                }
            }
            Framing::UntilClose => match self.reader.read(buf) {
                // A server that just hangs up without TLS close_notify is how this framing ends.
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
                other => other,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_validation_blocks_injection() {
        assert!(valid_header("X-Test", "value"));
        assert!(!valid_header("X-Test", "a\r\nInjected: 1"));
        assert!(!valid_header("Bad Name", "v"));
        assert!(!valid_header("", "v"));
    }

    #[test]
    fn only_transient_statuses_are_retryable() {
        for code in [429, 500, 502, 503, 504] {
            assert!(HttpError::Status(code).is_retryable(), "{code}");
        }
        for code in [400, 401, 403, 404, 410, 416] {
            assert!(!HttpError::Status(code).is_retryable(), "{code}");
        }
        assert!(HttpError::Io("x".into()).is_retryable());
        assert!(!HttpError::TooLarge.is_retryable());
    }

    #[test]
    fn framing_prefers_chunked_and_rejects_conflicting_lengths() {
        let h = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
        assert!(matches!(framing_for("GET", 200, &h(&[("Transfer-Encoding", "chunked"), ("Content-Length", "5")])), Ok(Framing::Chunked(0))));
        assert!(matches!(framing_for("GET", 200, &h(&[("content-length", "7"), ("Content-Length", "7")])), Ok(Framing::Length(7))));
        assert!(framing_for("GET", 200, &h(&[("content-length", "7"), ("Content-Length", "8")])).is_err());
        assert!(matches!(framing_for("GET", 200, &h(&[])), Ok(Framing::UntilClose)));
        assert!(matches!(framing_for("HEAD", 200, &h(&[("content-length", "7")])), Ok(Framing::None)));
        assert!(matches!(framing_for("GET", 204, &h(&[])), Ok(Framing::None)));
    }
}
