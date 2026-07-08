//! Entropy pool + recorder integration (T2 brief items 5, 6).
//!
//! - Pool fills from pipeline-processed audio and drains.
//! - Recorder writes a valid 48 kHz / 16-bit / mono WAV with the gnosis filename
//!   scheme into a configurable directory, with fades applied.

use std::path::PathBuf;

use hertz_dsp::entropy::EntropyPool;
use hertz_dsp::pipeline::{Pipeline, PipelineConfig, SDR_RATE};
use hertz_dsp::recorder;
use hertz_dsp::testutil::fm_signal;
use hertz_dsp::Mode;

#[test]
fn pool_fills_from_pipeline_and_drains() {
    let mut pool = EntropyPool::new();
    let n = SDR_RATE as usize;
    // Process some FM signal and feed the (would-be) demod audio to the pool.
    // Use the demod directly so we have raw audio.
    let iq = fm_signal(SDR_RATE, n, 0.0, 1000.0, 5000.0, 1.0);
    let mut demod = hertz_dsp::demod::NfmDemod::new(SDR_RATE);
    let (audio, _) = demod.process(&iq);
    for _ in 0..3 {
        pool.harvest(&audio);
    }
    assert!(!pool.is_empty(), "pool should have filled");
    let drained = pool.drain(64);
    assert_eq!(drained.len(), 64);
    assert!(pool.len() <= hertz_dsp::entropy::POOL_CAPACITY);
}

#[test]
fn pool_caps_at_4kb() {
    let mut pool = EntropyPool::new();
    let audio: Vec<f32> = (0..8192).map(|i| (i as f32).sin()).collect();
    for _ in 0..200 {
        pool.harvest(&audio);
    }
    assert!(
        pool.len() <= hertz_dsp::entropy::POOL_CAPACITY,
        "pool exceeded capacity: {}",
        pool.len()
    );
}

#[test]
fn pipeline_drains_entropy() {
    // The pipeline owns an internal pool; the daemon drains it via drain_entropy.
    let mut p = Pipeline::new(PipelineConfig {
        mode: Mode::Nfm,
        ..PipelineConfig::default()
    });
    let n = SDR_RATE as usize;
    let iq = fm_signal(SDR_RATE, n, 0.0, 1000.0, 5000.0, 1.0);
    p.process_buffer(&iq);
    p.process_buffer(&iq);
    let bytes = p.drain_entropy(128);
    assert!(!bytes.is_empty(), "pipeline entropy pool drained nothing");
    assert!(bytes.len() <= 128);
}

#[test]
fn recorder_writes_valid_wav() {
    let tmp: PathBuf = std::env::temp_dir().join("hertz-dsp-recorder-test");
    std::fs::remove_dir_all(&tmp).ok();
    let audio: Vec<f32> = (0..48_000)
        .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48_000.0).sin() * 0.5)
        .collect();

    let path = recorder::write_recording(&audio, 156_800_000, Some(16), Some("SAFETY"), 1, &tmp)
        .expect("write_recording failed");

    assert!(path.exists(), "WAV not written at {path:?}");
    let fname = path.file_name().unwrap().to_string_lossy().to_string();
    assert!(
        fname.starts_with("transmission_"),
        "gnosis filename scheme: {fname}"
    );
    assert!(
        fname.contains("Ch16"),
        "filename should include channel: {fname}"
    );
    assert!(
        fname.contains("SAFETY"),
        "filename should include label: {fname}"
    );
    assert!(fname.ends_with(".wav"));

    // Validate spec via hound.
    let reader = hound::WavReader::open(&path).expect("open WAV");
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 48_000);
    assert_eq!(spec.bits_per_sample, 16);
    assert_eq!(spec.channels, 1);
    // Sample count preserved (with fades applied, length unchanged).
    let count = reader.into_samples::<i16>().count();
    assert_eq!(count, 48_000);

    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn recorder_applies_fades() {
    // Fades should zero (approximately) the very first and very last samples.
    let mut audio: Vec<f32> = vec![0.5; 10_000];
    recorder::apply_fades(&mut audio);
    assert!(audio[0].abs() < 0.01, "fade-in should start near zero");
    assert!(audio[5].abs() < 0.5, "fade-in ramp: {}", audio[5]);
    assert!(
        audio.last().unwrap().abs() < 0.01,
        "fade-out should end near zero"
    );
}
