//! Support DSP: FFT-based signal detection, windowing, parabolic peak
//! interpolation, noise-floor tracking, spectral-flatness squelch, impulse/click
//! suppression, audio notch, and phase-slope frequency estimation.
//!
//! Direct port of gnosis-radio `dsp.rs`, cleaned up: NaN-safe
//! comparisons (via `total_cmp`), no redundant `Complex32`→`Complex<f32>` conversion
//! (`num_complex::Complex32` *is* `rustfft`'s `Complex<f32>`), and the FFT planner is
//! cached on the detector.

use std::f32::consts::PI;

use num_complex::Complex32;
use rustfft::FftPlanner;

/// FFT size for signal detection (power of 2). Carried verbatim from gnosis.
pub const FFT_SIZE: usize = 2048;

/// Hann window coefficients (precomputed).
pub struct HannWindow {
    coeffs: Vec<f32>,
}

impl HannWindow {
    pub fn new(size: usize) -> Self {
        let coeffs = (0..size)
            .map(|i| 0.5 * (1.0 - ((2.0 * PI * i as f32) / size.max(1) as f32).cos()))
            .collect();
        Self { coeffs }
    }

    /// Apply the Hann window to complex samples in place.
    pub fn apply(&self, samples: &[Complex32]) -> Vec<Complex32> {
        samples
            .iter()
            .zip(&self.coeffs)
            .map(|(sample, &w)| sample * w)
            .collect()
    }
}

/// Blackman window (better sidelobe suppression than Hann).
pub struct BlackmanWindow {
    coeffs: Vec<f32>,
}

impl BlackmanWindow {
    pub fn new(size: usize) -> Self {
        let a0 = 0.42;
        let a1 = 0.5;
        let a2 = 0.08;
        let coeffs = (0..size)
            .map(|i| {
                let x = (2.0 * PI * i as f32) / size.max(1) as f32;
                a0 - a1 * x.cos() + a2 * (2.0 * x).cos()
            })
            .collect();
        Self { coeffs }
    }

    /// Apply the Blackman window to complex samples.
    pub fn apply(&self, samples: &[Complex32]) -> Vec<Complex32> {
        samples
            .iter()
            .zip(&self.coeffs)
            .map(|(sample, &w)| sample * w)
            .collect()
    }
}

/// FFT-based signal detector with Hann windowing and cached FFT planner.
pub struct SignalDetector {
    window: HannWindow,
    fft_size: usize,
    sample_rate: f32,
    planner: FftPlanner<f32>,
}

impl SignalDetector {
    pub fn new(sample_rate: f32) -> Self {
        Self::with_fft_size(sample_rate, FFT_SIZE)
    }

    /// Construct with a custom FFT size.
    pub fn with_fft_size(sample_rate: f32, fft_size: usize) -> Self {
        Self {
            window: HannWindow::new(fft_size),
            fft_size,
            sample_rate,
            planner: FftPlanner::new(),
        }
    }

    /// Compute the power spectrum in dB.
    /// Returns `(frequencies, magnitudes_db)` where frequencies span `-Fs/2..Fs/2`.
    pub fn power_spectrum(&mut self, samples: &[Complex32]) -> (Vec<f32>, Vec<f32>) {
        let windowed = self.window.apply(samples);

        let magnitudes = self.compute_fft_magnitudes(&windowed);

        let magnitudes_db: Vec<f32> = magnitudes
            .iter()
            .map(|&mag| 10.0 * (mag + 1e-12).log10())
            .collect();

        let bin_width = self.sample_rate / self.fft_size as f32;
        let frequencies: Vec<f32> = (0..self.fft_size)
            .map(|i| {
                if i <= self.fft_size / 2 {
                    i as f32 * bin_width
                } else {
                    (i as f32 - self.fft_size as f32) * bin_width
                }
            })
            .collect();

        (frequencies, magnitudes_db)
    }

