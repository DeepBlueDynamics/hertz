//! Signal classification — port of `plan/reference/gnosis-radio/src/pipeline/squelch.rs`.
//!
//! Carried verbatim; the gnosis field-tuned thresholds are sacred.

/// Coarse classification of what the squelch opened on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalClassification {
    Static,
    Carrier,
    Voice,
}

impl SignalClassification {
    pub fn label(&self) -> &'static str {
        match self {
            SignalClassification::Static => "STATIC",
            SignalClassification::Carrier => "CARRIER",
            SignalClassification::Voice => "VOICE",
        }
    }
}

/// gnosis's classifier. The heuristics (peak-vs-floor, power-vs-floor, broadband
/// flatness/harmonic count) are carried unchanged.
#[allow(clippy::too_many_arguments)]
pub fn classify_signal(
    signal_db: f32,
    noise_floor_db: f32,
    peak_mag_db: f32,
    spectral_floor_db: f32,
    voice_detected: bool,
    spectral_flatness: f32,
    harmonic_bins: usize,
) -> SignalClassification {
    let carrier_from_peak = peak_mag_db - spectral_floor_db > 4.0;
    let carrier_from_power = signal_db - noise_floor_db > 3.0;
    let has_carrier = carrier_from_peak || carrier_from_power;
    let broadband = spectral_flatness > 0.78 || harmonic_bins > 12;

    if voice_detected {
        SignalClassification::Voice
    } else if !has_carrier || broadband {
        SignalClassification::Static
    } else {
        SignalClassification::Carrier
    }
}
