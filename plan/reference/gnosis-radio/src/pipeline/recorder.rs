use std::fs;
use std::path::PathBuf;

use chrono::Utc;

const RECORDINGS_DIR: &str = "recordings";
const FADE_SAMPLES: usize = 480; // 10ms fade at 48kHz

fn ensure_dirs() -> Result<(), String> {
    fs::create_dir_all(RECORDINGS_DIR)
        .map_err(|e| format!("Failed to create recordings directory: {}", e))
}

fn build_filename(
    freq_hz: u32,
    channel: Option<u8>,
    channel_label: Option<&str>,
    transmission_index: u32,
) -> PathBuf {
    let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
    let freq_mhz = freq_hz as f64 / 1e6;

    match channel {
        Some(ch) => {
            let label = channel_label.unwrap_or("UNKNOWN");
            PathBuf::from(format!(
                "{}/transmission_{}_Ch{}_{}_{:.3}MHz_{}.wav",
                RECORDINGS_DIR, timestamp, ch, label, freq_mhz, transmission_index,
            ))
        }
        None => PathBuf::from(format!(
            "{}/transmission_{}_{:.3}MHz_{}.wav",
            RECORDINGS_DIR, timestamp, freq_mhz, transmission_index,
        )),
    }
}

/// Apply fade-in and fade-out to audio buffer to eliminate click artifacts.
fn apply_fades(audio: &mut [f32]) {
    let len = audio.len();
    if len == 0 {
        return;
    }

    // Fade-in
    let fade_in_len = FADE_SAMPLES.min(len / 2);
    for i in 0..fade_in_len {
        let gain = i as f32 / fade_in_len as f32;
        audio[i] *= gain;
    }

    // Fade-out
    let fade_out_len = FADE_SAMPLES.min(len / 2);
    let fade_out_start = len - fade_out_len;
    for i in 0..fade_out_len {
        let gain = 1.0 - (i as f32 / fade_out_len as f32);
        audio[fade_out_start + i] *= gain;
    }
}

pub fn write_recording(
    audio: &[f32],
    freq_hz: u32,
    channel: Option<u8>,
    channel_label: Option<&str>,
    transmission_index: u32,
) -> Result<PathBuf, String> {
    ensure_dirs()?;

    // Apply fades to a copy
    let mut faded = audio.to_vec();
    apply_fades(&mut faded);

    let path = build_filename(freq_hz, channel, channel_label, transmission_index);
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer = hound::WavWriter::create(&path, spec)
        .map_err(|e| format!("Failed to create WAV: {}", e))?;

    for &sample in &faded {
        let sample_i16 = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
        writer
            .write_sample(sample_i16)
            .map_err(|e| format!("Failed to write sample: {}", e))?;
    }

    writer
        .finalize()
        .map_err(|e| format!("Failed to finalize WAV: {}", e))?;

    Ok(path)
}
