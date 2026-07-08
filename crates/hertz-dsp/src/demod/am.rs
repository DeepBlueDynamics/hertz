//! Amplitude-modulation demodulator — NEW in Hertz (PLAN §4), needed for CB and
//! airband. Envelope detector with DC-block and a simple AGC, sharing the NFM
//! channel-FIR front end so selectivity is identical to the NFM path.
//!
//! Per the T2 brief: envelope `|x|`, DC-block, simple AGC, same channel-FIR front
//! end. Same `(audio, in_channel_power_db)` interface as [`super::NfmDemod`].

use num_complex::Complex32;

use super::nfm::{simple_lowpass, CHAN_FIR_CUTOFF_HZ, CHAN_FIR_TAPS, DECIMATION};
use super::Mode;

/// DC-block coefficient for AM (matches the NFM path).
const DC_ALPHA: f32 = 0.001;
/// Post-envelope audio low-pass cutoff (Hz).
const AUDIO_LPF_CUTOFF_HZ: f32 = 3000.0;
/// Peak-normalization target (matches NFM).
const PEAK_NORM_TARGET: f32 = 0.7;

/// AGC attack/release time constant (per sample). Slow enough to track syllables,
/// fast enough to follow fading carriers.
const AGC_ALPHA: f32 = 0.002;
/// Target AGC envelope level.
const AGC_TARGET: f32 = 0.5;

pub struct AmDemod {
    input_rate_hz: u32,
    #[allow(dead_code)]
    mode: Mode,
    chan_fir: Vec<f32>,
    fir_hist: Vec<Complex32>,
    agc_gain: f32,
}

impl AmDemod {
    /// Construct with a given input sample rate (same constraints as [`NfmDemod`]).
    pub fn new(input_rate_hz: u32) -> Self {
        assert!(
            (input_rate_hz as usize).is_multiple_of(DECIMATION),
            "input rate must be a multiple of DECIMATION ({DECIMATION})"
        );
        let cutoff_norm = CHAN_FIR_CUTOFF_HZ / input_rate_hz as f32;
        Self {
            input_rate_hz,
            mode: Mode::Am,
            chan_fir: crate::channelizer::design_lowpass_fir(CHAN_FIR_TAPS, cutoff_norm),
            fir_hist: Vec::new(),
            agc_gain: 1.0,
        }
    }

    /// Audio output sample rate (input rate ÷ `DECIMATION`).
    pub fn output_rate_hz(&self) -> u32 {
        self.input_rate_hz / DECIMATION as u32
    }

    /// Demodulate. Returns `(audio, in_channel_power_db)` — the in-channel power is
    /// measured after the channel FIR, identical to the NFM path so the entropy
    /// squelch metering behaves the same.
    pub fn process(&mut self, iq: &[Complex32]) -> (Vec<f32>, f32) {
        let taps = self.chan_fir.len();
        let mut buf = std::mem::take(&mut self.fir_hist);
        buf.extend_from_slice(iq);

        let mut decimated: Vec<Complex32> = Vec::with_capacity(buf.len() / DECIMATION + 1);
        let mut pos = 0usize;
        while pos + taps <= buf.len() {
            let mut acc = Complex32::new(0.0, 0.0);
            for (j, &c) in self.chan_fir.iter().enumerate() {
                acc += buf[pos + j] * c;
            }
            decimated.push(acc);
            pos += DECIMATION;
        }
        self.fir_hist = buf.split_off(pos.min(buf.len()));

        let narrow_db = if decimated.is_empty() {
            -120.0
        } else {
            10.0 * (decimated.iter().map(|s| s.norm_sqr()).sum::<f32>() / decimated.len() as f32
                + 1e-12)
                .log10()
        };

        // Envelope detector: |x|.
        let mut envelope: Vec<f32> = decimated.iter().map(|s| s.norm()).collect();

        // DC-block to remove the carrier offset left by the envelope.
        let mut dc_offset = 0.0f32;
        for e in envelope.iter_mut() {
            dc_offset = DC_ALPHA * *e + (1.0 - DC_ALPHA) * dc_offset;
            *e -= dc_offset;
        }

        // Simple AGC: track the envelope with a slow one-pole, scale audio to target.
        let mut audio: Vec<f32> = Vec::with_capacity(envelope.len());
        for &e in &envelope {
            let level = e.abs() + 1e-9;
            self.agc_gain = AGC_ALPHA * (AGC_TARGET / level) + (1.0 - AGC_ALPHA) * self.agc_gain;
            audio.push(e * self.agc_gain);
        }

        let filtered = simple_lowpass(&audio, self.output_rate_hz() as f32, AUDIO_LPF_CUTOFF_HZ);

        let max = filtered.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
        let audio = if max > 0.01 {
            filtered
                .iter()
                .map(|&x| (x / max) * PEAK_NORM_TARGET)
                .collect()
        } else {
            filtered
        };
        (audio, narrow_db)
    }
}
