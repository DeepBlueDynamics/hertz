//! WAV recorder — ported from gnosis-radio `pipeline/recorder.rs`.
//!
//! Writes 48 kHz / 16-bit / mono WAVs with 10 ms fades, using the gnosis filename
//! scheme but with a configurable output directory (gnosis hardcoded `recordings/`).
//!
//! This is the one piece of file I/O in the crate (explicitly excepted by the T2
//! brief). The [`crate::Pipeline`] never calls it directly — it emits
//! [`crate::PipelineEvent::TransmissionComplete`] and the daemon wires that to
//! [`write_recording`] (or to its own recorder that adds transcription/DB plumbing).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use hound::{WavSpec, WavWriter};

use crate::pipeline::AUDIO_SAMPLE_RATE;

/// Format a UTC timestamp as `YYYYMMDD_HHMMSS` using only `std::time`.
///
/// Pure `std` is used (rather than chrono) to keep this crate's dependency set to
/// the T2-brief-allowed num-complex / rustfft / hound / serde.
fn utc_timestamp_string() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let sod = secs % 86_400;
    let hour = sod / 3600;
    let min = (sod % 3600) / 60;
    let sec = sod % 60;

    // Civil-from-days (Howard Hinnant's algorithm). Epoch 1970-01-01.
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}",
        year, m, d, hour, min, sec
    )
}

/// Fade length (samples). Carried from gnosis: 10 ms at 48 kHz.
pub const FADE_SAMPLES: usize = 480;

/// Build the gnosis filename for a transmission, rooted at `dir`.
pub fn build_filename(
    dir: &Path,
    freq_hz: u32,
    channel: Option<u32>,
    channel_label: Option<&str>,
    transmission_index: u32,
) -> PathBuf {
    let timestamp = utc_timestamp_string();
    let freq_mhz = freq_hz as f64 / 1e6;
    match channel {
        Some(ch) => {
            let label = channel_label.unwrap_or("UNKNOWN");
            PathBuf::from(format!(
                "{}/transmission_{}_Ch{}_{}_{:.3}MHz_{}.wav",
                dir.display(),
                timestamp,
                ch,
                label,
                freq_mhz,
                transmission_index,
            ))
        }
        None => PathBuf::from(format!(
            "{}/transmission_{}_{:.3}MHz_{}.wav",
            dir.display(),
            timestamp,
            freq_mhz,
            transmission_index,
        )),
    }
}

/// Apply 10 ms linear fade-in and fade-out to eliminate click artifacts.
pub fn apply_fades(audio: &mut [f32]) {
    let len = audio.len();
    if len == 0 {
        return;
    }
    let fade_in_len = FADE_SAMPLES.min(len / 2);
    for (i, s) in audio[..fade_in_len].iter_mut().enumerate() {
        let gain = i as f32 / fade_in_len as f32;
        *s *= gain;
    }
    let fade_out_len = FADE_SAMPLES.min(len / 2);
    let fade_out_start = len - fade_out_len;
    for (i, s) in audio[fade_out_start..].iter_mut().enumerate() {
        let gain = 1.0 - (i as f32 / fade_out_len as f32);
        *s *= gain;
    }
}

/// Write a transmission WAV to `dir`, applying fades first. Returns the path written.
pub fn write_recording(
    audio: &[f32],
    freq_hz: u32,
    channel: Option<u32>,
    channel_label: Option<&str>,
    transmission_index: u32,
    dir: &Path,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("Failed to create {:?}: {}", dir, e))?;

    let mut faded = audio.to_vec();
    apply_fades(&mut faded);

    let path = build_filename(dir, freq_hz, channel, channel_label, transmission_index);
    let spec = WavSpec {
        channels: 1,
        sample_rate: AUDIO_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer =
        WavWriter::create(&path, spec).map_err(|e| format!("Failed to create WAV: {}", e))?;
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
