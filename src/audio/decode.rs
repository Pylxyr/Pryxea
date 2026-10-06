//! Turns a WebM/Opus or MP4/AAC byte source into 48 kHz interleaved stereo
//! f32 PCM, a bit at a time. Demuxing and AAC come from symphonia (pure Rust,
//! only those parts compiled in); Opus goes through libopus.
//!
//! Only what the song source actually serves is supported: Opus (WebM or
//! MP4) and AAC-LC (MP4), mono or stereo. Anything else is reported as an
//! error so the player can skip the track instead of playing noise.

use std::fmt;

use symphonia::core::codecs::audio::well_known::profiles::{CODEC_PROFILE_AAC_HE, CODEC_PROFILE_AAC_HE_V2};
use symphonia::core::codecs::audio::well_known::{CODEC_ID_AAC, CODEC_ID_OPUS};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;

use super::opus::{self, SAMPLE_RATE};
use super::resample::Resampler;

/// Give up on a stream after this many undecodable packets in a row.
const MAX_CONSECUTIVE_BAD_PACKETS: u32 = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecodeError {}

fn err<T>(msg: impl Into<String>) -> Result<T, DecodeError> {
    Err(DecodeError(msg.into()))
}

fn sym(what: &str, e: SymError) -> DecodeError {
    DecodeError(format!("{what}: {e}"))
}

enum Codec {
    Opus { decoder: opus::Decoder, skip_frames: usize },
    Aac(Box<dyn AudioDecoder>),
}

pub struct TrackDecoder {
    format: Box<dyn FormatReader>,
    track_id: u32,
    codec: Codec,
    resampler: Option<Resampler>,
    duration_secs: Option<f64>,
    scratch: Vec<f32>,
    stereo: Vec<f32>,
    bad_packets: u32,
    finished: bool,
}

/// What an OpusHead header says that matters for playback.
struct OpusHead {
    channels: u8,
    pre_skip: u16,
    gain_q8: i16,
    mapping_family: u8,
}

fn parse_opus_head(extra: &[u8]) -> Option<OpusHead> {
    if extra.len() < 19 || &extra[..8] != b"OpusHead" {
        return None;
    }
    Some(OpusHead {
        channels: extra[9],
        pre_skip: u16::from_le_bytes([extra[10], extra[11]]),
        gain_q8: i16::from_le_bytes([extra[16], extra[17]]),
        mapping_family: extra[18],
    })
}

