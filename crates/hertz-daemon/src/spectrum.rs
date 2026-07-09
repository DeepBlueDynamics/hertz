//! FFT spectrum frames for the live waterfall (T4.1).
//!
//! Each dongle's DSP thread computes a decimated FFT magnitude snapshot of what
//! it's receiving and publishes a [`SpectrumFrame`] on the bus's spectrum
//! channel. The WS `/stream` handler forwards them to clients that opted in via
//! `?spectrum=` as a binary frame (see `hertz_types::wire::encode_spectrum_frame`).
//!
//! Rate-limiting lives in [`SpectrumEmitter`]: no dongle emits more than
//! [`SpectrumEmitter::MAX_FPS`] frames per second regardless of the DSP frame
//! rate, keeping the wire cost bounded (~40-80 KB/s at 1024 bins × 20 fps).

use std::time::{Duration, Instant};

use hertz_types::wire::encode_spectrum_frame;
use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

/// The FFT size used for the published spectrum (bin count per frame).
pub const SPECTRUM_FFT_SIZE: usize = 1024;

/// One spectrum snapshot from a dongle, carried on the bus's spectrum channel
/// and encoded for WS via [`SpectrumFrame::encode`].
#[derive(Clone, Debug)]
pub struct SpectrumFrame {
    pub dongle_id: String,
    pub dongle_idx: u8,
    /// Center of the span the bins cover (the dongle's tuned / wideband center).
    pub center_hz: f64,
    /// Total bandwidth the bins cover (== the SDR sample rate at this tap).
    pub span_hz: f64,
    /// Carrier present on the tuned channel this slice (best-effort aggregate
    /// for the channelized role).
    pub squelch_open: bool,
    /// Magnitude per FFT bin in dB, fft-shifted so bin 0 = center - span/2 and
    /// the last bin = center + span/2 (matches `plan/tui-waterfall-spec.md` §3).
    pub bins_db: Vec<f32>,
}

impl SpectrumFrame {
    /// Encode to the wire binary layout (`[0x02][dongle][f64 center][f64 span]
    /// [u8 squelch][u16 n][f32 bins...]`).
    pub fn encode(&self) -> Vec<u8> {
        encode_spectrum_frame(
            self.dongle_idx,
            self.center_hz,
            self.span_hz,
            self.squelch_open,
            &self.bins_db,
        )
    }
}

/// Per-dongle rate limiter + FFT runner. Call [`SpectrumEmitter::maybe_emit`] on
/// each DSP frame; it returns `Some(bins)` only when enough time has elapsed
/// since the last emit, enforcing the ≤20 fps cap.
pub struct SpectrumEmitter {
    fft_size: usize,
    min_interval: Duration,
    last_emit: Option<Instant>,
    // Cached FFT plan: planning is the expensive part, so reuse it across frames.
    fft: std::sync::Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,
}

impl SpectrumEmitter {
    /// Maximum publish rate per dongle (T4.1: ≤20 fps regardless of DSP rate).
    pub const MAX_FPS: u32 = 20;

    /// New emitter with the default 1024-pt FFT and a 20 fps cap.
    pub fn new() -> Self {
        Self::with_size(SPECTRUM_FFT_SIZE)
    }

    /// New emitter for an explicit FFT size (must match a size `rustfft` can plan;
    /// powers of two are the intended use).
    pub fn with_size(fft_size: usize) -> Self {
        let fft_size = fft_size.max(1);
        let planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(fft_size);
        Self {
            fft_size,
            min_interval: Duration::from_secs_f64(1.0 / Self::MAX_FPS as f64),
            last_emit: None,
            fft,
            scratch: Vec::with_capacity(fft_size),
        }
    }

    /// Returns the configured FFT size (== bins per frame).
    pub fn fft_size(&self) -> usize {
        self.fft_size
    }

    /// If the rate-limit window has elapsed, compute a magnitude spectrum (dB,
    /// fft-shifted) over the first `fft_size` samples of `iq` and return it;
    /// otherwise return `None`. Fewer than `fft_size` samples yields a smaller
    /// spectrum over whatever is available (still shifted), never a panic.
    pub fn maybe_emit(&mut self, iq: &[Complex32]) -> Option<Vec<f32>> {
        let now = Instant::now();
        if let Some(last) = self.last_emit {
            if now.duration_since(last) < self.min_interval {
                return None;
            }
        }
        self.last_emit = Some(now);
        Some(compute_spectrum_db(iq, self.fft_size, &self.fft, &mut self.scratch))
    }
}

