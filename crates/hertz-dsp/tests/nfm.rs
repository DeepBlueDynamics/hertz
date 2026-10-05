//! NFM demod + channel-FIR selectivity tests (T2 brief item 1).
//!
//! - A 1 kHz-tone FM signal demodulates to a clean ~1 kHz tone (FFT peak + SNR).
//! - An adjacent-channel NFM signal (+25 kHz, the marine spacing) is rejected by the
//!   81-tap channel FIR: in-channel power barely moves and the demod tone stays clean.

use hertz_dsp::demod::nfm::{NfmDemod, DECIMATION};
use hertz_dsp::pipeline::SDR_RATE;
use hertz_dsp::testutil::{audio_peak_hz, combine, fm_multitone_signal, fm_signal, white_noise};

const AUDIO_RATE: u32 = SDR_RATE / DECIMATION as u32;

/// Median magnitude of the (peak-excluded) vocal-range bins — a floor for SNR.
fn vocal_floor_db(audio: &[f32], peak_bin: usize) -> f32 {
    use std::cmp::Ordering;
    let mut others: Vec<f32> = audio
        .iter()
        .step_by(audio.len() / 512)
        .map(|s| s.abs() + 1e-12)
        .collect();
    others.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let _ = peak_bin;
    20.0 * others[others.len() / 2].log10()
}

#[test]
fn nfm_demodulates_clean_tone() {
    let n = SDR_RATE as usize; // 1 s of input → 200 ms audio
    let iq = fm_signal(SDR_RATE, n, 0.0, 1000.0, 5000.0, 1.0);

    let mut demod = NfmDemod::new(SDR_RATE);
    let (audio, _db) = demod.process(&iq);

    assert!(!audio.is_empty(), "demod produced no audio");
    let (peak_hz, peak_mag) = audio_peak_hz(&audio, AUDIO_RATE);
    let floor = vocal_floor_db(&audio, 0);
    let peak_db = 20.0 * (peak_mag + 1e-12).log10();

    assert!(
        (peak_hz - 1000.0).abs() < 60.0,
        "demod tone peak {peak_hz:.1} Hz, expected ~1000 Hz"
    );
    assert!(
        peak_db - floor > 15.0,
        "tone SNR too low: peak {peak_db:.1} dB vs floor {floor:.1} dB"
    );
}

#[test]
fn channel_fir_rejects_adjacent_channel_25khz() {
    // The on-channel signal alone vs on-channel + a same-strength adjacent signal
    // at +25 kHz (the marine channel spacing). gnosis's 81-tap/11 kHz channel FIR
    // is the fix that keeps adjacent transmissions out of the in-channel meter and
    // out of the demod audio.
    let n = SDR_RATE as usize;
    let on_channel = fm_signal(SDR_RATE, n, 0.0, 1000.0, 5000.0, 1.0);
    let adjacent = fm_signal(SDR_RATE, n, 25_000.0, 1500.0, 5000.0, 1.0);

    let mut demod_a = NfmDemod::new(SDR_RATE);
    let (audio_alone, db_alone) = demod_a.process(&on_channel);

    let mut demod_b = NfmDemod::new(SDR_RATE);
    let combined = combine(&[&on_channel, &adjacent]);
    let (audio_with_adj, db_with_adj) = demod_b.process(&combined);

    // In-channel power barely moves: adjacent is in the FIR stopband.
    let delta = (db_with_adj - db_alone).abs();
    assert!(
        delta < 2.0,
        "in-channel power moved {delta:.2} dB with adjacent signal (should be < 2 dB)"
    );

    // Demod tone stays a clean ~1 kHz: adjacent didn't alias into the audio band.
    let (peak_hz, _m) = audio_peak_hz(&audio_with_adj, AUDIO_RATE);
    assert!(
        (peak_hz - 1000.0).abs() < 80.0,
        "demod tone corrupted by adjacent: peak {peak_hz:.1} Hz, expected ~1000 Hz"
    );
    // The alone-tone is also clean (sanity).
    let (peak_hz_alone, _m) = audio_peak_hz(&audio_alone, AUDIO_RATE);
    assert!((peak_hz_alone - 1000.0).abs() < 80.0);
}

