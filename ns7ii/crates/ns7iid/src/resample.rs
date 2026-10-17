//! Band-limited resampling for the playback bridge.
//!
//! The app's audio arrives on the loopback's clock (the kernel's timer) at
//! the app's rate; the NS7II plays it on its own crystal at 44.1 kHz. A
//! polyphase windowed-sinc interpolator (64 taps, Kaiser window, 512 phases
//! with linear interpolation between neighbouring phases, every phase
//! normalised to unity DC gain) converts between the two, and its ratio can
//! be nudged continuously by the drift controller without clicks: the
//! fractional read position simply advances a little faster or slower.
//!
//! Samples are the codecs' 24-bit-in-the-top-of-an-i32 layout. Output is
//! clamped to full scale before it is turned back into integers, so the
//! filter's overshoot on clipped material can never wrap around.

use std::f64::consts::PI;

const TAPS: usize = 64;
const HALF: usize = TAPS / 2;
const PHASES: usize = 512;
const KAISER_BETA: f64 = 7.0;

/// Multi-channel resampler with an adjustable ratio.
pub struct Resampler {
    ch: usize,
    table: Vec<f32>,
    hist: Vec<f32>,
    pos: f64,
    nominal: f64,
    step: f64,
    coef: Vec<f32>,
}

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
    while term > 1e-12 * sum {
        term *= (x / (2.0 * k)).powi(2);
        sum += term;
        k += 1.0;
    }
    sum
}

impl Resampler {
    /// Convert `channels`-channel audio from `in_rate` to `out_rate`.
    pub fn new(channels: usize, in_rate: u32, out_rate: u32) -> Self {
        let nominal = in_rate as f64 / out_rate as f64;
        // Cutoff in cycles per input sample: just below the lower Nyquist.
        let fc = 0.5 * (1.0 / nominal).min(1.0) * 0.91;
        let mut table = vec![0f32; (PHASES + 1) * TAPS];
        let i0b = bessel_i0(KAISER_BETA);
        for p in 0..=PHASES {
            let frac = p as f64 / PHASES as f64;
            let row = &mut table[p * TAPS..(p + 1) * TAPS];
            let mut sum = 0.0;
            let mut vals = [0f64; TAPS];
            for (k, v) in vals.iter_mut().enumerate() {
                // Tap k multiplies input sample i + k - HALF + 1; distance to the output point.
                let x = (k as f64 - HALF as f64 + 1.0) - frac;
                let sinc = if x.abs() < 1e-12 { 1.0 } else { (2.0 * PI * fc * x).sin() / (PI * x) / (2.0 * fc) };
                let r = x / HALF as f64;
                let w = if r.abs() >= 1.0 { 0.0 } else { bessel_i0(KAISER_BETA * (1.0 - r * r).sqrt()) / i0b };
                *v = sinc * w;
                sum += *v;
            }
            for (o, v) in row.iter_mut().zip(vals.iter()) {
                *o = (*v / sum) as f32;
            }
        }
        Self {
            ch: channels,
            table,
            hist: vec![0.0; (HALF - 1) * channels],
            pos: (HALF - 1) as f64,
            nominal,
            step: nominal,
            coef: vec![0.0; TAPS],
        }
    }

    /// Run `correction` faster (positive: consume input faster) than the
    /// nominal ratio, e.g. 100e-6 for +100 ppm.
    pub fn set_correction(&mut self, correction: f64) {
        self.step = self.nominal * (1.0 + correction);
    }

    /// Input frames per output frame right now.
    #[cfg(test)]
    pub fn step(&self) -> f64 {
        self.step
    }

