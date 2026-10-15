//! Windowed-sinc resampling to the 16 kHz the model expects.

/// Resample mono `input` from `from_hz` to `to_hz` with a Blackman-windowed
/// sinc low-pass (cutoff at 0.45 of the lower Nyquist). Exact for integer
/// ratios such as 48 kHz -> 16 kHz, good enough for speech at any ratio.
pub fn resample(input: &[f32], from_hz: u32, to_hz: u32) -> Vec<f32> {
    if from_hz == to_hz || input.is_empty() {
        return input.to_vec();
    }
    const HALF_TAPS: isize = 32;
    let step = from_hz as f64 / to_hz as f64;
    // Cutoff relative to the input rate.
    let cutoff = 0.9 * 0.5 * (to_hz.min(from_hz) as f64) / from_hz as f64;
    let out_len = ((input.len() as f64) / step).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    let width = HALF_TAPS as f64 * step.max(1.0);
    let n = input.len() as isize;
    for i in 0..out_len {
        let t = i as f64 * step;
        let center = t.floor() as isize;
        let span = width.ceil() as isize;
        let (mut acc, mut norm) = (0.0f64, 0.0f64);
        for k in (center - span).max(0)..=(center + span).min(n - 1) {
            let d = t - k as f64;
            if d.abs() > width {
                continue;
            }
            let x = 2.0 * cutoff * d;
            let sinc = if x == 0.0 {
                1.0
            } else {
                (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
            };
            let w = d / width; // -1..1
            let blackman = 0.42
                + 0.5 * (std::f64::consts::PI * w).cos()
                + 0.08 * (2.0 * std::f64::consts::PI * w).cos();
            let h = sinc * blackman;
            acc += input[k as usize] as f64 * h;
            norm += h;
        }
        out.push(if norm != 0.0 {
            (acc / norm) as f32
        } else {
            0.0
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_and_dc() {
        let x = vec![0.5f32; 48_000];
        let y = resample(&x, 48_000, 16_000);
        assert_eq!(y.len(), 16_000);
        assert!(y[100..15_900].iter().all(|v| (v - 0.5).abs() < 1e-3));
    }

    #[test]
    fn tone_survives() {
        let x: Vec<f32> = (0..48_000)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48_000.0).sin())
            .collect();
        let y = resample(&x, 48_000, 16_000);
        let rms = (y[200..15_800].iter().map(|v| v * v).sum::<f32>() / 15_600.0).sqrt();
        assert!(
            (rms - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.02,
            "rms {rms}"
        );
    }
}