    /// Find the peak frequency with parabolic (quadratic) interpolation.
    /// Returns `(peak_frequency_hz, peak_magnitude_db, peak_bin_index)`.
    pub fn find_peak(&self, frequencies: &[f32], magnitudes: &[f32]) -> (f32, f32, usize) {
        let (peak_idx, &peak_mag) = magnitudes
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap_or((0, &0.0));

        let fine_freq = if peak_idx > 0 && peak_idx < magnitudes.len().saturating_sub(1) {
            let y1 = magnitudes[peak_idx - 1];
            let y2 = magnitudes[peak_idx];
            let y3 = magnitudes[peak_idx + 1];
            let denom = y1 - 2.0 * y2 + y3;
            if denom.abs() > 0.1 {
                let delta = (0.5 * (y1 - y3) / denom).clamp(-0.5, 0.5);
                let bin_width = self.sample_rate / self.fft_size as f32;
                frequencies[peak_idx] + delta * bin_width
            } else {
                frequencies[peak_idx]
            }
        } else {
            frequencies[peak_idx]
        };

        (fine_freq, peak_mag, peak_idx)
    }

    /// Estimate the noise floor as the median of the spectrum magnitudes.
    pub fn estimate_noise_floor(&self, magnitudes: &[f32]) -> f32 {
        if magnitudes.is_empty() {
            return -100.0;
        }
        let mut sorted = magnitudes.to_vec();
        sorted.sort_by(|a, b| a.total_cmp(b));
        sorted[sorted.len() / 2]
    }

    fn compute_fft_magnitudes(&mut self, samples: &[Complex32]) -> Vec<f32> {
        let mut buffer: Vec<Complex32> = samples.to_vec();
        buffer.resize(self.fft_size, Complex32::new(0.0, 0.0));
        let fft = self.planner.plan_fft_forward(self.fft_size);
        fft.process(&mut buffer);
        buffer.iter().map(|c| c.norm()).collect()
    }
}

/// Running median noise-floor tracker.
pub struct NoiseFloorTracker {
    history: Vec<f32>,
    capacity: usize,
}

impl NoiseFloorTracker {
    pub fn new(capacity: usize) -> Self {
        Self {
            history: Vec::with_capacity(capacity),
            capacity,
        }
    }

    /// Update with a new noise-floor estimate.
    pub fn update(&mut self, floor_estimate: f32) {
        self.history.push(floor_estimate);
        if self.history.len() > self.capacity {
            self.history.remove(0);
        }
    }

    /// Current noise floor (median of recent estimates).
    pub fn current_floor(&self) -> f32 {
        if self.history.is_empty() {
            return -100.0;
        }
        let mut sorted = self.history.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        sorted[sorted.len() / 2]
    }
}

/// Spectral squelch: distinguishes voice from static using spectral flatness.
pub struct SpectralSquelch {
    flatness_threshold: f32,
}

impl Default for SpectralSquelch {
    fn default() -> Self {
        Self::new()
    }
}

impl SpectralSquelch {
    /// Flatness threshold carried from gnosis: open squelch if flatness < 0.55.
    pub const DEFAULT_FLATNESS_THRESHOLD: f32 = 0.55;

    pub fn new() -> Self {
        Self {
            flatness_threshold: Self::DEFAULT_FLATNESS_THRESHOLD,
        }
    }

    /// Set the flatness threshold (lower = more sensitive, higher = more selective).
    pub fn set_threshold(&mut self, threshold: f32) {
        self.flatness_threshold = threshold.clamp(0.0, 1.0);
    }

    /// Spectral flatness measure (SFM): 0.0 for a pure tone, 1.0 for white noise.
    pub fn spectral_flatness(&self, magnitudes: &[f32]) -> f32 {
        if magnitudes.is_empty() {
            return 0.0;
        }
        let linear: Vec<f32> = magnitudes.iter().map(|&db| 10f32.powf(db / 10.0)).collect();
        let log_sum: f32 = linear.iter().map(|&x| (x + 1e-12).ln()).sum();
        let geometric_mean = (log_sum / linear.len() as f32).exp();
        let arithmetic_mean: f32 = linear.iter().sum::<f32>() / linear.len() as f32;
        (geometric_mean / (arithmetic_mean + 1e-12)).clamp(0.0, 1.0)
    }

