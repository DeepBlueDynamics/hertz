//! Channelizer — ported from `plan/reference/gnosis-radio/src/wideband.rs` DSP core.
//!
//! Differences vs gnosis:
//! - **Generic** over sample rate and decimation factor (`extract_channel`,
//!   `detect_active_channels`, `estimate_noise_floor` take them as parameters) rather
//!   than hardcoded to 2.4 MHz / ÷10.
//! - **f32 IQ end-to-end**: [`extract_channel`] returns `Vec<Complex32>`; the gnosis
//!   u8 round-trip between channelizer and pipeline is gone. [`bytes_to_iq`] is the
//!   single edge helper that turns raw dongle u8 into f32 IQ.
//! - Channel ids are `u32` (any bandplan key), not `u8` marine-only.
//! - FFT activity detection is parameterized by a per-channel half-width in Hz.

use std::f32::consts::PI;

use num_complex::Complex32;
use rustfft::FftPlanner;

// ---- gnosis default wideband constants (kept as reference defaults) -----

/// gnosis wideband capture rate.
pub const WIDEBAND_RATE: u32 = 2_400_000;
/// gnosis wideband center (midpoint of US marine VHF band).
pub const WIDEBAND_CENTER: u32 = 156_737_500;
/// gnosis wideband decimation (2.4 MHz → 240 kHz per channel).
pub const WIDEBAND_DECIMATION: usize = 10;
/// gnosis FFT detection size.
pub const FFT_DETECT_SIZE: usize = 8192;
/// gnosis wideband channel low-pass taps.
pub const LPF_TAPS: usize = 51;
/// gnosis wideband channel normalized cutoff (120 kHz / 2.4 MHz).
pub const LPF_CUTOFF_NORM: f32 = 0.05;
/// Half-width of a 25 kHz channel.
pub const CHANNEL_HALF_WIDTH_HZ: f32 = 12_500.0;

/// Design a windowed-sinc low-pass FIR (Hann window), normalized to unit DC gain.
/// `cutoff_norm` is the cutoff frequency normalized to the sample rate (0..0.5).
pub fn design_lowpass_fir(num_taps: usize, cutoff_norm: f32) -> Vec<f32> {
    let center = (num_taps - 1) as f32 / 2.0;
    let mut coeffs = Vec::with_capacity(num_taps);
    for i in 0..num_taps {
        let n = i as f32 - center;
        let sinc = if n.abs() < 1e-6 {
            2.0 * cutoff_norm
        } else {
            (2.0 * PI * cutoff_norm * n).sin() / (PI * n)
        };
        let window = 0.5 * (1.0 - (2.0 * PI * i as f32 / (num_taps - 1) as f32).cos());
        coeffs.push(sinc * window);
    }
    let sum: f32 = coeffs.iter().sum();
    for c in &mut coeffs {
        *c /= sum.max(1e-12);
    }
    coeffs
}

/// Convert raw dongle u8 IQ bytes to complex f32 (the edge conversion; not used
/// between channelizer and pipeline, which stay in f32).
pub fn bytes_to_iq(buffer: &[u8]) -> Vec<Complex32> {
    buffer
        .chunks_exact(2)
        .map(|chunk| {
            let i = (chunk[0] as f32 - 127.5) / 127.5;
            let q = (chunk[1] as f32 - 127.5) / 127.5;
            Complex32::new(i, q)
        })
        .collect()
}

/// Extract one channel from wideband IQ: phase-continuous complex mixer +
/// polyphase decimating FIR. Returns baseband complex f32 at
/// `sample_rate_hz / decimation`. `mixer_phase` is carried across calls so the
/// frequency shift is phase-continuous across buffers (gnosis behaviour).
pub fn extract_channel(
    wideband: &[Complex32],
    offset_hz: f32,
    sample_rate_hz: u32,
    decimation: usize,
    filter_coeffs: &[f32],
    mixer_phase: &mut f32,
) -> Vec<Complex32> {
    let phase_step = -2.0 * PI * offset_hz / sample_rate_hz as f32;
    let half_len = filter_coeffs.len() / 2;

    // Frequency-shift to baseband.
    let mut shifted = Vec::with_capacity(wideband.len());
    let mut phase = *mixer_phase;
    for &sample in wideband {
        let mixer = Complex32::from_polar(1.0, phase);
        shifted.push(sample * mixer);
        phase += phase_step;
        if phase > PI {
            phase -= 2.0 * PI;
        } else if phase < -PI {
            phase += 2.0 * PI;
        }
    }
    *mixer_phase = phase;

    // FIR filter + decimate (polyphase: only compute at output positions).
    let mut out = Vec::with_capacity(wideband.len() / decimation.max(1) + 1);
    let mut pos = half_len;
    while pos + half_len < shifted.len() {
        let mut sum = Complex32::new(0.0, 0.0);
        for (j, &coeff) in filter_coeffs.iter().enumerate() {
            sum += shifted[pos + j - half_len] * coeff;
        }
        out.push(sum);
        pos += decimation;
    }
    out
}

