//! Audio: decode, resample, encode, Ogg-mux, and the real-time engine that
//! strings tracks together without gaps. Songs play at their own volume: there
//! is no loudness analysis, gain or limiter anywhere.

pub mod decode;
pub mod engine;
pub mod ogg;
pub mod opus;
pub mod resample;
