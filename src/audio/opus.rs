//! Thin safe wrappers over the bundled libopus. This is the only file in the
//! crate with `unsafe` code.

use std::ffi::CStr;
use std::fmt;

use opusic_sys as sys;

/// Opus always runs at 48 kHz here.
pub const SAMPLE_RATE: u32 = 48_000;
/// 20 ms of audio per Opus packet, in frames (one frame = one sample per channel).
pub const FRAME_SIZE: usize = 960;
/// Largest packet libopus can produce for one frame.
const MAX_PACKET: usize = 1_500;
/// Longest Opus packet duration (120 ms) in frames.
const MAX_DECODE_FRAMES: usize = 5_760;

#[derive(Debug)]
pub struct OpusError(pub i32);

impl fmt::Display for OpusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: opus_strerror returns a pointer to a static NUL-terminated string.
        let text = unsafe { CStr::from_ptr(sys::opus_strerror(self.0)) };
        write!(f, "libopus: {} ({})", text.to_string_lossy(), self.0)
    }
}

impl std::error::Error for OpusError {}

fn check(code: i32) -> Result<i32, OpusError> {
    if code < 0 { Err(OpusError(code)) } else { Ok(code) }
}

// ------------------------------------------------------------------ encoder

/// Stereo 48 kHz Opus encoder.
pub struct Encoder {
    ptr: *mut sys::OpusEncoder,
    lookahead: u16,
}

// SAFETY: the encoder state is only touched through &mut self, so it is safe to move between threads.
unsafe impl Send for Encoder {}

impl Encoder {
    /// VBR at about `bitrate_bps`, complexity 5: the same settings ffmpeg's
    /// `-c:a libopus -b:a ... -vbr on -compression_level 5` used before.
    pub fn new(bitrate_bps: i32) -> Result<Encoder, OpusError> {
        let mut err = 0;
        // SAFETY: plain FFI call; `err` is a valid out pointer.
        let ptr = unsafe { sys::opus_encoder_create(SAMPLE_RATE as i32, 2, sys::OPUS_APPLICATION_AUDIO, &mut err) };
        check(err)?;
        let mut enc = Encoder { ptr, lookahead: 0 };
        // SAFETY: `ptr` is a live encoder; each request takes a single opus_int32 argument.
        unsafe {
            check(sys::opus_encoder_ctl(ptr, sys::OPUS_SET_BITRATE_REQUEST, bitrate_bps))?;
            check(sys::opus_encoder_ctl(ptr, sys::OPUS_SET_VBR_REQUEST, 1i32))?;
            check(sys::opus_encoder_ctl(ptr, sys::OPUS_SET_COMPLEXITY_REQUEST, 5i32))?;
            let mut la: i32 = 0;
            check(sys::opus_encoder_ctl(ptr, sys::OPUS_GET_LOOKAHEAD_REQUEST, &mut la as *mut i32))?;
            enc.lookahead = la.clamp(0, i32::from(u16::MAX)) as u16;
        }
        Ok(enc)
    }

    /// Samples the decoder must discard at the start (goes into OpusHead).
    pub fn lookahead(&self) -> u16 {
        self.lookahead
    }

    /// Encodes exactly one 20 ms frame of interleaved stereo i16 samples.
    pub fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) -> Result<(), OpusError> {
        assert_eq!(pcm.len(), FRAME_SIZE * 2, "one 20 ms stereo frame");
        out.resize(MAX_PACKET, 0);
        // SAFETY: `pcm` holds FRAME_SIZE*2 samples and `out` has MAX_PACKET writable bytes.
        let n = unsafe { sys::opus_encode(self.ptr, pcm.as_ptr(), FRAME_SIZE as i32, out.as_mut_ptr(), MAX_PACKET as i32) };
        out.truncate(check(n)? as usize);
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from opus_encoder_create and is destroyed exactly once.
        unsafe { sys::opus_encoder_destroy(self.ptr) }
    }
}

