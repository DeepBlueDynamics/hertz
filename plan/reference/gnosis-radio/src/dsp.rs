/// Advanced DSP Module for VHF Monitor
/// Provides FFT-based signal detection, windowing, peak interpolation,
/// noise floor tracking, and frequency offset estimation
use num_complex::Complex32;
use rustfft::{num_complex::Complex, FftPlanner};
use std::f32::consts::PI;

/// FFT size for signal detection (power of 2)
const FFT_SIZE: usize = 2048;

/// Hann window coefficients (precomputed)
pub struct HannWindow {
    coeffs: Vec<f32>,
}

impl HannWindow {
    pub fn new(size: usize) -> Self {
        let coeffs = (0..size)
            .map(|i| 0.5 * (1.0 - ((2.0 * PI * i as f32) / size as f32).cos()))
            .collect();
        Self { coeffs }
    }

    /// Apply Hann window to complex samples
    pub fn apply(&self, samples: &[Complex32]) -> Vec<Complex32> {
        samples
            .iter()
            .zip(&self.coeffs)
            .map(|(sample, &w)| sample * w)
            .collect()
    }
}

/// Blackman window coefficients (better sidelobe suppression than Hann)
#[allow(dead_code)]
pub struct BlackmanWindow {
    coeffs: Vec<f32>,
}

#[allow(dead_code)]

impl BlackmanWindow {
    pub fn new(size: usize) -> Self {
        let a0 = 0.42;
        let a1 = 0.5;
        let a2 = 0.08;

        let coeffs = (0..size)
            .map(|i| {
                let x = (2.0 * PI * i as f32) / size as f32;
                a0 - a1 * x.cos() + a2 * (2.0 * x).cos()
            })
            .collect();
        Self { coeffs }
    }

    /// Apply Blackman window to complex samples
    pub fn apply(&self, samples: &[Complex32]) -> Vec<Complex32> {
        samples
            .iter()
            .zip(&self.coeffs)
            .map(|(sample, &w)| sample * w)
            .collect()
    }
}

/// FFT-based signal detector with windowing
pub struct SignalDetector {
    window: HannWindow,
    fft_size: usize,
    sample_rate: f32,
    planner: FftPlanner<f32>,
}

impl SignalDetector {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            window: HannWindow::new(FFT_SIZE),
            fft_size: FFT_SIZE,
            sample_rate,
            planner: FftPlanner::new(),
        }
    }

    /// Compute power spectrum in dB
    /// Returns (frequencies, magnitudes_db)
    pub fn power_spectrum(&mut self, samples: &[Complex32]) -> (Vec<f32>, Vec<f32>) {
        // Apply window
        let windowed = self.window.apply(samples);

        // Compute FFT (placeholder - would use rustfft here)
        let magnitudes = self.compute_fft_magnitudes(&windowed);

        // Convert to dB scale
        let magnitudes_db: Vec<f32> = magnitudes
            .iter()
            .map(|&mag| 10.0 * (mag + 1e-12).log10())
            .collect();

        // Generate frequency bins
        // FFT output is [0..Fs/2, -Fs/2..0] (bins N/2+1 to N-1 are negative freqs)
        let bin_width = self.sample_rate / self.fft_size as f32;
        let frequencies: Vec<f32> = (0..self.fft_size)
            .map(|i| {
                if i <= self.fft_size / 2 {
                    // Positive frequencies: 0 to Fs/2
                    i as f32 * bin_width
                } else {
                    // Negative frequencies: -Fs/2 to 0
                    (i as f32 - self.fft_size as f32) * bin_width
                }
            })
            .collect();

        (frequencies, magnitudes_db)
    }

    /// Find peak frequency with quadratic interpolation
    /// Returns (peak_frequency_hz, peak_magnitude_db, peak_bin_index)
    pub fn find_peak(&self, frequencies: &[f32], magnitudes: &[f32]) -> (f32, f32, usize) {
        // Find maximum bin
        let (peak_idx, &peak_mag) = magnitudes
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();

        // Quadratic interpolation for sub-bin accuracy
        let fine_freq = if peak_idx > 0 && peak_idx < magnitudes.len() - 1 {
            let y1 = magnitudes[peak_idx - 1];
            let y2 = magnitudes[peak_idx];
            let y3 = magnitudes[peak_idx + 1];

            let denom = y1 - 2.0 * y2 + y3;

            // Only interpolate if we have a clear peak (denom is large enough)
            if denom.abs() > 0.1 {
                let delta = (0.5 * (y1 - y3) / denom).clamp(-0.5, 0.5);
                let bin_width = self.sample_rate / self.fft_size as f32;
                frequencies[peak_idx] + delta * bin_width
            } else {
                // Flat spectrum (noise), just use bin center
                frequencies[peak_idx]
            }
        } else {
            frequencies[peak_idx]
        };

        (fine_freq, peak_mag, peak_idx)
    }

    /// Estimate noise floor using median of spectrum
    pub fn estimate_noise_floor(&self, magnitudes: &[f32]) -> f32 {
        let mut sorted = magnitudes.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

        // Use median as robust noise floor estimate
        sorted[sorted.len() / 2]
    }

    /// Compute FFT magnitudes using RustFFT
    fn compute_fft_magnitudes(&mut self, samples: &[Complex32]) -> Vec<f32> {
        // Convert Complex32 to Complex<f32> for RustFFT
        let mut buffer: Vec<Complex<f32>> = samples
            .iter()
            .map(|s| Complex { re: s.re, im: s.im })
            .collect();

        // Pad or truncate to FFT size
        buffer.resize(self.fft_size, Complex { re: 0.0, im: 0.0 });

        // Get FFT plan and execute
        let fft = self.planner.plan_fft_forward(self.fft_size);
        fft.process(&mut buffer);

        // Compute magnitudes
        buffer.iter().map(|c| c.norm()).collect()
    }
}

