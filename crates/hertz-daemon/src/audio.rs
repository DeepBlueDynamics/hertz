//! Audio frame payload carried on the bus's separate audio channel and encoded as a
//! binary WS frame on `/stream`.

use hertz_types::wire::encode_audio_frame;

/// One chunk of demodulated audio, tagged with its source. Carried on the audio
/// broadcast channel and encoded for WS via [`AudioFrame::encode`].
#[derive(Clone, Debug)]
pub struct AudioFrame {
    pub dongle_id: String,
    /// Index assigned by the daemon to this dongle (matches the binary-frame byte).
    pub dongle_idx: u8,
    /// Opaque per-channel key the daemon assigns.
    pub channel_key: u32,
    pub channel: Option<String>,
    pub freq_hz: u32,
    pub signal_db: f32,
    pub pcm: Vec<f32>,
}

impl AudioFrame {
    /// Encode to the wire binary layout (gnosis + leading dongle byte).
    pub fn encode(&self) -> Vec<u8> {
        encode_audio_frame(
            self.dongle_idx,
            self.channel_key,
            self.freq_hz,
            self.signal_db,
            &self.pcm,
        )
    }
}