// ------------------------------------------------------------------ decoder

/// Stereo 48 kHz Opus decoder. Mono streams come out as dual-mono.
pub struct Decoder {
    ptr: *mut sys::OpusDecoder,
}

// SAFETY: as for Encoder.
unsafe impl Send for Decoder {}

impl Decoder {
    /// `gain_q8` is the OpusHead output gain in Q7.8 dB (0 for almost every file).
    pub fn new(gain_q8: i16) -> Result<Decoder, OpusError> {
        let mut err = 0;
        // SAFETY: plain FFI call; `err` is a valid out pointer.
        let ptr = unsafe { sys::opus_decoder_create(SAMPLE_RATE as i32, 2, &mut err) };
        check(err)?;
        let dec = Decoder { ptr };
        if gain_q8 != 0 {
            // SAFETY: `ptr` is a live decoder; OPUS_SET_GAIN takes one opus_int32.
            unsafe { check(sys::opus_decoder_ctl(ptr, sys::OPUS_SET_GAIN_REQUEST, i32::from(gain_q8)))? };
        }
        Ok(dec)
    }

    /// Decodes one packet, appending interleaved stereo f32 frames to `out`.
    /// Returns the number of frames decoded.
    pub fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<usize, OpusError> {
        let start = out.len();
        out.resize(start + MAX_DECODE_FRAMES * 2, 0.0);
        // SAFETY: `packet` is valid for its length, and `out[start..]` has room for MAX_DECODE_FRAMES stereo frames.
        let n = unsafe {
            sys::opus_decode_float(self.ptr, packet.as_ptr(), packet.len() as i32, out[start..].as_mut_ptr(), MAX_DECODE_FRAMES as i32, 0)
        };
        match check(n) {
            Ok(frames) => {
                out.truncate(start + frames as usize * 2);
                Ok(frames as usize)
            }
            Err(e) => {
                out.truncate(start);
                Err(e)
            }
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from opus_decoder_create and is destroyed exactly once.
        unsafe { sys::opus_decoder_destroy(self.ptr) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn tone(freq: f32, frames: usize, amp: f32) -> Vec<i16> {
        (0..frames)
            .flat_map(|i| {
                let s = (2.0 * std::f32::consts::PI * freq * i as f32 / SAMPLE_RATE as f32).sin();
                let v = (s * amp * 32767.0) as i16;
                [v, v]
            })
            .collect()
    }

    #[test]
    fn a_tone_survives_an_encode_decode_round_trip() {
        let mut enc = Encoder::new(160_000).unwrap();
        assert!(enc.lookahead() > 0 && enc.lookahead() < 1000, "{}", enc.lookahead());
        let mut dec = Decoder::new(0).unwrap();
        let pcm = tone(1000.0, FRAME_SIZE * 25, 0.5);
        let (mut packet, mut decoded) = (Vec::new(), Vec::new());
        for frame in pcm.chunks(FRAME_SIZE * 2) {
            enc.encode(frame, &mut packet).unwrap();
            assert!(!packet.is_empty() && packet.len() < 1500);
            assert_eq!(dec.decode(&packet, &mut decoded).unwrap(), FRAME_SIZE);
        }
        assert_eq!(decoded.len(), pcm.len());
        // Steady-state RMS of a 0.5-amplitude sine is 0.354.
        let tail = &decoded[decoded.len() / 2..];
        let rms = (tail.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / tail.len() as f64).sqrt();
        assert!((rms - 0.3536).abs() < 0.02, "rms {rms}");
    }

    #[test]
    fn garbage_packets_are_an_error_not_a_crash() {
        let mut dec = Decoder::new(0).unwrap();
        let mut out = Vec::new();
        assert!(dec.decode(&[], &mut out).is_ok() || out.is_empty()); // empty packet = loss concealment or error
        out.clear();
        let _ = dec.decode(&[0xFF; 3], &mut out);
        assert!(out.len() % 2 == 0);
    }
}
