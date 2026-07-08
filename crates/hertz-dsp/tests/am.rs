//! AM demod tests (T2 brief item 7 / NEW).
//!
//! - A modulated AM carrier demodulates to a clean tone at the message frequency.
//! - A carrier-only AM signal (no modulation) classifies as Carrier via the pipeline
//!   (AM squelch/classify is signal_db-driven since a pure carrier demodulates to
//!   silence).

use hertz_dsp::demod::am::AmDemod;
use hertz_dsp::demod::Mode;
use hertz_dsp::pipeline::{Pipeline, PipelineConfig, PipelineEvent, SDR_RATE};
use hertz_dsp::testutil::{am_signal, audio_peak_hz, carrier};
use hertz_dsp::Complex32;

const DECIMATION: usize = 5;
const AUDIO_RATE: u32 = SDR_RATE / DECIMATION as u32;
const FRAME_IQ: usize = SDR_RATE as usize / 5;

#[test]
fn am_demodulates_modulated_tone() {
    let n = SDR_RATE as usize;
    let iq = am_signal(SDR_RATE, n, 0.0, 1000.0, 0.5, 1.0);

    let mut demod = AmDemod::new(SDR_RATE);
    let (audio, db) = demod.process(&iq);

    assert!(!audio.is_empty());
    assert!(
        db > -40.0,
        "AM in-channel power {db:.1} dB should indicate a carrier"
    );
    let (peak_hz, peak_mag) = audio_peak_hz(&audio, AUDIO_RATE);
    assert!(
        (peak_hz - 1000.0).abs() < 80.0,
        "AM demod tone peak {peak_hz:.1} Hz, expected ~1000 Hz"
    );
    // Tone magnitude should be well above zero.
    assert!(peak_mag > 0.05, "AM demod tone too weak: {peak_mag:.4}");
}

#[test]
fn am_carrier_only_classifies_as_carrier() {
    // Pure AM carrier (no modulation) through the pipeline. AM classify is
    // signal_db-driven (a pure carrier demodulates to silence), so it should open on
    // carrier power and classify as Carrier — not Voice, not Static.
    let mut p = Pipeline::new(PipelineConfig {
        mode: Mode::Am,
        squelch_margin_db: 6.0,
        initial_noise_floor_db: -32.0,
        ..PipelineConfig::default()
    });

    let iq: Vec<Complex32> = carrier(SDR_RATE, FRAME_IQ, 0.0, 1.0);

    // Feed the carrier for several frames to let the floor settle and squelch open.
    let mut classification = None;
    for _ in 0..8 {
        let ev = p.process_buffer(&iq);
        for e in &ev {
            if let PipelineEvent::SquelchOpened {
                classification: c, ..
            } = e
            {
                classification = Some(*c);
            }
        }
    }
    let c = classification.expect("AM carrier should open the squelch");
    assert!(
        matches!(c, hertz_dsp::classify::SignalClassification::Carrier),
        "AM carrier classified as {c:?}, expected Carrier"
    );
}

#[test]
fn am_noise_does_not_open_squelch() {
    let mut p = Pipeline::new(PipelineConfig {
        mode: Mode::Am,
        squelch_margin_db: 6.0,
        ..PipelineConfig::default()
    });
    use hertz_dsp::testutil::white_noise;
    for f in 0..12 {
        let ev = p.process_buffer(&white_noise(SDR_RATE, FRAME_IQ, 0.02, 900 + f));
        for e in &ev {
            assert!(
                !matches!(e, PipelineEvent::SquelchOpened { .. }),
                "AM squelch opened on noise"
            );
        }
    }
    assert!(!p.is_squelch_open());
}
