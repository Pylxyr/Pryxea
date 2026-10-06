//! Pryxea: a small, native Twitch song-request radio.
//!
//! Chat commands go in, one Opus stream comes out for OBS. Everything here is
//! built to stay small: a single-threaded async runtime, no framework, no
//! bundled browser, and no child processes while the stream is idle.

pub mod audio;
pub mod commands;
pub mod config;
pub mod envfile;
pub mod http;
pub mod hub;
pub mod logging;
pub mod paths;
pub mod state;
pub mod store;
pub mod telemetry;
pub mod toggles;
pub mod tunables;