/// Running median noise floor tracker
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

    /// Update with new noise floor estimate
    pub fn update(&mut self, floor_estimate: f32) {
        self.history.push(floor_estimate);
        if self.history.len() > self.capacity {
            self.history.remove(0);
        }
    }

    /// Get current noise floor (median of recent estimates)
    #[allow(dead_code)]
    pub fn current_floor(&self) -> f32 {
        if self.history.is_empty() {
            return -100.0; // Default very low floor
        }

        let mut sorted = self.history.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        sorted[sorted.len() / 2]
    }
}

/// Spectral squelch: distinguish voice from static using entropy
pub struct SpectralSquelch {
    /// Threshold for spectral flatness (0.0 = pure tone, 1.0 = white noise)
    flatness_threshold: f32,
}

impl SpectralSquelch {
    pub fn new() -> Self {
        Self {
            flatness_threshold: 0.55, // Open squelch if flatness < 0.55 (MUCH more selective - only real voice)
        }
    }

    /// Set flatness threshold (lower = more sensitive, higher = more selective)
    #[allow(dead_code)]
    pub fn set_threshold(&mut self, threshold: f32) {
        self.flatness_threshold = threshold.clamp(0.0, 1.0);
    }

    /// Compute spectral flatness measure (SFM)
    /// Returns 0.0 for pure tone, 1.0 for white noise
    pub fn spectral_flatness(&self, magnitudes: &[f32]) -> f32 {
        if magnitudes.is_empty() {
            return 0.0;
        }

        // Convert from dB back to linear power
        let linear: Vec<f32> = magnitudes.iter().map(|&db| 10f32.powf(db / 10.0)).collect();

        // Geometric mean (product^(1/N))
        let log_sum: f32 = linear.iter().map(|&x| (x + 1e-12).ln()).sum();
        let geometric_mean = (log_sum / linear.len() as f32).exp();

        // Arithmetic mean
        let arithmetic_mean: f32 = linear.iter().sum::<f32>() / linear.len() as f32;

        // Flatness = geometric / arithmetic (0 to 1)
        (geometric_mean / (arithmetic_mean + 1e-12)).clamp(0.0, 1.0)
    }

    /// Check if squelch should open (true = voice detected, false = static)
    pub fn should_open(&self, magnitudes: &[f32]) -> bool {
        let flatness = self.spectral_flatness(magnitudes);
        flatness < self.flatness_threshold
    }

    /// Get last computed flatness (for display)
    pub fn get_flatness(&self, magnitudes: &[f32]) -> f32 {
        self.spectral_flatness(magnitudes)
    }
}

/// Impulse noise suppressor (removes clicks/pops)
pub struct ImpulseNoiseFilter {
    threshold_db: f32, // Suppress bins this many dB above median
}

impl ImpulseNoiseFilter {
    pub fn new(threshold_db: f32) -> Self {
        Self { threshold_db }
    }