    /// Feed interleaved input frames and append every output frame that can
    /// be computed to `out`.
    pub fn process(&mut self, input: &[i32], out: &mut Vec<i32>) {
        let ch = self.ch;
        self.hist.extend(input.iter().map(|s| *s as f32 / 2_147_483_648.0));
        let frames = self.hist.len() / ch;
        while (self.pos.floor() as usize) + HALF < frames {
            let i = self.pos.floor() as usize;
            let f = (self.pos - i as f64) * PHASES as f64;
            let p = f.floor() as usize;
            let w = (f - p as f64) as f32;
            let (a, b) = (&self.table[p * TAPS..(p + 1) * TAPS], &self.table[(p + 1) * TAPS..(p + 2) * TAPS]);
            for ((c, x), y) in self.coef.iter_mut().zip(a).zip(b) {
                *c = x + (y - x) * w;
            }
            let start = (i + 1 - HALF) * ch;
            for c in 0..ch {
                let mut acc = 0f32;
                for (k, h) in self.coef.iter().enumerate() {
                    acc += h * self.hist[start + k * ch + c];
                }
                let v = acc.clamp(-1.0, 8_388_607.0 / 8_388_608.0);
                out.push(((v * 8_388_608.0).round() as i32).clamp(-8_388_608, 8_388_607) << 8);
            }
            self.pos += self.step;
        }
        // Drop history the next output no longer needs.
        let keep_from = (self.pos.floor() as usize + 1).saturating_sub(HALF);
        if keep_from > 0 {
            self.hist.drain(..keep_from * ch);
            self.pos -= keep_from as f64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: f64, n: usize, amp: f64) -> Vec<i32> {
        (0..n).map(|i| (((2.0 * PI * freq * i as f64 / rate).sin() * amp * 8_388_607.0).round() as i32) << 8).collect()
    }

    /// RMS error (dB relative to the signal) of the output against the
    /// ideal sine sampled at the positions the resampler used.
    fn error_db(freq: f64, in_rate: u32, out_rate: u32, correction: f64) -> f64 {
        let mut r = Resampler::new(1, in_rate, out_rate);
        r.set_correction(correction);
        let input = sine(freq, in_rate as f64, 40_000, 0.5);
        let mut out = Vec::new();
        for chunk in input.chunks(333) {
            r.process(chunk, &mut out);
        }
        let step = r.step();
        let (mut err, mut sig) = (0.0, 0.0);
        for (m, y) in out.iter().enumerate().skip(2000).take(30_000) {
            // Output m sits at input position m * step (the first output is input sample 0).
            let t = m as f64 * step;
            let ideal = (2.0 * PI * freq * t / in_rate as f64).sin() * 0.5;
            let got = (*y >> 8) as f64 / 8_388_607.0;
            err += (got - ideal).powi(2);
            sig += ideal.powi(2);
        }
        10.0 * (err / sig).log10()
    }

    #[test]
    fn drift_correction_is_clean_across_the_band() {
        // Measured: about -85 dB at every frequency (24-bit output, f32 math).
        assert!(error_db(1000.0, 44_100, 44_100, 300e-6) < -80.0);
        assert!(error_db(10_000.0, 44_100, 44_100, -300e-6) < -80.0);
        assert!(error_db(16_000.0, 44_100, 44_100, 2000e-6) < -80.0);
    }

    #[test]
    fn converts_48k_to_44k1_cleanly() {
        assert!(error_db(1000.0, 48_000, 44_100, 0.0) < -80.0);
        assert!(error_db(15_000.0, 48_000, 44_100, 100e-6) < -80.0);
    }

    #[test]
    fn output_count_follows_the_ratio() {
        let mut r = Resampler::new(4, 48_000, 44_100);
        let input = vec![0i32; 48_000 * 4];
        let mut out = Vec::new();
        r.process(&input, &mut out);
        let frames = out.len() / 4;
        assert!((44_100 - 40..=44_100).contains(&frames), "{frames}");
        let mut r = Resampler::new(2, 44_100, 44_100);
        r.set_correction(1e-3);
        let mut out = Vec::new();
        r.process(&vec![0i32; 441_000 * 2], &mut out);
        let frames = out.len() / 2;
        assert!((440_559 - 40..=440_560 + 1).contains(&frames), "{frames}");
    }

    #[test]
    fn full_scale_square_waves_clip_instead_of_wrapping() {
        let mut r = Resampler::new(1, 44_100, 44_100);
        r.set_correction(500e-6);
        let input: Vec<i32> = (0..10_000).map(|i| if (i / 20) % 2 == 0 { 0x7FFF_FF00 } else { -0x8000_0000 }).collect();
        let mut out = Vec::new();
        r.process(&input, &mut out);
        // Overshoot clamps at full scale; a wrap would flip the sign mid-plateau.
        for w in out.windows(2).skip(100) {
            let jump = ((w[0] >> 8) - (w[1] >> 8)).abs();
            assert!(jump <= 16_777_215);
        }
        assert!(out.iter().all(|s| s & 0xFF == 0));
        let peak_pos = out.iter().max().unwrap() >> 8;
        assert_eq!(peak_pos, 8_388_607);
    }

    #[test]
    fn channels_stay_separate() {
        let mut r = Resampler::new(2, 44_100, 44_100);
        let input: Vec<i32> = (0..4000).flat_map(|_| [0x4000_0000, 0]).collect();
        let mut out = Vec::new();
        r.process(&input, &mut out);
        for f in out.chunks(2).skip(100) {
            assert!((f[0] - 0x4000_0000).abs() < 0x1000, "{:x}", f[0]);
            assert_eq!(f[1], 0);
        }
    }
}
