//! Synthetic IQ generators — used by this crate's tests and by downstream
//! integration tests (the daemon, fixture pinning). Pure and dependency-free.
//!
//! All generators produce complex baseband samples at a given sample rate.

use std::f32::consts::TAU;

use num_complex::Complex32;

/// A small deterministic PRNG (xorshift32) so test signals are reproducible without
/// pulling a crate.
fn xorshift32(state: &mut u32) -> u32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    x
}

/// Uniform [-1, 1) from the PRNG.
fn rand(state: &mut u32) -> f32 {
    (xorshift32(state) as f32 / u32::MAX as f32) * 2.0 - 1.0
}

/// An FM-modulated complex signal.
///
/// Produces `n` samples at `sample_rate_hz`. The carrier is at `carrier_offset_hz`
/// from baseband center; the message is a sinusoid at `msg_freq_hz`; `peak_dev_hz` is
/// the peak frequency deviation (modulation index β = peak_dev / msg_freq).
pub fn fm_signal(
    sample_rate_hz: u32,
    n: usize,
    carrier_offset_hz: f32,
    msg_freq_hz: f32,
    peak_dev_hz: f32,
    amplitude: f32,
) -> Vec<Complex32> {
    let dt = 1.0 / sample_rate_hz as f32;
    // message phase = β sin(ωm t); instantaneous freq = carrier + peak_dev * cos(ωm t)
    let mut phase = 0.0_f32;
    let mut t = 0.0_f32;
    let carrier_step = TAU * carrier_offset_hz * dt;
    (0..n)
        .map(|_| {
            let msg = (TAU * msg_freq_hz * t).cos();
            let inst_freq = carrier_offset_hz + peak_dev_hz * msg;
            phase += TAU * inst_freq * dt;
            t += dt;
            // Ensure the per-step phase advance uses inst_freq, not carrier+msg double-count.
            let _ = carrier_step; // carrier folded into inst_freq above
            Complex32::from_polar(amplitude, phase)
        })
        .collect()
}

/// An FM signal modulated by a sum of sinusoids (richer harmonic content → lower
/// spectral flatness → more voice-like).
pub fn fm_multitone_signal(
    sample_rate_hz: u32,
    n: usize,
    carrier_offset_hz: f32,
    msg_freqs_hz: &[f32],
    peak_dev_hz: f32,
    amplitude: f32,
) -> Vec<Complex32> {
    let dt = 1.0 / sample_rate_hz as f32;
    let mut phase = 0.0_f32;
    let mut t = 0.0_f32;
    (0..n)
        .map(|_| {
            let msg: f32 = msg_freqs_hz
                .iter()
                .map(|&f| (TAU * f * t).cos())
                .sum::<f32>()
                / msg_freqs_hz.len().max(1) as f32;
            let inst_freq = carrier_offset_hz + peak_dev_hz * msg;
            phase += TAU * inst_freq * dt;
            t += dt;
            Complex32::from_polar(amplitude, phase)
        })
        .collect()
}

/// An AM-modulated complex signal: carrier at `carrier_offset_hz` with modulation
/// depth `modulation_depth` (0..1) by a sinusoid at `msg_freq_hz`.
pub fn am_signal(
    sample_rate_hz: u32,
    n: usize,
    carrier_offset_hz: f32,
    msg_freq_hz: f32,
    modulation_depth: f32,
    amplitude: f32,
) -> Vec<Complex32> {
    let dt = 1.0 / sample_rate_hz as f32;
    let mut t = 0.0_f32;
    let out: Vec<Complex32> = (0..n)
        .map(|_| {
            let envelope = amplitude * (1.0 + modulation_depth * (TAU * msg_freq_hz * t).cos());
            let carrier = Complex32::from_polar(1.0, TAU * carrier_offset_hz * t);
            t += dt;
            envelope * carrier
        })
        .collect();
    out
}

/// An unmodulated carrier at `offset_hz`.
pub fn carrier(sample_rate_hz: u32, n: usize, offset_hz: f32, amplitude: f32) -> Vec<Complex32> {
    let dt = 1.0 / sample_rate_hz as f32;
    let mut t = 0.0_f32;
    (0..n)
        .map(|_| {
            let s = Complex32::from_polar(amplitude, TAU * offset_hz * t);
            t += dt;
            s
        })
        .collect()
}

/// Complex white noise (deterministic, seeded).
pub fn white_noise(_sample_rate_hz: u32, n: usize, amplitude: f32, seed: u64) -> Vec<Complex32> {
    let mut state = seed as u32 | 1;
    (0..n)
        .map(|_| {
            let re = rand(&mut state) * amplitude;
            let im = rand(&mut state) * amplitude;
            Complex32::new(re, im)
        })
        .collect()
}

/// Sum several signals sample-wise (they must be equal length).
pub fn combine(signals: &[&[Complex32]]) -> Vec<Complex32> {
    if signals.is_empty() {
        return Vec::new();
    }
    let len = signals[0].len();
    let mut out = vec![Complex32::new(0.0, 0.0); len];
    for s in signals {
        for (o, &x) in out.iter_mut().zip(*s) {
            *o += x;
        }
    }
    out
}

/// Find the dominant peak frequency of a real-valued audio buffer via zero-padded
/// FFT, returning `(peak_hz, magnitude)`. Useful for asserting demod output tone.
pub fn audio_peak_hz(audio: &[f32], sample_rate_hz: u32) -> (f32, f32) {
    use rustfft::FftPlanner;
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(audio.len());
    let mut buf: Vec<Complex32> = audio.iter().map(|&s| Complex32::new(s, 0.0)).collect();
    fft.process(&mut buf);
    let half = buf.len() / 2;
    let (idx, &mag) = buf[..half]
        .iter()
        .enumerate()
        .max_by(|a, b| {
            a.1.norm()
                .partial_cmp(&b.1.norm())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or((0, &Complex32::new(0.0, 0.0)));
    let bin_width = sample_rate_hz as f32 / audio.len() as f32;
    (idx as f32 * bin_width, mag.norm())
}