impl Default for SpectrumEmitter {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute an fft-shifted magnitude spectrum in dB from `iq`. Takes the first
/// `fft_size` samples (or fewer if the buffer is short). `10·log10(|bin|² + ε)`
/// matches the convention used by `hertz_dsp::channelizer::detect_active_channels`.
pub fn compute_spectrum_db(
    iq: &[Complex32],
    fft_size: usize,
    fft: &std::sync::Arc<dyn Fft<f32>>,
    scratch: &mut Vec<Complex32>,
) -> Vec<f32> {
    let n = fft_size.min(iq.len());
    if n == 0 {
        return Vec::new();
    }
    let mut buf: Vec<Complex32> = iq[..n].to_vec();
    // If the IQ window is smaller than the planned FFT we fall back to a
    // same-size plan; otherwise reuse the cached one.
    if n == fft_size {
        fft.process_with_scratch(&mut buf, scratch);
    } else {
        let planner = FftPlanner::new();
        let ad_hoc = planner.plan_fft_forward(n);
        ad_hoc.process(&mut buf);
    }

    // dB magnitude per bin, then fftshift so bin 0 = lowest frequency.
    let mut db: Vec<f32> = buf.iter().map(|c| 10.0 * (c.norm_sqr() + 1e-12).log10()).collect();
    db.rotate(n / 2);
    db
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emitter_rate_limits_to_max_fps() {
        let mut em = SpectrumEmitter::new();
        // First call always emits.
        assert!(em.maybe_emit(&[]).is_some(), "first call should emit");
        // Immediately after: suppressed.
        assert!(em.maybe_emit(&[]).is_none(), "second call within window");
    }

    #[test]
    fn spectrum_of_pure_tone_peaks_away_from_center() {
        // A real sinusoid at +bin offset should produce a clear peak. Use a
        // large window so the tone lands in a resolvable bin.
        let n = 1024usize;
        let rate = 1_000_000_f64;
        let tone_hz = 100_000.0; // +10% of Nyquist → bin ~ round(100_000/ (rate/n))
        let k = (tone_hz / (rate / n as f64)).round() as usize;
        let iq: Vec<Complex32> = (0..n)
            .map(|i| {
                let t = i as f32 / n as f32;
                let phase = 2.0 * std::f32::consts::PI * (tone_hz as f32) * t;
                Complex32::new(phase.cos(), phase.sin())
            })
            .collect();
        let planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(n);
        let mut scratch = Vec::new();
        let db = compute_spectrum_db(&iq, n, &fft, &mut scratch);
        assert_eq!(db.len(), n);
        // The peak bin after fftshift corresponds to +tone_hz.
        let peak = db
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.total_cmp(b))
            .map(|(i, _)| i)
            .unwrap();
        let lo = k.saturating_sub(3);
        let hi = (k + 3).min(n - 1);
        assert!(
            (lo..=hi).contains(&peak),
            "peak at bin {peak}, expected near {k} (±3) after fftshift"
        );
        let _ = rate;
    }

    #[test]
    fn frame_encode_decode_round_trips() {
        use hertz_types::wire::decode_spectrum_frame;
        let bins = vec![-70.0, -55.5, -33.0, -10.0];
        let frame = SpectrumFrame {
            dongle_id: "D0".into(),
            dongle_idx: 0,
            center_hz: 156_800_000.0,
            span_hz: 240_000.0,
            squelch_open: true,
            bins_db: bins.clone(),
        };
        let bytes = frame.encode();
        let decoded = decode_spectrum_frame(&bytes).expect("decode");
        assert_eq!(decoded.dongle_idx, 0);
        assert!((decoded.center_hz - 156_800_000.0).abs() < 1e-9);
        assert!((decoded.span_hz - 240_000.0).abs() < 1e-9);
        assert!(decoded.squelch_open);
        assert_eq!(decoded.bins_db.len(), bins.len());
        for (a, b) in decoded.bins_db.iter().zip(&bins) {
            assert!((a - b).abs() < 1e-6);
        }
    }
}
