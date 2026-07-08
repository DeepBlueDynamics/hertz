//! Entropy squelch + prebuffer + hang + TransmissionComplete (T2 brief item 1/squelch).
//!
//! - Noise → stays closed.
//! - Voice-like FM signal → opens within N frames; first `Audio` event carries the
//!   drained prebuffer.
//! - Back to noise → closes after hang; exactly one `TransmissionComplete`.

use hertz_dsp::pipeline::{AudioKind, Pipeline, PipelineConfig, PipelineEvent, SDR_RATE};
use hertz_dsp::testutil::{fm_multitone_signal, white_noise};
use hertz_dsp::Mode;

/// One frame = 200 ms of 240 kS/s IQ = 48 000 IQ pairs.
const FRAME_IQ: usize = SDR_RATE as usize / 5;

fn noise_frame(seed: u64) -> Vec<num_complex::Complex32> {
    white_noise(SDR_RATE, FRAME_IQ, 0.05, seed)
}

fn voice_frame(seed: u64) -> Vec<num_complex::Complex32> {
    // Multi-tone FM → structured demod audio → low spectral flatness.
    let base = fm_multitone_signal(
        SDR_RATE,
        FRAME_IQ,
        0.0,
        &[700.0, 1100.0, 1900.0],
        4500.0,
        1.0,
    );
    // Slight per-frame variation so it's not perfectly periodic.
    let noise = white_noise(SDR_RATE, FRAME_IQ, 0.01, seed);
    base.iter().zip(&noise).map(|(b, n)| b + n).collect()
}

#[test]
fn noise_keeps_squelch_closed() {
    let mut p = Pipeline::new(PipelineConfig {
        mode: Mode::Nfm,
        squelch_margin_db: 6.0,
        ..PipelineConfig::default()
    });
    for f in 0..15 {
        let ev = p.process_buffer(&noise_frame(100 + f));
        for e in &ev {
            assert!(
                !matches!(e, PipelineEvent::SquelchOpened { .. }),
                "squelch opened on pure noise at frame {f}"
            );
        }
    }
    assert!(!p.is_squelch_open(), "squelch should stay closed on noise");
}

#[test]
fn voice_opens_and_prebuffer_appears_first() {
    let mut p = Pipeline::new(PipelineConfig {
        mode: Mode::Nfm,
        squelch_margin_db: 6.0,
        ..PipelineConfig::default()
    });

    // Fill the prebuffer with noise first (so there's content to drain on open).
    for f in 0..8 {
        p.process_buffer(&noise_frame(200 + f));
    }
    assert!(!p.is_squelch_open());

    // Now feed voice frames until it opens.
    let mut opened_at = None;
    let mut first_audio_kind = None;
    let mut first_audio_len = 0;
    for f in 0..12 {
        let ev = p.process_buffer(&voice_frame(300 + f));
        if let Some(PipelineEvent::SquelchOpened { .. }) = ev.first() {
            opened_at = Some(f);
            // The Audio event immediately following should be the prebuffer drain.
            for e in &ev {
                if let PipelineEvent::Audio { samples, kind, .. } = e {
                    first_audio_kind = Some(*kind);
                    first_audio_len = samples.len();
                    break;
                }
            }
        }
    }

    let opened_at = opened_at.expect("squelch never opened on voice");
    assert!(opened_at <= 5, "opened at frame {opened_at}, expected ≤ 5");
    assert_eq!(
        first_audio_kind,
        Some(AudioKind::Prebuffer),
        "first Audio after open should be the prebuffer"
    );
    // Prebuffer is 1.5 s of audio = 72 000 samples at 48 kHz.
    assert!(
        first_audio_len >= 48_000,
        "prebuffer drain too small: {first_audio_len} samples"
    );
}

#[test]
fn full_transmission_lifecycle() {
    let mut p = Pipeline::new(PipelineConfig {
        mode: Mode::Nfm,
        squelch_margin_db: 6.0,
        ..PipelineConfig::default()
    });

    // 1) noise -> closed
    for f in 0..6 {
        p.process_buffer(&noise_frame(500 + f));
    }
    // 2) voice -> open
    let mut opened = 0;
    let mut audio_events_during_open = 0;
    for f in 0..15 {
        let ev = p.process_buffer(&voice_frame(600 + f));
        if ev
            .iter()
            .any(|e| matches!(e, PipelineEvent::SquelchOpened { .. }))
        {
            opened += 1;
        }
        audio_events_during_open += ev
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    PipelineEvent::Audio {
                        kind: AudioKind::Transmission,
                        ..
                    }
                )
            })
            .count();
    }
    assert_eq!(opened, 1, "squelch should open exactly once");
    assert!(
        audio_events_during_open > 0,
        "no transmission audio emitted"
    );
    assert!(p.is_squelch_open());

    // 3) back to noise -> closes after hang, emits TransmissionComplete exactly once.
    let mut closed = 0;
    let mut complete = 0;
    let mut complete_samples = 0;
    for f in 0..25 {
        let ev = p.process_buffer(&noise_frame(700 + f));
        if ev
            .iter()
            .any(|e| matches!(e, PipelineEvent::SquelchClosed { .. }))
        {
            closed += 1;
        }
        for e in &ev {
            if let PipelineEvent::TransmissionComplete(sum) = e {
                complete += 1;
                complete_samples = sum.samples.len();
            }
        }
    }
    assert_eq!(closed, 1, "squelch should close exactly once");
    assert_eq!(complete, 1, "should emit exactly one TransmissionComplete");
    assert!(
        complete_samples >= 48_000,
        "transmission sample count {complete_samples} too small"
    );
    assert!(!p.is_squelch_open(), "squelch should be closed at end");
}
