//! Wire protocol shared by the daemon, the TUI, MCP clients, and external subscribers.
//!
//! Three things live here:
//! 1. [`WsServerMsg`] — the JSON text-frame envelope for `/stream` and SSE `/events`:
//!    either a bus [`Event`](crate::Event) or a one-shot [`WsServerMsg::Hello`].
//! 2. The **binary audio frame** layout (gnosis-compatible + a leading dongle byte),
//!    with [`encode_audio_frame`] / [`decode_audio_frame`].
//! 3. REST DTOs for the `/api/*` surface ([`StatusResponse`], [`TuneRequest`], …).
//!
//! This module owns only new types — it does not touch the existing `Event` /
//! `DaemonConfig` types in `crate`. The TUI builds against `hertz_types::wire::*`
//! (re-exported from the crate root).

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{DongleRole, Event};

// ---------------------------------------------------------------------------
// WebSocket / SSE message envelope (JSON text frames)
// ---------------------------------------------------------------------------

/// One message from the daemon to a WS/SSE subscriber. Audio is NOT carried here —
/// it goes as a binary WS frame (see [`encode_audio_frame`]); every other bus event
/// rides as [`WsServerMsg::Event`].
///
/// Serialized with serde default (externally tagged) so a client can match on the
/// outer key: `{"Event": { "type": "SquelchEvent", … }}` or `{"Hello": {…}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WsServerMsg {
    /// A bus event (everything except raw audio).
    Event(Event),
    /// Sent once on WS connect: protocol version + the dongle roster.
    Hello {
        version: String,
        dongles: Vec<DongleSummary>,
    },
}

/// A dongle's summary as advertised to clients (WS Hello, REST `/api/dongles`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DongleSummary {
    pub id: String,
    pub serial: String,
    pub role: DongleRole,
    pub online: bool,
    pub bandplan: Option<String>,
    pub tap_channel: Option<String>,
    pub groups: Vec<String>,
    pub freq_hz: u64,
    pub squelch_db: f32,
    pub recording: bool,
    pub listening: bool,
    pub message: Option<String>,
}

// ---------------------------------------------------------------------------
// REST DTOs (`/api/*`)
// ---------------------------------------------------------------------------

/// `GET /api/status`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StatusResponse {
    pub version: String,
    pub uptime_sec: f32,
    pub dongles: Vec<DongleSummary>,
    pub channels_loaded: usize,
    /// True when a non-loopback client must present a bearer token.
    pub auth_required: bool,
}

/// `POST /api/dongles/{id}/tune`. Exactly one of `channel_id` / `freq_hz` must be set
/// (the daemon returns 400 otherwise).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TuneRequest {
    pub channel_id: Option<String>,
    pub freq_hz: Option<u64>,
}

impl TuneRequest {
    /// `Ok(())` if exactly one selector is present.
    pub fn validate(&self) -> Result<(), WireError> {
        match (self.channel_id.as_ref(), self.freq_hz) {
            (Some(_), Some(_)) => Err(WireError::BadRequest(
                "tune: provide channel_id OR freq_hz, not both".into(),
            )),
            (None, None) => Err(WireError::BadRequest(
                "tune: provide one of channel_id or freq_hz".into(),
            )),
            _ => Ok(()),
        }
    }
}

/// `POST /api/dongles/{id}/squelch`. Squelch clamped to 0..=30 dB by the daemon.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SquelchRequest {
    pub squelch_db: f32,
}

/// `POST /api/dongles/{id}/recording`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordingRequest {
    pub record: bool,
}

/// `POST /api/dongles/{id}/listen`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ListenRequest {
    pub listen: bool,
}

/// One row of `GET /api/activity`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActivityEntry {
    pub ts_sec: f32,
    pub dongle_id: String,
    pub channel: Option<String>,
    pub freq_hz: u64,
    pub signal_db: f32,
    pub classification: String,
    pub open: bool,
}

/// One row of `GET /api/transcriptions`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub ts_sec: f32,
    pub dongle_id: String,
    pub channel: Option<String>,
    pub freq_hz: u64,
    pub text: String,
}