    /// Suppress spectral bins with abnormal energy (clicks/pops)
    pub fn suppress(&self, magnitudes: &[f32]) -> Vec<f32> {
        if magnitudes.is_empty() {
            return Vec::new();
        }

        // Find median magnitude
        let mut sorted = magnitudes.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = sorted[sorted.len() / 2];

        // Suppress bins above threshold
        magnitudes
            .iter()
            .map(|&mag| {
                if mag > median + self.threshold_db {
                    median // Replace with median
                } else {
                    mag
                }
            })
            .collect()
    }
}

/// Audio notch filter - removes specific frequency ranges
pub struct AudioNotchFilter {
    #[allow(dead_code)]
    sample_rate: f32,
    notch_freqs: Vec<(f32, f32)>, // (center_freq, bandwidth) pairs in Hz
}

impl AudioNotchFilter {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate,
            notch_freqs: Vec::new(),
        }
    }

    /// Apply notch filter in frequency domain
    pub fn apply_to_spectrum(&self, freqs: &[f32], mags: &mut [f32]) {
        for (i, &freq) in freqs.iter().enumerate() {
            for &(center, bw) in &self.notch_freqs {
                let freq_abs = freq.abs();
                if (freq_abs - center).abs() < bw / 2.0 {
                    // Attenuate this bin (reduce by 40 dB)
                    mags[i] -= 40.0;
                }
            }
        }
    }
}

/// Time-domain click suppressor - removes impulse noise from audio
pub struct ClickSuppressor {
    threshold: f32,     // Amplitude threshold (0.0-1.0)
    window_size: usize, // Samples to check around click
}

impl ClickSuppressor {
    pub fn new(threshold: f32) -> Self {
        Self {
            threshold,
            window_size: 5, // Check 5 samples before/after
        }
    }

    /// Remove clicks from audio samples
    pub fn process(&self, audio: &mut [f32]) {
        if audio.len() < self.window_size * 2 {
            return;
        }

        // Find median level for comparison
        let mut sorted = audio.to_vec();
        sorted.sort_by(|a, b| a.abs().partial_cmp(&b.abs()).unwrap());
        let median = sorted[sorted.len() / 2].abs();

        // Scan for impulse spikes
        for i in self.window_size..audio.len() - self.window_size {
            let sample = audio[i].abs();

            // If sample is much larger than median, it's likely a click
            if sample > median + self.threshold {
                // Get average of surrounding samples
                let mut sum = 0.0;
                let mut count = 0;
                for j in (i.saturating_sub(self.window_size))..i {
                    sum += audio[j];
                    count += 1;
                }
                for j in (i + 1)..(i + self.window_size).min(audio.len()) {
                    sum += audio[j];
                    count += 1;
                }

                // Replace click with interpolated value
                if count > 0 {
                    audio[i] = sum / count as f32;
                }
            }
        }
    }
}

/// Frequency offset estimator using phase slope
#[allow(dead_code)]
pub struct FrequencyEstimator {
    prev_phase: f32,
    sample_rate: f32,
}

#[allow(dead_code)]

impl FrequencyEstimator {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            prev_phase: f32::NAN,
            sample_rate,
        }
    }

    /// Estimate frequency offset from complex samples
    pub fn estimate(&mut self, samples: &[Complex32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }

        let mut freq_sum = 0.0;
        let mut count = 0;

        for sample in samples {
            let phase = sample.arg(); // atan2(im, re)
            if !self.prev_phase.is_finite() {
                self.prev_phase = phase;
                continue;
            }
            let mut phase_diff = phase - self.prev_phase;

            // Unwrap phase (handle 2π wrapping)
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

        // Convert average phase difference to frequency
        if count > 0 {
            (freq_sum / count as f32) * self.sample_rate / (2.0 * PI)
        } else {
            0.0
        }
    }

    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.prev_phase = f32::NAN;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hann_window() {
        let window = HannWindow::new(16);
        assert_eq!(window.coeffs.len(), 16);
        // First and last coefficients should be near zero
        assert!(window.coeffs[0] < 0.01);
        assert!(window.coeffs[15] < 0.01);
        // Middle should be near 1.0
        assert!((window.coeffs[8] - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_blackman_window() {
        let window = BlackmanWindow::new(16);
        assert_eq!(window.coeffs.len(), 16);
        // First and last coefficients should be small
        assert!(window.coeffs[0] < 0.1);
        assert!(window.coeffs[15] < 0.1);
    }

    #[test]
    fn test_noise_floor_tracker() {
        let mut tracker = NoiseFloorTracker::new(10);
        tracker.update(-50.0);
        tracker.update(-52.0);
        tracker.update(-48.0);

        let floor = tracker.current_floor();
        assert!((floor + 50.0).abs() < 2.0); // Should be around -50
    }
}
