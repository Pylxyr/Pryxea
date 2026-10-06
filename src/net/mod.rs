//! Networking without a framework: a small blocking HTTP/1.1 client over
//! rustls, and a seekable HTTP source for the audio decoder. Blocking on
//! purpose: decoders already run on their own short-lived threads, and
//! anything else calls in through `spawn_blocking`.

pub mod http;
pub mod source;
pub mod url;