impl TrackDecoder {
    /// Probes the container and prepares a decoder. `extension` ("webm",
    /// "m4a", ...) is only a hint; the content is sniffed either way.
    pub fn open(source: Box<dyn MediaSource>, extension: Option<&str>) -> Result<TrackDecoder, DecodeError> {
        let mss = MediaSourceStream::new(source, Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = extension {
            hint.with_extension(ext);
        }
        let format = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .map_err(|e| sym("unrecognised media format", e))?;

        let track = format.first_track_known_codec(TrackType::Audio).or_else(|| format.default_track(TrackType::Audio));
        let Some(track) = track else { return err("no audio track in the stream") };
        let track_id = track.id;
        let Some(params) = track.codec_params.as_ref().and_then(|p| p.audio()) else {
            return err("the audio track has no usable codec parameters");
        };
        let duration_secs = track
            .duration
            .zip(track.time_base)
            .and_then(|(d, tb)| tb.calc_duration(d))
            .map(|t| t.as_secs_f64())
            .filter(|s| s.is_finite() && *s > 0.0);

        let (codec, src_rate) = if params.codec == CODEC_ID_OPUS {
            let head = params.extra_data.as_deref().and_then(parse_opus_head);
            if let Some(h) = &head {
                if h.channels > 2 || h.mapping_family != 0 {
                    return err(format!("unsupported Opus layout ({} channels, mapping family {})", h.channels, h.mapping_family));
                }
            }
            let decoder = opus::Decoder::new(head.as_ref().map_or(0, |h| h.gain_q8)).map_err(|e| DecodeError(e.to_string()))?;
            (Codec::Opus { decoder, skip_frames: head.map_or(0, |h| usize::from(h.pre_skip)) }, SAMPLE_RATE)
        } else if params.codec == CODEC_ID_AAC {
            if matches!(params.profile, Some(p) if p == CODEC_PROFILE_AAC_HE || p == CODEC_PROFILE_AAC_HE_V2) {
                return err("HE-AAC streams are not supported (pick an AAC-LC or Opus format)");
            }
            let Some(rate) = params.sample_rate else { return err("the AAC stream does not state its sample rate") };
            let decoder = symphonia::default::get_codecs()
                .make_audio_decoder(params, &AudioDecoderOptions::default())
                .map_err(|e| sym("cannot start the AAC decoder", e))?;
            (Codec::Aac(decoder), rate)
        } else {
            return err("unsupported audio codec (only Opus and AAC-LC are handled)");
        };

        let resampler = if src_rate == SAMPLE_RATE {
            None
        } else {
            Some(Resampler::new(src_rate, SAMPLE_RATE).map_err(|e| DecodeError(e.to_string()))?)
        };
        Ok(TrackDecoder {
            format,
            track_id,
            codec,
            resampler,
            duration_secs,
            scratch: Vec::new(),
            stereo: Vec::new(),
            bad_packets: 0,
            finished: false,
        })
    }

    /// Length of the track in seconds, when the container states it.
    pub fn duration_secs(&self) -> Option<f64> {
        self.duration_secs
    }

    /// Decodes until at least one packet's worth of audio is appended to `out`
    /// (48 kHz interleaved stereo). Returns `Ok(false)` once the stream has
    /// ended and everything has been flushed.
    pub fn decode_more(&mut self, out: &mut Vec<f32>) -> Result<bool, DecodeError> {
        if self.finished {
            return Ok(false);
        }
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => return Ok(self.finish(out)),
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(self.finish(out)),
                Err(SymError::ResetRequired) => return Ok(self.finish(out)),
                Err(e) => return Err(sym("reading the stream failed", e)),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            self.scratch.clear();
            let decoded = match &mut self.codec {
                Codec::Opus { decoder, skip_frames } => match decoder.decode(&packet.data, &mut self.scratch) {
                    Ok(_) => {
                        let drop = (*skip_frames).min(self.scratch.len() / 2);
                        *skip_frames -= drop;
                        self.scratch.drain(..drop * 2);
                        self.stereo = std::mem::take(&mut self.scratch);
                        Ok(())
                    }
                    Err(e) => Err(DecodeError(e.to_string())),
                },
                Codec::Aac(decoder) => match decoder.decode(&packet) {
                    Ok(buf) => {
                        let channels = buf.spec().channels().count();
                        let mut interleaved = Vec::new();
                        buf.copy_to_vec_interleaved::<f32>(&mut interleaved);
                        self.stereo.clear();
                        match channels {
                            1 => self.stereo.extend(interleaved.iter().flat_map(|&s| [s, s])),
                            2 => self.stereo = interleaved,
                            n => return err(format!("unsupported channel count: {n}")),
                        }
                        Ok(())
                    }
                    Err(SymError::DecodeError(m)) => Err(DecodeError(format!("bad AAC packet: {m}"))),
                    Err(e) => return Err(sym("AAC decoding failed", e)),
                },
            };
            match decoded {
                Ok(()) => self.bad_packets = 0,
                Err(e) => {
                    self.bad_packets += 1;
                    if self.bad_packets > MAX_CONSECUTIVE_BAD_PACKETS {
                        return err(format!("too many undecodable packets in a row (last: {e})"));
                    }
                    continue; // skip a damaged packet and keep going
                }
            }
            let before = out.len();
            match &mut self.resampler {
                Some(r) => r.process(&self.stereo, out),
                None => out.extend_from_slice(&self.stereo),
            }
            if out.len() > before {
                return Ok(true);
            }
        }
    }

    fn finish(&mut self, out: &mut Vec<f32>) -> bool {
        self.finished = true;
        let before = out.len();
        if let Some(r) = &mut self.resampler {
            r.finish(out);
        }
        out.len() > before
    }
}