/// One row of `GET /api/recordings`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordingFileEntry {
    pub filename: String,
    pub freq_hz: u64,
    pub channel: Option<String>,
    pub size_bytes: u64,
    pub modified_ts_sec: f32,
    pub duration_sec: f32,
}

/// `GET /api/doctor` — runtime diagnostic (librtlsdr-free; USB presence is reported
/// by the dongle workers' `online` flags).
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct DoctorReport {
    pub platform: String,
    pub channels_loaded: usize,
    pub dongles: Vec<DongleDoctorEntry>,
    pub messages: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DongleDoctorEntry {
    pub id: String,
    pub serial: String,
    pub online: bool,
    pub dropped_samples: usize,
    pub read_errors: usize,
    pub epoch: u64,
}

// ---------------------------------------------------------------------------
// Binary audio frame
// ---------------------------------------------------------------------------
//
// Layout (little-endian throughout):
//   [u8  dongle_idx ][u32 channel_key][u32 freq_hz][f32 signal_db][f32 pcm ...]
//   off 0           off 1            off 5         off 9          off 13
//
// `channel_key` is an opaque u32 the daemon assigns per channel (e.g. a stable hash
// of the channel id); clients echo it back in filters. `freq_hz` is u32 (radio
// frequencies fit) for gnosis compatibility — the bus `Event` carries u64.

pub const AUDIO_FRAME_HEADER_BYTES: usize = 13;
pub const AUDIO_FRAME_DONGLE_OFFSET: usize = 0;
pub const AUDIO_FRAME_CHANNEL_KEY_OFFSET: usize = 1;
pub const AUDIO_FRAME_FREQ_OFFSET: usize = 5;
pub const AUDIO_FRAME_SIGNAL_OFFSET: usize = 9;
pub const AUDIO_FRAME_PCM_OFFSET: usize = 13;

/// Encode one audio frame into a freshly-allocated byte buffer.
pub fn encode_audio_frame(
    dongle_idx: u8,
    channel_key: u32,
    freq_hz: u32,
    signal_db: f32,
    pcm: &[f32],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(AUDIO_FRAME_HEADER_BYTES + pcm.len() * 4);
    out.push(dongle_idx);
    out.extend_from_slice(&channel_key.to_le_bytes());
    out.extend_from_slice(&freq_hz.to_le_bytes());
    out.extend_from_slice(&signal_db.to_le_bytes());
    for &s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// A decoded audio frame (owned).
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedAudioFrame {
    pub dongle_idx: u8,
    pub channel_key: u32,
    pub freq_hz: u32,
    pub signal_db: f32,
    pub pcm: Vec<f32>,
}

/// Errors from [`decode_audio_frame`] / [`decode_spectrum_frame`] / DTO validation.
#[derive(Debug, Error)]
pub enum WireError {
    #[error("buffer too short for frame header ({len} < {need})")]
    ShortFrame { len: usize, need: usize },
    #[error("trailing payload is not a whole number of f32 ({rem} trailing bytes)")]
    TrailingBytes { rem: usize },
    #[error("bad spectrum frame magic byte: got 0x{got:02X}, want 0x{want:02X}")]
    BadMagic { got: u8, want: u8 },
    #[error("bad request: {0}")]
    BadRequest(String),
}

/// Decode an audio frame. The buffer must be at least [`AUDIO_FRAME_HEADER_BYTES`]
/// long and its trailing bytes (after the 13-byte header) a whole number of f32s.
pub fn decode_audio_frame(buf: &[u8]) -> Result<DecodedAudioFrame, WireError> {
    if buf.len() < AUDIO_FRAME_HEADER_BYTES {
        return Err(WireError::ShortFrame {
            len: buf.len(),
            need: AUDIO_FRAME_HEADER_BYTES,
        });
    }
    let dongle_idx = buf[AUDIO_FRAME_DONGLE_OFFSET];
    let channel_key = u32::from_le_bytes(
        buf[AUDIO_FRAME_CHANNEL_KEY_OFFSET..][..4]
            .try_into()
            .unwrap(),
    );
    let freq_hz = u32::from_le_bytes(buf[AUDIO_FRAME_FREQ_OFFSET..][..4].try_into().unwrap());
    let signal_db = f32::from_le_bytes(buf[AUDIO_FRAME_SIGNAL_OFFSET..][..4].try_into().unwrap());

    let pcm_bytes = &buf[AUDIO_FRAME_PCM_OFFSET..];
    if !pcm_bytes.len().is_multiple_of(4) {
        return Err(WireError::TrailingBytes {
            rem: pcm_bytes.len() % 4,
        });
    }
    let pcm: Vec<f32> = pcm_bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();

    Ok(DecodedAudioFrame {
        dongle_idx,
        channel_key,
        freq_hz,
        signal_db,
        pcm,
    })
}

// ---------------------------------------------------------------------------
// Binary spectrum frame (T4.1)
// ---------------------------------------------------------------------------
//
// Layout (little-endian throughout):
//   [u8 0x02 magic ][u8 dongle_idx][f64 center_hz][f64 span_hz]
//   [u8 squelch_open][u16 n][f32 bins_db ...]
//   off 0           off 1          off 2          off 10
//   off 18          off 19         off 21
//
// The leading magic byte 0x02 distinguishes a spectrum frame from an audio
// frame on the same WS connection: an audio frame's first byte is the dongle
// index (0..N-1), never a typed tag. A client that subscribed to both streams
// discriminates by `buf[0] == SPECTRUM_FRAME_MAGIC` (then validates the full
// decode); anything else is an audio frame. Requesting `?spectrum=` is opt-in
// (default off) so the ~40-80 KB/s stream only flows to clients that want it.

/// Magic byte at offset 0 of a spectrum binary frame (audio frames have no tag —
/// their first byte is the dongle index — so this byte is the discriminator).
pub const SPECTRUM_FRAME_MAGIC: u8 = 0x02;
pub const SPECTRUM_FRAME_HEADER_BYTES: usize = 21;
pub const SPECTRUM_FRAME_MAGIC_OFFSET: usize = 0;
pub const SPECTRUM_FRAME_DONGLE_OFFSET: usize = 1;
pub const SPECTRUM_FRAME_CENTER_OFFSET: usize = 2;
pub const SPECTRUM_FRAME_SPAN_OFFSET: usize = 10;
pub const SPECTRUM_FRAME_SQUELCH_OFFSET: usize = 18;
pub const SPECTRUM_FRAME_N_OFFSET: usize = 19;
pub const SPECTRUM_FRAME_BINS_OFFSET: usize = 21;

/// Encode one spectrum frame into a freshly-allocated byte buffer. `bins_db.len()`
/// must fit in a `u16`; the caller is expected to keep FFT sizes modest (1024).
#[track_caller]
pub fn encode_spectrum_frame(
    dongle_idx: u8,
    center_hz: f64,
    span_hz: f64,
    squelch_open: bool,
    bins_db: &[f32],
) -> Vec<u8> {
    assert!(
        bins_db.len() <= u16::MAX as usize,
        "spectrum bin count {} exceeds u16",
        bins_db.len()
    );
    let mut out = Vec::with_capacity(SPECTRUM_FRAME_HEADER_BYTES + bins_db.len() * 4);
    out.push(SPECTRUM_FRAME_MAGIC);
    out.push(dongle_idx);
    out.extend_from_slice(&center_hz.to_le_bytes());
    out.extend_from_slice(&span_hz.to_le_bytes());
    out.push(if squelch_open { 1 } else { 0 });
    out.extend_from_slice(&(bins_db.len() as u16).to_le_bytes());
    for &b in bins_db {
        out.extend_from_slice(&b.to_le_bytes());
    }
    out
}

/// A decoded spectrum frame (owned).
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedSpectrumFrame {
    pub dongle_idx: u8,
    pub center_hz: f64,
    pub span_hz: f64,
    pub squelch_open: bool,
    /// Magnitude per FFT bin in dB, fft-shifted so bin 0 = center - span/2.
    pub bins_db: Vec<f32>,
}

/// Decode a spectrum frame. The buffer must be at least
/// [`SPECTRUM_FRAME_HEADER_BYTES`] long, begin with [`SPECTRUM_FRAME_MAGIC`],
/// and carry exactly the bin count declared in its header as a whole number of
/// f32s.
pub fn decode_spectrum_frame(buf: &[u8]) -> Result<DecodedSpectrumFrame, WireError> {
    if buf.len() < SPECTRUM_FRAME_HEADER_BYTES {
        return Err(WireError::ShortFrame {
            len: buf.len(),
            need: SPECTRUM_FRAME_HEADER_BYTES,
        });
    }
    if buf[SPECTRUM_FRAME_MAGIC_OFFSET] != SPECTRUM_FRAME_MAGIC {
        return Err(WireError::BadMagic {
            got: buf[SPECTRUM_FRAME_MAGIC_OFFSET],
            want: SPECTRUM_FRAME_MAGIC,
        });
    }
    let dongle_idx = buf[SPECTRUM_FRAME_DONGLE_OFFSET];
    let center_hz =
        f64::from_le_bytes(buf[SPECTRUM_FRAME_CENTER_OFFSET..][..8].try_into().unwrap());
    let span_hz = f64::from_le_bytes(buf[SPECTRUM_FRAME_SPAN_OFFSET..][..8].try_into().unwrap());
    let squelch_open = buf[SPECTRUM_FRAME_SQUELCH_OFFSET] != 0;
    let n = u16::from_le_bytes(buf[SPECTRUM_FRAME_N_OFFSET..][..2].try_into().unwrap()) as usize;

    let bin_bytes = &buf[SPECTRUM_FRAME_BINS_OFFSET..];
    if !bin_bytes.len().is_multiple_of(4) {
        return Err(WireError::TrailingBytes {
            rem: bin_bytes.len() % 4,
        });
    }
    let declared = bin_bytes.len() / 4;
    if declared != n {
        return Err(WireError::BadRequest(format!(
            "spectrum frame bin count mismatch: header n={n}, trailing={declared}"
        )));
    }
    let bins_db: Vec<f32> = bin_bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();

    Ok(DecodedSpectrumFrame {
        dongle_idx,
        center_hz,
        span_hz,
        squelch_open,
        bins_db,
    })
}

// (REST DTOs reference only plain primitive fields; no re-export needed.)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChannelActivityInfo, VoicePaintingData};

    #[test]
    fn audio_frame_round_trip() {
        let pcm = vec![0.0, 0.5, -0.5, 1.0, -1.0, 0.1234];
        let bytes = encode_audio_frame(2, 0xCAFEBABE, 156_800_000, -12.5, &pcm);
        assert!(bytes.len() == AUDIO_FRAME_HEADER_BYTES + pcm.len() * 4);
        let decoded = decode_audio_frame(&bytes).expect("decode");
        assert_eq!(decoded.dongle_idx, 2);
        assert_eq!(decoded.channel_key, 0xCAFEBABE);
        assert_eq!(decoded.freq_hz, 156_800_000);
        assert!((decoded.signal_db - (-12.5)).abs() < 1e-6);
        assert_eq!(decoded.pcm.len(), pcm.len());
        for (a, b) in decoded.pcm.iter().zip(&pcm) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn audio_frame_rejects_short_and_trailing() {
        let short = [0u8; 5];
        assert!(matches!(
            decode_audio_frame(&short),
            Err(WireError::ShortFrame { .. })
        ));
        // 13-byte header + 2 trailing bytes → not a whole f32.
        let mut bad = vec![0u8; AUDIO_FRAME_HEADER_BYTES];
        bad.extend_from_slice(&[1, 2]);
        assert!(matches!(
            decode_audio_frame(&bad),
            Err(WireError::TrailingBytes { .. })
        ));
    }

    #[test]
    fn ws_hello_round_trip() {
        let msg = WsServerMsg::Hello {
            version: "0.1.0".into(),
            dongles: vec![DongleSummary {
                id: "MARINE01".into(),
                serial: "MARINE01".into(),
                role: DongleRole::Channelized,
                online: true,
                bandplan: Some("marine-vhf-us".into()),
                tap_channel: Some("16".into()),
                groups: vec![],
                freq_hz: 156_737_500,
                squelch_db: 12.0,
                recording: true,
                listening: false,
                message: None,
            }],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: WsServerMsg = serde_json::from_str(&json).unwrap();
        match back {
            WsServerMsg::Hello { version, dongles } => {
                assert_eq!(version, "0.1.0");
                assert_eq!(dongles.len(), 1);
            }
            WsServerMsg::Event(_) => panic!("expected Hello"),
        }
    }

    /// Every Event variant must serialize and parse through the WS envelope.
    #[test]
    fn every_event_variant_round_trips() {
        let mk_channel = || ChannelActivityInfo {
            channel: "16".into(),
            label: "SAFETY".into(),
            freq_hz: 156_800_000,
            signal_db: -9.0,
            classification: "VOICE".into(),
        };
        let events: Vec<Event> = vec![
            Event::Audio {
                dongle_id: "D1".into(),
                channel: Some("16".into()),
                freq_hz: 156_800_000,
                samples: vec![0.1, 0.2],
                signal_db: -9.0,
            },
            Event::SignalLevel {
                dongle_id: "D1".into(),
                channel: None,
                freq_hz: 156_800_000,
                signal_db: -12.0,
                noise_floor: -40.0,
                squelch_open: false,
                audio_flatness: 0.8,
            },
            Event::SquelchEvent {
                dongle_id: "D1".into(),
                channel: Some("16".into()),
                freq_hz: 156_800_000,
                open: true,
                signal_db: -9.0,
                classification: "VOICE".into(),
            },
            Event::ChannelActivity {
                dongle_id: "D1".into(),
                active: vec![mk_channel()],
                noise_floor: -40.0,
            },
            Event::Transcription {
                dongle_id: "D1".into(),
                channel: Some("16".into()),
                freq_hz: 156_800_000,
                text: "securite".into(),
            },
            Event::Translation {
                dongle_id: "D1".into(),
                channel: Some("16".into()),
                freq_hz: 156_800_000,
                text: "safety".into(),
                language: "en".into(),
            },
            Event::RecordingSaved {
                dongle_id: "D1".into(),
                channel: Some("16".into()),
                freq_hz: 156_800_000,
                filename: "transmission_x.wav".into(),
                filepath: "/data/recordings/transmission_x.wav".into(),
                duration_sec: 2.1,
            },
            Event::ScanState {
                dongle_id: "D1".into(),
                state: "SCANNING".into(),
                current_channel: None,
                current_freq_hz: 0,
            },
            Event::DongleStatus {
                dongle_id: "D1".into(),
                serial: "MARINE01".into(),
                online: true,
                role: DongleRole::Monitor,
                message: None,
            },
            Event::TxEvent {
                dongle_id: "D1".into(),
                channel: "16".into(),
                freq_hz: 156_800_000,
                status: "blocked".into(),
                text: None,
                reason: Some("policy".into()),
            },
            Event::VoicePaint {
                dongle_id: "D1".into(),
                painting: VoicePaintingData {
                    description: "formants".into(),
                    regions: vec![],
                    timestamp: "2026-07-08T00:00:00Z".into(),
                },
            },
        ];
        for ev in &events {
            let msg = WsServerMsg::Event(ev.clone());
            let json =
                serde_json::to_string(&msg).unwrap_or_else(|e| panic!("serialize {ev:?}: {e}"));
            let back: WsServerMsg =
                serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {json}: {e}"));
            // Round-trip must land back on an Event variant (not Hello).
            assert!(
                matches!(back, WsServerMsg::Event(_)),
                "not an Event: {json}"
            );
        }
    }

    #[test]
    fn tune_request_validate() {
        assert!(TuneRequest {
            channel_id: Some("16".into()),
            freq_hz: None,
        }
        .validate()
        .is_ok());
        assert!(TuneRequest {
            channel_id: None,
            freq_hz: Some(156_800_000),
        }
        .validate()
        .is_ok());
        assert!(TuneRequest {
            channel_id: None,
            freq_hz: None,
        }
        .validate()
        .is_err());
        assert!(TuneRequest {
            channel_id: Some("16".into()),
            freq_hz: Some(156_800_000),
        }
        .validate()
        .is_err());
    }

    #[test]
    fn spectrum_frame_round_trip() {
        let bins = vec![-90.0, -80.5, -60.0, -40.25, -10.0, -120.0, 0.0, -3.5];
        let bytes = encode_spectrum_frame(3, 156_800_000.0, 240_000.0, true, &bins);
        assert_eq!(bytes[0], SPECTRUM_FRAME_MAGIC);
        assert_eq!(bytes.len(), SPECTRUM_FRAME_HEADER_BYTES + bins.len() * 4);
        let decoded = decode_spectrum_frame(&bytes).expect("decode");
        assert_eq!(decoded.dongle_idx, 3);
        assert!((decoded.center_hz - 156_800_000.0).abs() < 1e-9);
        assert!((decoded.span_hz - 240_000.0).abs() < 1e-9);
        assert!(decoded.squelch_open);
        assert_eq!(decoded.bins_db.len(), bins.len());
        for (a, b) in decoded.bins_db.iter().zip(&bins) {
            assert!((a - b).abs() < 1e-6, "{a} vs {b}");
        }
    }

    #[test]
    fn spectrum_frame_empty_bins_round_trip() {
        // A squelch-closed frame with zero bins is still legal (header-only).
        let bytes = encode_spectrum_frame(0, 0.0, 0.0, false, &[]);
        assert_eq!(bytes.len(), SPECTRUM_FRAME_HEADER_BYTES);
        let decoded = decode_spectrum_frame(&bytes).expect("decode");
        assert!(!decoded.squelch_open);
        assert!(decoded.bins_db.is_empty());
    }

    #[test]
    fn spectrum_frame_rejects_short_badmagic_trailing_mismatch() {
        // Header too short.
        assert!(matches!(
            decode_spectrum_frame(&[0u8; 5]),
            Err(WireError::ShortFrame { .. })
        ));

        // Wrong magic byte (0x01, not 0x02) → looks like an audio frame.
        let mut bad = encode_spectrum_frame(0, 0.0, 0.0, false, &[]);
        bad[0] = 0x01;
        assert!(matches!(
            decode_spectrum_frame(&bad),
            Err(WireError::BadMagic { .. })
        ));

        // Trailing bytes not a whole number of f32.
        let mut trail = encode_spectrum_frame(0, 0.0, 0.0, false, &[]);
        trail.extend_from_slice(&[1, 2]); // 2 stray bytes
        assert!(matches!(
            decode_spectrum_frame(&trail),
            Err(WireError::TrailingBytes { .. })
        ));

        // Header bin count disagrees with the trailing payload.
        let mut mm = encode_spectrum_frame(0, 0.0, 0.0, false, &[0.0]); // n=1
        mm[SPECTRUM_FRAME_N_OFFSET..][..2].copy_from_slice(&2u16.to_le_bytes()); // claim n=2
        assert!(matches!(
            decode_spectrum_frame(&mm),
            Err(WireError::BadRequest(_))
        ));
    }

    #[test]
    fn spectrum_magic_distinguishes_from_audio() {
        // An audio frame for dongle idx 0 begins with 0x00; a spectrum frame
        // always begins with 0x02. That leading byte is the on-wire discriminator.
        let audio = encode_audio_frame(0, 1, 156_800_000, -9.0, &[0.0, 0.5]);
        assert_ne!(audio[0], SPECTRUM_FRAME_MAGIC);
        let spec = encode_spectrum_frame(0, 156_800_000.0, 240_000.0, false, &[-40.0]);
        assert_eq!(spec[0], SPECTRUM_FRAME_MAGIC);
    }
}
