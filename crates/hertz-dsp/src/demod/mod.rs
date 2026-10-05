//! Demodulator front ends. Selected by [`Mode`] on [`crate::PipelineConfig`].
//!
//! NFM is ported from gnosis's `NFMDemod`; AM is new for CB + airband.

pub mod am;
pub mod nfm;

use num_complex::Complex32;

pub use am::AmDemod;
pub use nfm::NfmDemod;

/// Modulation the pipeline should demodulate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum Mode {
    /// Narrowband FM (marine, GMRS, MURS, ham, rail). Ported from gnosis.
    Nfm,
    /// Amplitude modulation (CB, airband). New in Hertz.
    Am,
}

/// Internal dispatch wrapper holding the active demodulator.
pub(crate) enum DemodImpl {
    Nfm(NfmDemod),
    Am(AmDemod),
}

impl DemodImpl {
    /// Demodulate a buffer of complex baseband IQ into (audio, in-channel power dB).
    pub fn process(&mut self, iq: &[Complex32]) -> (Vec<f32>, f32) {
        match self {
            DemodImpl::Nfm(d) => d.process(iq),
            DemodImpl::Am(d) => d.process(iq),
        }
    }

    /// Apply a frequency-offset correction rotator (AFC). A no-op for AM (AFC is
    /// NFM-only in the pipeline).
    pub fn set_freq_offset(&mut self, offset_hz: f32) {
        match self {
            DemodImpl::Nfm(d) => d.set_freq_offset(offset_hz),
            DemodImpl::Am(_) => {}
        }
    }
}