/// f32 [-1, 1] -> i16, rounding and clipping (never wrapping).
pub fn to_i16(samples: &[f32], out: &mut Vec<i16>) {
    out.extend(samples.iter().map(|&s| (s * 32768.0).round().clamp(-32768.0, 32767.0) as i16));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;
    use std::io::Cursor;

    fn fixture(name: &str) -> Box<dyn MediaSource> {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        Box::new(Cursor::new(std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))))
    }

    fn decode_all(name: &str, ext: &str) -> (Vec<f32>, Option<f64>) {
        let mut dec = TrackDecoder::open(fixture(name), Some(ext)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let duration = dec.duration_secs();
        let mut out = Vec::new();
        while dec.decode_more(&mut out).unwrap() {}
        (out, duration)
    }

    /// Amplitude of `freq` in one channel of 48 kHz interleaved stereo (single-bin DFT).
    fn amplitude(pcm: &[f32], channel: usize, freq: f64) -> f64 {
        let (mut re, mut im, mut n) = (0.0, 0.0, 0usize);
        for (i, frame) in pcm.chunks_exact(2).enumerate().skip(4_800).take(pcm.len() / 2 - 9_600) {
            let a = 2.0 * PI * freq * i as f64 / 48_000.0;
            re += f64::from(frame[channel]) * a.cos();
            im += f64::from(frame[channel]) * a.sin();
            n += 1;
        }
        2.0 * (re * re + im * im).sqrt() / n as f64
    }

    #[test]
    fn opus_in_webm_decodes_with_the_right_length_channels_and_level() {
        let (pcm, duration) = decode_all("tone.webm", "webm");
        let frames = pcm.len() / 2;
        assert!((95_000..=98_000).contains(&frames), "{frames} frames");
        // WebM often omits the length; when it is stated it must be right.
        assert!(duration.is_none_or(|d| (1.9..2.1).contains(&d)), "{duration:?}");
        // Left carries 440 Hz and right 660 Hz at amplitude 0.5, and not the other way round.
        assert!((amplitude(&pcm, 0, 440.0) - 0.5).abs() < 0.02);
        assert!((amplitude(&pcm, 1, 660.0) - 0.5).abs() < 0.02);
        assert!(amplitude(&pcm, 0, 660.0) < 0.01 && amplitude(&pcm, 1, 440.0) < 0.01);
    }

    #[test]
    fn the_opus_pre_skip_is_removed_from_the_start() {
        let (pcm, _) = decode_all("tone.webm", "webm");
        // The first output frames are the start of the tone, not the encoder's priming silence:
        // within 1 ms the left channel is already moving.
        let early_peak = pcm[..96].iter().step_by(2).fold(0f32, |m, s| m.max(s.abs()));
        assert!(early_peak > 0.02, "first 48 frames look silent: {early_peak}");
    }

    #[test]
    fn mono_opus_comes_out_as_dual_mono() {
        let (pcm, _) = decode_all("tone_mono.webm", "webm");
        assert!(pcm.chunks_exact(2).skip(2_000).take(2_000).all(|f| (f[0] - f[1]).abs() < 1e-6));
        assert!((amplitude(&pcm, 0, 440.0) - 0.5).abs() < 0.02);
    }

    #[test]
    fn aac_at_44100_hz_is_resampled_to_48k() {
        for name in ["tone44.m4a", "tone44_frag.m4a"] {
            let (pcm, _) = decode_all(name, "m4a");
            let frames = pcm.len() / 2;
            assert!((95_500..=98_500).contains(&frames), "{name}: {frames} frames");
            assert!((amplitude(&pcm, 0, 440.0) - 0.5).abs() < 0.03, "{name} left");
            assert!((amplitude(&pcm, 1, 660.0) - 0.5).abs() < 0.03, "{name} right");
            assert!(amplitude(&pcm, 0, 660.0) < 0.01, "{name} crosstalk");
        }
    }

    #[test]
    fn garbage_and_truncated_input_report_errors_instead_of_panicking() {
        let junk: Box<dyn MediaSource> = Box::new(Cursor::new(vec![0x42u8; 5_000]));
        assert!(TrackDecoder::open(junk, Some("webm")).is_err());
        let mut cut = std::fs::read(format!("{}/tests/fixtures/tone.webm", env!("CARGO_MANIFEST_DIR"))).unwrap();
        cut.truncate(cut.len() * 3 / 4);
        let Ok(mut dec) = TrackDecoder::open(Box::new(Cursor::new(cut)), Some("webm")) else {
            return; // refusing a file whose index is cut off is fine too
        };
        let mut out = Vec::new();
        loop {
            match dec.decode_more(&mut out) {
                Ok(true) => {}
                Ok(false) | Err(_) => break, // either a clean early end or an error is acceptable
            }
        }
        assert!(out.len() / 2 > 20_000, "should still deliver the part that is there ({} frames)", out.len() / 2);
    }

    #[test]
    fn to_i16_rounds_clips_and_never_wraps() {
        let mut out = Vec::new();
        to_i16(&[0.0, 0.5, -0.5, 1.0, -1.0, 2.5, -9.0], &mut out);
        assert_eq!(out, [0, 16384, -16384, 32767, -32768, 32767, -32768]);
    }
}
