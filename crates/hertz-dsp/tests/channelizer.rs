//! Channelizer integration tests (T2 brief item 3).
//!
//! - `detect_active_channels` finds exactly the active channels among probes.
//! - `extract_channel` yields a baseband tone at the expected frequency.

use hertz_dsp::channelizer::{
    design_lowpass_fir, detect_active_channels, estimate_noise_floor, extract_channel,
    ChannelProbe, FFT_DETECT_SIZE, LPF_CUTOFF_NORM, LPF_TAPS, WIDEBAND_DECIMATION, WIDEBAND_RATE,
};
use hertz_dsp::testutil::{audio_peak_hz, carrier, combine, white_noise};

/// Measure per-probe integrated power (dB) without any threshold, for testing.
fn probe_powers(
    wideband: &[num_complex::Complex32],
    probes: &[ChannelProbe],
    half_width_hz: f32,
) -> Vec<(u32, f32)> {
    use rustfft::FftPlanner;
    let fft_size = FFT_DETECT_SIZE.min(wideband.len());
    let mut buffer: Vec<num_complex::Complex32> = wideband[..fft_size].to_vec();
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(fft_size);
    fft.process(&mut buffer);
    let bin_width = WIDEBAND_RATE as f32 / fft_size as f32;
    let half_bins = (half_width_hz / bin_width) as usize;
    probes
        .iter()
        .map(|&(id, offset)| {
            let bin_idx = if offset >= 0.0 {
                (offset / bin_width) as usize
            } else {
                fft_size - ((-offset) / bin_width) as usize
            };
            let start = bin_idx.saturating_sub(half_bins);
            let end = (bin_idx + half_bins).min(fft_size - 1);
            let win = &buffer[start..=end];
            let power: f32 =
                win.iter().map(|c| c.norm_sqr()).sum::<f32>() / win.len().max(1) as f32;
            (id, 10.0 * (power + 1e-12).log10())
        })
        .collect()
}

#[test]
fn detect_finds_exactly_the_two_active_channels() {
    let n = 12_000;
    // Two carriers, each placed inside its own ±12.5 kHz channel slot.
    let ch_a_carrier = 105_000.0_f32; // inside channel A (center +100 kHz)
    let ch_b_carrier = -200_000.0_f32; // at channel B center (-200 kHz)
    let a = carrier(WIDEBAND_RATE, n, ch_a_carrier, 1.0);
    let b = carrier(WIDEBAND_RATE, n, ch_b_carrier, 1.0);
    // Realistic noise floor so the detector's median floor is meaningful and empty
    // channels sit at it (a noise-free wideband would leak sinc sidelobes everywhere).
    let noise = white_noise(WIDEBAND_RATE, n, 0.3, 7);
    let wideband = combine(&[&a, &b, &noise]);

    let probes: Vec<ChannelProbe> = vec![
        (1, 100_000.0),  // active (carrier at +105 kHz is within ±12.5 kHz)
        (2, -200_000.0), // active
        (3, 50_000.0),   // empty
        (4, -400_000.0), // empty
    ];

    // (a) Selectivity: active probes must read far above empty ones.
    let powers = probe_powers(&wideband, &probes, 12_500.0);
    let by_id: std::collections::HashMap<u32, f32> = powers.iter().cloned().collect();
    let p1 = by_id[&1];
    let p2 = by_id[&2];
    let p3 = by_id[&3];
    let p4 = by_id[&4];
    assert!(p1 - p3 > 15.0, "ch1 {p1:.1} not >> ch3 {p3:.1}");
    assert!(p1 - p4 > 15.0, "ch1 {p1:.1} not >> ch4 {p4:.1}");
    assert!(p2 - p3 > 15.0, "ch2 {p2:.1} not >> ch3 {p3:.1}");
    assert!(p2 - p4 > 15.0, "ch2 {p2:.1} not >> ch4 {p4:.1}");

    // (b) With the gnosis median floor + margin, exactly channels 1 and 2 fire.
    let nf = estimate_noise_floor(&wideband, WIDEBAND_RATE, FFT_DETECT_SIZE);
    let active = detect_active_channels(
        &wideband,
        WIDEBAND_RATE,
        &probes,
        12_500.0,
        nf,
        10.0,
        FFT_DETECT_SIZE,
    );
    let mut active_ids: Vec<u32> = active.iter().map(|(id, _)| *id).collect();
    active_ids.sort();
    assert_eq!(active_ids, vec![1, 2], "active channels: {:?}", active);
}

#[test]
fn extract_channel_yields_baseband_tone_at_offset() {
    let n = 12_000;
    // Carrier placed 5 kHz inside channel A (center +100 kHz): at +105 kHz.
    let sig = carrier(WIDEBAND_RATE, n, 105_000.0, 1.0);

    let fir = design_lowpass_fir(LPF_TAPS, LPF_CUTOFF_NORM);
    let mut mixer_phase = 0.0_f32;
    let baseband = extract_channel(
        &sig,
        100_000.0,
        WIDEBAND_RATE,
        WIDEBAND_DECIMATION,
        &fir,
        &mut mixer_phase,
    );

    // After shifting by −100 kHz and ÷10, the carrier lands at +5 kHz in the
    // 240 kHz baseband. The low-pass (120 kHz cutoff) keeps it.
    let out_rate = WIDEBAND_RATE / WIDEBAND_DECIMATION as u32;
    let (peak_hz, _mag) =
        audio_peak_hz(&baseband.iter().map(|c| c.re).collect::<Vec<_>>(), out_rate);
    assert!(
        (peak_hz - 5000.0).abs() < 200.0,
        "extracted baseband peak {peak_hz:.1} Hz, expected ~5000 Hz"
    );
}

#[test]
fn extract_channel_is_phase_continuous_across_buffers() {
    // Two back-to-back extracts must not glitch: the mixer phase carries over and the
    // resulting tone stays at a stable frequency with no discontinuity spike.
    let make = || carrier(WIDEBAND_RATE, 6_000, 30_000.0, 1.0);
    let s1 = make();
    let s2 = make();
    let fir = design_lowpass_fir(LPF_TAPS, LPF_CUTOFF_NORM);
    let mut phase = 0.0_f32;
    let bb1 = extract_channel(
        &s1,
        30_000.0,
        WIDEBAND_RATE,
        WIDEBAND_DECIMATION,
        &fir,
        &mut phase,
    );
    let bb2 = extract_channel(
        &s2,
        30_000.0,
        WIDEBAND_RATE,
        WIDEBAND_DECIMATION,
        &fir,
        &mut phase,
    );
    // Phase-continuity sanity: the first sample of bb2 should be close in magnitude
    // to the last sample of bb1 (no large jump from a phase reset).
    let last = bb1.last().unwrap().norm();
    let first = bb2.first().unwrap().norm();
    assert!(
        (last - first).abs() < 0.3,
        "phase discontinuity: last={last:.3} first={first:.3}"
    );
}
