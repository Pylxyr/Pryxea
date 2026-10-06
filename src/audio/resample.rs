//! A streaming polyphase sinc resampler for interleaved stereo f32.
//!
//! Output sample `n` sits at input time `n * down / up`. Each of the `up`
//! possible fractional offsets gets its own precomputed FIR (Kaiser-windowed
//! sinc, normalised to unity gain at DC), so the per-sample cost is one short
//! dot product per channel. The ratio is reduced by its gcd; real-world
//! rates (22.05, 32, 44.1, 88.2, 96 kHz -> 48 kHz) give at most 320 phases.

use std::f64::consts::PI;

/// Zero crossings of the sinc kernel on each side of the centre.
const ZERO_CROSSINGS: f64 = 32.0;
/// Kaiser beta: roughly 90 dB of stopband attenuation.
const KAISER_BETA: f64 = 9.0;
/// Pass-band edge as a fraction of the lower Nyquist frequency. The slack
/// leaves room for the transition band so images and aliases stay far down.
const BANDWIDTH: f64 = 0.97;
/// More phases than this means an odd sample rate; refuse rather than build a huge table.
const MAX_PHASES: usize = 4096;

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, q) = (1.0, 1.0, x * x / 4.0);
    for k in 1..60 {
        term *= q / (f64::from(k) * f64::from(k));
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

fn kaiser(t: f64) -> f64 {
    // t in [-1, 1]
    if t.abs() >= 1.0 { 0.0 } else { bessel_i0(KAISER_BETA * (1.0 - t * t).sqrt()) / bessel_i0(KAISER_BETA) }
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 { 1.0 } else { (PI * x).sin() / (PI * x) }
}

pub struct Resampler {
    up: u64,
    down: u64,
    taps: usize,
    half: usize,
    /// `up` rows of `taps` coefficients.
    table: Vec<f32>,
    /// Interleaved stereo input not yet released; starts with `half - 1` zero frames.
    hist: Vec<f32>,
    /// Absolute input-frame index of `hist[0]` (negative while the leading zeros are present).
    base: i64,
    /// Next output frame index.
    n: u64,
    /// Real input frames seen so far.
    total_in: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct UnsupportedRate(pub u32);

impl std::fmt::Display for UnsupportedRate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unsupported sample rate {} Hz", self.0)
    }
}

impl Resampler {
    pub fn new(from_hz: u32, to_hz: u32) -> Result<Resampler, UnsupportedRate> {
        if from_hz < 4_000 || from_hz > 400_000 {
            return Err(UnsupportedRate(from_hz));
        }
        let g = gcd(u64::from(from_hz), u64::from(to_hz));
        let (up, down) = (u64::from(to_hz) / g, u64::from(from_hz) / g);
        if up as usize > MAX_PHASES {
            return Err(UnsupportedRate(from_hz));
        }
        // Cut-off relative to the input sample rate: at most input Nyquist (upsampling) or output Nyquist (downsampling).
        let cutoff = (BANDWIDTH * (up as f64 / down as f64).min(1.0)).min(BANDWIDTH);
        let half = (ZERO_CROSSINGS / cutoff).ceil() as usize;
        let taps = 2 * half;
        let mut table = vec![0f32; up as usize * taps];
        for phase in 0..up as usize {
            let frac = phase as f64 / up as f64;
            let row = &mut table[phase * taps..(phase + 1) * taps];
            let mut coeffs: Vec<f64> = (0..taps)
                .map(|k| {
                    // Distance from this tap's input sample to the output time, in input samples.
                    let u = (half as f64 - 1.0 - k as f64) + frac;
                    cutoff * sinc(cutoff * u) * kaiser(u / half as f64)
                })
                .collect();
            let sum: f64 = coeffs.iter().sum();
            for c in &mut coeffs {
                *c /= sum;
            }
            for (dst, c) in row.iter_mut().zip(coeffs) {
                *dst = c as f32;
            }
        }
        Ok(Resampler { up, down, taps, half, table, hist: vec![0.0; (half - 1) * 2], base: -((half - 1) as i64), n: 0, total_in: 0 })
    }