/// A channel probe for [`detect_active_channels`]: an opaque id plus its offset from
/// the capture center in Hz.
pub type ChannelProbe = (u32, f32);

/// Detect which channels have energy above `noise_floor_db + squelch_margin_db` using
/// an FFT, integrating power over `±channel_half_width_hz` around each probe offset.
/// Returns `(id, power_db)` for each active channel.
pub fn detect_active_channels(
    wideband: &[Complex32],
    sample_rate_hz: u32,
    channels: &[ChannelProbe],
    channel_half_width_hz: f32,
    noise_floor_db: f32,
    squelch_margin_db: f32,
    fft_size: usize,
) -> Vec<(u32, f32)> {
    let fft_size = fft_size.min(wideband.len());
    if fft_size == 0 {
        return Vec::new();
    }
    let mut buffer: Vec<Complex32> = wideband[..fft_size].to_vec();
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(fft_size);
    fft.process(&mut buffer);

    let bin_width = sample_rate_hz as f32 / fft_size as f32;
    let channel_half_bins = (channel_half_width_hz / bin_width) as usize;
    let thresh = noise_floor_db + squelch_margin_db;
    let nyquist = sample_rate_hz as f32 / 2.0;

    let mut active = Vec::new();
    for &(id, offset) in channels {
        // A channel outside the capture span can never be detected here; probing it
        // would index past the FFT (bandplans may list channels beyond this capture).
        if offset.abs() + channel_half_width_hz >= nyquist {
            continue;
        }
        let bin_idx = if offset >= 0.0 {
            (offset / bin_width) as usize
        } else {
            fft_size - ((-offset) / bin_width) as usize
        };
        let bin_idx = bin_idx.min(fft_size - 1);
        let start = bin_idx.saturating_sub(channel_half_bins);
        let end = (bin_idx + channel_half_bins).min(fft_size - 1);
        let mut power = 0.0f32;
        let window = &buffer[start..=end];
        let count = window.len();
        for c in window {
            power += c.norm_sqr();
        }
        if count == 0 {
            continue;
        }
        let power_db = 10.0 * (power / count as f32 + 1e-12).log10();
        if power_db > thresh {
            active.push((id, power_db));
        }
    }
    active
}

/// Estimate the wideband noise floor (median of FFT-bin power in dB).
pub fn estimate_noise_floor(wideband: &[Complex32], _sample_rate_hz: u32, fft_size: usize) -> f32 {
    let fft_size = fft_size.min(wideband.len());
    if fft_size == 0 {
        return -100.0;
    }
    let mut buffer: Vec<Complex32> = wideband[..fft_size].to_vec();
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(fft_size);
    fft.process(&mut buffer);
    let mut mags: Vec<f32> = buffer
        .iter()
        .map(|c| 10.0 * (c.norm_sqr() + 1e-12).log10())
        .collect();
    mags.sort_by(|a, b| a.total_cmp(b));
    mags[mags.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fir_is_unit_gain() {
        let coeffs = design_lowpass_fir(81, 0.05);
        let sum: f32 = coeffs.iter().sum();
        assert!((sum - 1.0).abs() < 1e-5, "FIR DC gain = {sum}, want 1.0");
    }

    #[test]
    fn bytes_to_iq_roundtrip() {
        // gnosis mapping: (x - 127.5) / 127.5 → [-1.0, +1.0].
        let bytes = [0u8, 128, 255, 0, 127, 128];
        let iq = bytes_to_iq(&bytes);
        assert_eq!(iq.len(), 3);
        // [0,128]   -> re=(0-127.5)/127.5=-1.0, im=(128-127.5)/127.5≈0.0039
        assert!((iq[0].re - (-1.0)).abs() < 1e-3);
        assert!(iq[0].im.abs() < 1e-2);
        // [255,0]   -> re=(255-127.5)/127.5=+1.0, im=(0-127.5)/127.5=-1.0
        assert!((iq[1].re - 1.0).abs() < 1e-3);
        assert!((iq[1].im - (-1.0)).abs() < 1e-3);
        // [127,128] -> mid-range both axes ~0
        assert!(iq[2].re.abs() < 1e-2 && iq[2].im.abs() < 1e-2);
    }

    #[test]
    fn out_of_span_probe_is_skipped_not_panicking() {
        // Regression: marine-vhf-us includes NOAA WX channels ~5.7 MHz from the
        // capture center — far outside a 2.4 MHz span. Probing one must be a no-op,
        // not an out-of-range slice (panicked live at bin 19584 of 8192).
        let wideband = vec![num_complex::Complex32::new(0.01, 0.0); 8192];
        let probes = vec![
            (16u32, 62_500.0f32),      // in-span
            (9993u32, 5_737_500.0f32), // WX3: beyond Nyquist for 2.4 MS/s
            (9994u32, -5_737_500.0f32),
        ];
        let active =
            detect_active_channels(&wideband, 2_400_000, &probes, 12_500.0, -60.0, 12.0, 8192);
        assert!(active.iter().all(|(id, _)| *id == 16));
    }
}