    /// True if squelch should open (voice-like, low flatness).
    pub fn should_open(&self, magnitudes: &[f32]) -> bool {
        self.spectral_flatness(magnitudes) < self.flatness_threshold
    }

    /// Last computed flatness (for display/metering).
    pub fn get_flatness(&self, magnitudes: &[f32]) -> f32 {
        self.spectral_flatness(magnitudes)
    }
}

/// Impulse-noise suppressor (removes spectral clicks/pops).
pub struct ImpulseNoiseFilter {
    threshold_db: f32,
}

impl ImpulseNoiseFilter {
    pub fn new(threshold_db: f32) -> Self {
        Self { threshold_db }
    }

    /// Suppress spectral bins this many dB above the median.
    pub fn suppress(&self, magnitudes: &[f32]) -> Vec<f32> {
        if magnitudes.is_empty() {
            return Vec::new();
        }
        let mut sorted = magnitudes.to_vec();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let median = sorted[sorted.len() / 2];
        magnitudes
            .iter()
            .map(|&mag| {
                if mag > median + self.threshold_db {
                    median
                } else {
                    mag
                }
            })
            .collect()
    }
}

/// Audio notch filter — removes specific frequency ranges in the frequency domain.
pub struct AudioNotchFilter {
    sample_rate: f32,
    notch_freqs: Vec<(f32, f32)>,
}

impl AudioNotchFilter {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate,
            notch_freqs: Vec::new(),
        }
    }

    /// Add a notch at `center_hz` with `bandwidth_hz`.
    pub fn add_notch(&mut self, center_hz: f32, bandwidth_hz: f32) {
        self.notch_freqs.push((center_hz, bandwidth_hz));
    }

    #[allow(dead_code)]
    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    /// Attenuate spectrum bins inside any notch by 40 dB.
    pub fn apply_to_spectrum(&self, freqs: &[f32], mags: &mut [f32]) {
        for (i, &freq) in freqs.iter().enumerate() {
            for &(center, bw) in &self.notch_freqs {
                if (freq.abs() - center).abs() < bw / 2.0 {
                    mags[i] -= 40.0;
                }
            }
        }
    }
}

/// Time-domain click suppressor — removes impulse noise from audio samples.
pub struct ClickSuppressor {
    threshold: f32,
    window_size: usize,
}

impl ClickSuppressor {
    /// `threshold` is the amplitude excess over the median (0.0–1.0) that counts as a click.
    pub fn new(threshold: f32) -> Self {
        Self {
            threshold,
            window_size: 5,
        }
    }

    /// Remove clicks from audio samples in place by interpolating over outliers.
    pub fn process(&self, audio: &mut [f32]) {
        if audio.len() < self.window_size * 2 {
            return;
        }
        let mut sorted = audio.to_vec();
        sorted.sort_by(|a, b| a.abs().total_cmp(&b.abs()));
        let median = sorted[sorted.len() / 2].abs();

        for i in self.window_size..audio.len() - self.window_size {
            let sample = audio[i].abs();
            if sample > median + self.threshold {
                let lo = i.saturating_sub(self.window_size);
                let hi = (i + self.window_size).min(audio.len());
                let neighbors: &[f32] = &audio[lo..i];
                let mut sum: f32 = neighbors.iter().sum();
                let mut count = neighbors.len() as f32;
                let after: &[f32] = &audio[i + 1..hi];
                sum += after.iter().sum::<f32>();
                count += after.len() as f32;
                if count > 0.0 {
                    audio[i] = sum / count;
                }
            }
        }
    }
}