    /// True when the ratio is exactly 1 and no filtering is needed.
    pub fn is_identity(&self) -> bool {
        self.up == 1 && self.down == 1
    }

    /// Feeds interleaved stereo frames and appends the output they unlock.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        debug_assert!(input.len() % 2 == 0);
        self.total_in += (input.len() / 2) as u64;
        self.hist.extend_from_slice(input);
        self.run(out, None);
    }

    /// Flushes the tail: outputs every sample that lies inside the input's duration.
    pub fn finish(&mut self, out: &mut Vec<f32>) {
        let limit = (self.total_in * self.up).div_ceil(self.down); // output frames covering the whole input
        self.hist.resize(self.hist.len() + self.half * 2, 0.0);
        self.run(out, Some(limit));
    }

    fn run(&mut self, out: &mut Vec<f32>, limit: Option<u64>) {
        let frames_in_hist = (self.hist.len() / 2) as i64;
        loop {
            if limit.is_some_and(|l| self.n >= l) {
                break;
            }
            let pos = self.n * self.down;
            let i0 = (pos / self.up) as i64;
            let phase = (pos % self.up) as usize;
            // Needs input frames i0 - half + 1 ..= i0 + half.
            if self.base + frames_in_hist <= i0 + self.half as i64 {
                break;
            }
            let first = ((i0 - self.half as i64 + 1) - self.base) as usize * 2;
            let coeffs = &self.table[phase * self.taps..(phase + 1) * self.taps];
            let window = &self.hist[first..first + self.taps * 2];
            let (mut l, mut r) = (0f32, 0f32);
            for (c, frame) in coeffs.iter().zip(window.chunks_exact(2)) {
                l += c * frame[0];
                r += c * frame[1];
            }
            out.push(l);
            out.push(r);
            self.n += 1;
        }
        // Drop input that no later output can touch.
        let next_i0 = ((self.n * self.down) / self.up) as i64;
        let keep_from = next_i0 - self.half as i64 + 1;
        if keep_from > self.base {
            let drop_frames = ((keep_from - self.base) as usize).min(self.hist.len() / 2);
            self.hist.drain(..drop_frames * 2);
            self.base += drop_frames as i64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: u32, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let v = (2.0 * PI * freq * i as f64 / f64::from(rate)).sin() as f32 * 0.5;
                [v, -v]
            })
            .collect()
    }

    fn run_all(from: u32, to: u32, input: &[f32], chunk_frames: usize) -> Vec<f32> {
        let mut r = Resampler::new(from, to).unwrap();
        let mut out = Vec::new();
        for c in input.chunks(chunk_frames * 2) {
            r.process(c, &mut out);
        }
        r.finish(&mut out);
        out
    }

    /// Fits amplitude*sin(2*pi*f*t+phase) to the left channel (skipping edges) and returns
    /// (fitted amplitude, residual power relative to the fit's power, in dB).
    fn fit(out: &[f32], freq: f64, rate: u32, skip: usize) -> (f64, f64) {
        let l: Vec<f64> = out.chunks_exact(2).map(|f| f64::from(f[0])).skip(skip).take(out.len() / 2 - 2 * skip).collect();
        let (mut sc, mut ss) = (0.0, 0.0);
        for (i, x) in l.iter().enumerate() {
            let a = 2.0 * PI * freq * (i + skip) as f64 / f64::from(rate);
            sc += x * a.cos();
            ss += x * a.sin();
        }
        let (sc, ss) = (2.0 * sc / l.len() as f64, 2.0 * ss / l.len() as f64);
        let amp = (sc * sc + ss * ss).sqrt();
        let resid: f64 = l
            .iter()
            .enumerate()
            .map(|(i, x)| {
                let a = 2.0 * PI * freq * (i + skip) as f64 / f64::from(rate);
                (x - (sc * a.cos() + ss * a.sin())).powi(2)
            })
            .sum::<f64>()
            / l.len() as f64;
        (amp, 10.0 * (resid / (amp * amp / 2.0)).log10())
    }

    #[test]
    fn tones_keep_their_level_and_stay_clean_for_common_rates() {
        for (from, freq) in [(44_100u32, 1_000.0), (44_100, 10_000.0), (44_100, 16_000.0), (44_100, 18_000.0), (22_050, 5_000.0), (32_000, 7_000.0), (96_000, 15_000.0), (88_200, 20_000.0), (24_000, 9_000.0)] {
            let input = sine(freq, from, from as usize); // 1 s
            let out = run_all(from, 48_000, &input, 1_000);
            let (amp, resid_db) = fit(&out, freq, 48_000, 2_000);
            assert!((amp - 0.5).abs() < 0.005, "{from}->48k {freq} Hz: amplitude {amp}");
            assert!(resid_db < -75.0, "{from}->48k {freq} Hz: residual {resid_db:.1} dB");
        }
    }

    #[test]
    fn response_is_flat_to_the_top_of_the_audible_band() {
        // Print-free sweep: loss at 19 and 20 kHz must stay under 0.5 dB (4 % of amplitude... well under).
        for freq in [19_000.0, 20_000.0] {
            let out = run_all(44_100, 48_000, &sine(freq, 44_100, 44_100), 1_000);
            let (amp, _) = fit(&out, freq, 48_000, 2_000);
            let loss_db = 20.0 * (amp / 0.5).log10();
            assert!(loss_db > -0.5, "{freq} Hz: {loss_db:.2} dB");
        }
    }

    #[test]
    fn output_length_matches_the_ratio_exactly() {
        for (from, frames) in [(44_100u32, 44_100usize), (44_100, 12_345), (96_000, 9_999), (32_000, 1), (22_050, 100)] {
            let out = run_all(from, 48_000, &sine(440.0, from, frames), 777);
            let want = (frames as u64 * 48_000).div_ceil(u64::from(from)) as usize;
            assert_eq!(out.len() / 2, want, "{from} Hz, {frames} frames");
        }
    }

    #[test]
    fn chunking_does_not_change_the_output() {
        let input = sine(3_000.0, 44_100, 20_000);
        let one = run_all(44_100, 48_000, &input, 20_000);
        for chunk in [1, 7, 480, 4_410] {
            assert_eq!(run_all(44_100, 48_000, &input, chunk), one, "chunk size {chunk}");
        }
    }

    #[test]
    fn content_above_the_output_nyquist_is_filtered_when_downsampling() {
        // 40 kHz tone at 96 kHz would alias to 8 kHz at 48 kHz without a proper low-pass.
        let out = run_all(96_000, 48_000, &sine(40_000.0, 96_000, 96_000), 1_000);
        let (amp, _) = fit(&out, 8_000.0, 48_000, 2_000);
        assert!(amp < 0.5 * 10f64.powf(-70.0 / 20.0), "alias amplitude {amp}");
    }

    #[test]
    fn identity_and_unsupported_rates() {
        assert!(Resampler::new(48_000, 48_000).unwrap().is_identity());
        assert!(!Resampler::new(44_100, 48_000).unwrap().is_identity());
        assert_eq!(Resampler::new(44_101, 48_000).err(), Some(UnsupportedRate(44_101)));
        assert!(Resampler::new(100, 48_000).is_err());
    }

    #[test]
    fn dc_passes_at_unity_and_memory_stays_bounded() {
        let mut r = Resampler::new(44_100, 48_000).unwrap();
        let (mut out, block) = (Vec::new(), vec![0.25f32; 2 * 4_410]);
        for _ in 0..100 {
            r.process(&block, &mut out);
            out.clear();
            assert!(r.hist.len() < 2 * (4_410 + 2 * r.taps), "history grew to {}", r.hist.len());
        }
        r.process(&block, &mut out);
        let mid = out.len() / 2;
        assert!((out[mid] - 0.25).abs() < 1e-4);
    }
}