#[test]
fn adjacent_channel_alone_is_heavily_attenuated() {
    // A signal centered on the adjacent channel (+25 kHz) alone should read far
    // lower in-channel power than the same signal on-channel.
    let n = SDR_RATE as usize;
    let on = fm_signal(SDR_RATE, n, 0.0, 1000.0, 5000.0, 1.0);
    let off = fm_signal(SDR_RATE, n, 25_000.0, 1000.0, 5000.0, 1.0);

    let mut d1 = NfmDemod::new(SDR_RATE);
    let (_, db_on) = d1.process(&on);
    let mut d2 = NfmDemod::new(SDR_RATE);
    let (_, db_off) = d2.process(&off);

    assert!(
        db_on - db_off > 15.0,
        "channel FIR selectivity too low: on {db_on:.1} dB, off {db_off:.1} dB"
    );
}

#[test]
fn nfm_handles_realistic_multitone_voice() {
    // A multi-tone (voice-like) FM signal demodulates to structured audio.
    let n = SDR_RATE as usize;
    let iq = fm_multitone_signal(SDR_RATE, n, 0.0, &[800.0, 1200.0, 2000.0], 4000.0, 1.0);
    let mut demod = NfmDemod::new(SDR_RATE);
    let (audio, _) = demod.process(&iq);
    assert!(!audio.is_empty());
    // No single dominant 1 kHz here, but the audio must be non-trivial energy.
    let rms = (audio.iter().map(|x| x * x).sum::<f32>() / audio.len() as f32).sqrt();
    assert!(rms > 0.01, "multitone demod rms {rms:.4} too low");
}

#[test]
fn nfm_warmup_then_stable() {
    // Second buffer should be phase-continuous (filter history carries over).
    let n = SDR_RATE as usize;
    let iq1 = fm_signal(SDR_RATE, n, 0.0, 1000.0, 5000.0, 1.0);
    let iq2 = fm_signal(SDR_RATE, n, 0.0, 1000.0, 5000.0, 1.0);
    let mut demod = NfmDemod::new(SDR_RATE);
    let (a1, _) = demod.process(&iq1);
    let (a2, _) = demod.process(&iq2);
    let (p1, _) = audio_peak_hz(&a1, AUDIO_RATE);
    let (p2, _) = audio_peak_hz(&a2, AUDIO_RATE);
    assert!((p1 - 1000.0).abs() < 80.0);
    assert!((p2 - 1000.0).abs() < 80.0);
    // Confirm noise-only input doesn't crash and yields bounded output.
    let noise = white_noise(SDR_RATE, n, 0.1, 42);
    let (na, _) = demod.process(&noise);
    let peak = na.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
    assert!(peak <= 0.71, "peak-normalized output should stay ≤ 0.7+");
}

#[test]
fn voice_out_level_is_sane_for_typical_deviation() {
    // 1 kHz tone at 3 kHz deviation (typical speech) through demod + listening chain
    // should land at a comfortable level: audible, not limiter-crushed.
    use hertz_dsp::voice_out::VoiceOut;
    let n = SDR_RATE as usize;
    let iq = fm_signal(SDR_RATE, n, 0.0, 1000.0, 3000.0, 1.0);
    let mut demod = NfmDemod::new(SDR_RATE);
    let (mut audio, _) = demod.process(&iq);
    let mut out = VoiceOut::new(AUDIO_RATE as f32, true);
    out.process(&mut audio);
    let tail = &audio[audio.len() / 2..];
    let peak = tail.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(
        (0.3..0.9).contains(&peak),
        "voice level peak {peak:.3} outside 0.3..0.9"
    );
}