/// Frequency-offset estimator using phase slope (for AFC).
pub struct FrequencyEstimator {
    prev_phase: f32,
    sample_rate: f32,
}

impl FrequencyEstimator {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            prev_phase: f32::NAN,
            sample_rate,
        }
    }

    /// Estimate the average frequency offset (Hz) from complex samples.
    pub fn estimate(&mut self, samples: &[Complex32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let mut freq_sum = 0.0;
        let mut count = 0;
        for sample in samples {
            let phase = sample.arg();
            if !self.prev_phase.is_finite() {
                self.prev_phase = phase;
                continue;
            }
            let mut phase_diff = phase - self.prev_phase;
            while phase_diff > PI {
                phase_diff -= 2.0 * PI;
            }
            while phase_diff < -PI {
                phase_diff += 2.0 * PI;
            }
            freq_sum += phase_diff;
            count += 1;
            self.prev_phase = phase;
        }
        if count > 0 {
            (freq_sum / count as f32) * self.sample_rate / (2.0 * PI)
        } else {
            0.0
        }
    }

    pub fn reset(&mut self) {
        self.prev_phase = f32::NAN;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hann_window_shape() {
        // Periodic ("DFT-even") Hann: 0.5*(1 - cos(2πi/N)) with /N, matching gnosis
        // dsp.rs. With N=16 the last coefficient is NOT zero (only /N-1 gives that);
        // coeffs[15] ≈ 0.5*(1 - cos(15π/8)) ≈ 0.038.
        let w = HannWindow::new(16);
        assert_eq!(w.coeffs.len(), 16);
        assert!(
            w.coeffs[0] < 1e-6,
            "coeffs[0] should be ~0, got {}",
            w.coeffs[0]
        );
        assert!(
            (w.coeffs[8] - 1.0).abs() < 0.01,
            "peak coeffs[8] should be ~1.0, got {}",
            w.coeffs[8]
        );
        assert!(
            (w.coeffs[15] - 0.0381).abs() < 1e-3,
            "periodic Hann coeffs[15] should be ~0.038, got {}",
            w.coeffs[15]
        );
        // Symmetry: coeffs[i] == coeffs[N-i] for the periodic Hann.
        for i in 1..8 {
            assert!((w.coeffs[i] - w.coeffs[16 - i]).abs() < 1e-5);
        }
    }

    #[test]
    fn blackman_window_shape() {
        let w = BlackmanWindow::new(16);
        assert_eq!(w.coeffs.len(), 16);
        assert!(w.coeffs[0] < 0.1);
        assert!(w.coeffs[15] < 0.1);
    }

    #[test]
    fn noise_floor_tracker_median() {
        let mut t = NoiseFloorTracker::new(10);
        t.update(-50.0);
        t.update(-52.0);
        t.update(-48.0);
        assert!((t.current_floor() + 50.0).abs() < 2.0);
    }

    #[test]
    fn flatness_tone_vs_noise() {
        let sq = SpectralSquelch::new();
        // Pure tone: one bin dominates → flatness near 0.
        let tone: Vec<f32> = (0..64)
            .map(|i| if i == 10 { 40.0 } else { -60.0 })
            .collect();
        assert!(sq.spectral_flatness(&tone) < 0.1);
        // White noise: roughly equal bins → flatness near 1.
        let noise: Vec<f32> = (0..64).map(|_| -40.0).collect();
        assert!(sq.spectral_flatness(&noise) > 0.9);
    }

    #[test]
    fn frequency_estimator_pure_tone() {
        let mut fe = FrequencyEstimator::new(240_000.0);
        let n = 4096;
        let f = 5000.0_f32;
        let s: Vec<Complex32> = (0..n)
            .map(|i| {
                let t = i as f32 / 240_000.0;
                Complex32::from_polar(1.0, 2.0 * PI * f * t)
            })
            .collect();
        let est = fe.estimate(&s);
        assert!((est - f).abs() < 100.0, "estimated {est}, expected {f}");
    }
}
