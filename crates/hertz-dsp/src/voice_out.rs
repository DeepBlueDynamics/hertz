//! Listening chain applied to demod audio *after* squelch/flatness analysis: FM
//! de-emphasis, a second 3 kHz roll-off, and a fixed gain with a soft limiter. Filter state
//! persists across frames so 200 ms frame boundaries don't click; the gain is fixed instead
//! of re-normalizing every frame (which blasted quiet hiss up to voice level).
//!
//! Analysis stays on the undecorated demod output: de-emphasis tilts the noise
//! spectrum, which would shift the flatness thresholds the squelch relies on.

/// Land-mobile / marine NFM de-emphasis time constant (750 µs, 212 Hz corner).
const DEEMPH_TAU_S: f32 = 750e-6;
/// Extra voice low-pass (Hz), cascaded with the demod's one-pole.
const VOICE_LPF_HZ: f32 = 3000.0;
/// Fixed NFM audio gain. FM audio level tracks transmitter deviation, not signal
/// strength, so a fixed gain is correct — an AGC rides up in speech pauses and
/// swells the hiss. ~3 kHz deviation at 1 kHz reads ~0.012 after de-emphasis and
/// the LPFs; x50 puts speech near 0.6.
const NFM_GAIN: f32 = 50.0;

pub struct VoiceOut {
    deemph: bool,
    deemph_alpha: f32,
    deemph_state: f32,
    lpf_alpha: f32,
    lpf_state: f32,
    gain: f32,
}

impl VoiceOut {
    /// `deemph`: apply FM de-emphasis (NFM only; AM has no pre-emphasis).
    pub fn new(sample_rate_hz: f32, deemph: bool) -> Self {
        let dt = 1.0 / sample_rate_hz;
        let rc = 1.0 / (2.0 * std::f32::consts::PI * VOICE_LPF_HZ);
        Self {
            deemph,
            deemph_alpha: dt / (DEEMPH_TAU_S + dt),
            deemph_state: 0.0,
            lpf_alpha: dt / (rc + dt),
            lpf_state: 0.0,
            gain: if deemph { NFM_GAIN } else { 1.0 },
        }
    }

    pub fn reset(&mut self) {
        self.deemph_state = 0.0;
        self.lpf_state = 0.0;
    }

    pub fn process(&mut self, audio: &mut [f32]) {
        if audio.is_empty() {
            return;
        }
        for s in audio.iter_mut() {
            let mut x = *s;
            if self.deemph {
                self.deemph_state += self.deemph_alpha * (x - self.deemph_state);
                x = self.deemph_state;
            }
            self.lpf_state += self.lpf_alpha * (x - self.lpf_state);
            *s = self.lpf_state;
        }

        // Fixed gain + tanh soft limiter (no hard clipping on loud peaks).
        for s in audio.iter_mut() {
            *s = (*s * self.gain).tanh();
        }
    }
}
