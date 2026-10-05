//! Narrowband-FM demodulator — ported from `NFMDemod` in
//! `plan/reference/gnosis-radio/src/pipeline/state.rs`.
//!
//! Field-tuned constants (channel FIR, decimation, DC-block, LPF, normalize) are
//! carried verbatim. The only change vs gnosis: input is `&[Complex32]` rather than
//! `&[u8]` — the u8→f32 edge conversion now lives at the dongle boundary (see
//! [`crate::channelizer::bytes_to_iq`]), so there is no lossy u8 round-trip between
//! the channelizer and the demod.

use std::f32::consts::PI;

use num_complex::Complex32;

/// Channel-select FIR: cutoff at NFM voice bandwidth so the neighbouring 25 kHz
/// channel is deep in the stopband by the time we demodulate.
pub const CHAN_FIR_TAPS: usize = 81;
pub const CHAN_FIR_CUTOFF_HZ: f32 = 11_000.0;

/// Polyphase decimation factor (240 kS/s → 48 kS/s audio).
pub const DECIMATION: usize = 5;

/// DC-block IIR coefficient. Carried verbatim.
const DC_ALPHA: f32 = 0.001;

/// Post-discriminator audio low-pass cutoff (Hz).
const AUDIO_LPF_CUTOFF_HZ: f32 = 3000.0;

pub struct NfmDemod {
    input_rate_hz: u32,
    prev_sample: Complex32,
    freq_corr_step: f32,
    freq_corr_phase: f32,
    chan_fir: Vec<f32>,
    /// Unconsumed tail — keeps filter + decimation phase continuous across buffers.
    fir_hist: Vec<Complex32>,
    /// DC-block and audio LPF state, carried across buffers so frame edges don't click.
    dc_state: f32,
    lpf_state: f32,
}

impl NfmDemod {
    /// Construct with a given input sample rate. The rate must be exactly `DECIMATION`
    /// times the audio rate (i.e. 240 kHz for 48 kHz audio); the channel FIR cutoff is
    /// normalized to the supplied rate so the same constants apply.
    pub fn new(input_rate_hz: u32) -> Self {
        assert!(
            (input_rate_hz as usize).is_multiple_of(DECIMATION),
            "input rate must be a multiple of DECIMATION ({DECIMATION})"
        );
        let cutoff_norm = CHAN_FIR_CUTOFF_HZ / input_rate_hz as f32;
        Self {
            input_rate_hz,
            prev_sample: Complex32::new(0.0, 0.0),
            freq_corr_step: 0.0,
            freq_corr_phase: 0.0,
            chan_fir: crate::channelizer::design_lowpass_fir(CHAN_FIR_TAPS, cutoff_norm),
            fir_hist: Vec::new(),
            dc_state: 0.0,
            lpf_state: 0.0,
        }
    }

    /// Audio output sample rate (input rate ÷ `DECIMATION`).
    pub fn output_rate_hz(&self) -> u32 {
        self.input_rate_hz / DECIMATION as u32
    }

    /// Set the AFC frequency-offset rotator.
    pub fn set_freq_offset(&mut self, offset_hz: f32) {
        let output_rate = self.output_rate_hz() as f32;
        self.freq_corr_step = if offset_hz.abs() < 0.05 {
            0.0
        } else {
            -2.0 * PI * offset_hz / output_rate
        };
    }

    fn apply_freq_correction(&mut self, samples: &mut [Complex32]) {
        if self.freq_corr_step.abs() < 1e-7 {
            return;
        }
        for sample in samples {
            let rot = Complex32::from_polar(1.0, self.freq_corr_phase);
            *sample *= rot;
            self.freq_corr_phase += self.freq_corr_step;
            if self.freq_corr_phase > PI {
                self.freq_corr_phase -= 2.0 * PI;
            } else if self.freq_corr_phase < -PI {
                self.freq_corr_phase += 2.0 * PI;
            }
        }
    }

    /// Demodulate. Returns `(audio, in_channel_power_db)`.
    ///
    /// `in_channel_power_db` is the power measured *after* the channel FIR and
    /// ÷5 decimation — adjacent transmissions must not move this meter. This is
    /// gnosis's hard-won fix: the old 3-tap smoother had no selectivity, so talking
    /// on ch 71 painted ch 72's waterfall and opened its entropy squelch.
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

        self.apply_freq_correction(&mut decimated);

        // Quadrature discriminator: arg(x · conj(prev)) / 2π, magnitude-normalized.
        let mut audio = Vec::with_capacity(decimated.len());
        let mut prev = self.prev_sample;
        for &sample in &decimated {
            let normalized = sample / (sample.norm() + 1e-10);
            let product = normalized * prev.conj();
            audio.push(product.arg() / (2.0 * PI));
            prev = normalized;
        }
        self.prev_sample = prev;

        // DC-block (one-pole high-pass) then voice LPF, both stateful across buffers.
        // Level is left raw: the pipeline's VoiceOut AGC sets loudness smoothly
        // (per-buffer peak normalization pumped and clicked every 200 ms).
        let rc = 1.0 / (2.0 * PI * AUDIO_LPF_CUTOFF_HZ);
        let dt = 1.0 / self.output_rate_hz() as f32;
        let lpf_alpha = dt / (rc + dt);
        for s in audio.iter_mut() {
            self.dc_state = DC_ALPHA * *s + (1.0 - DC_ALPHA) * self.dc_state;
            let x = *s - self.dc_state;
            self.lpf_state += lpf_alpha * (x - self.lpf_state);
            *s = self.lpf_state;
        }
        (audio, narrow_db)
    }
}

/// One-pole RC low-pass (gnosis's `simple_lowpass`).
pub(crate) fn simple_lowpass(input: &[f32], sample_rate: f32, cutoff: f32) -> Vec<f32> {
    let rc = 1.0 / (2.0 * PI * cutoff);
    let dt = 1.0 / sample_rate;
    let alpha = dt / (rc + dt);
    let mut state = 0.0;
    input
        .iter()
        .map(|&sample| {
            state = alpha * sample + (1.0 - alpha) * state;
            state
        })
        .collect()
}
