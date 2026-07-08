use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde::{Deserialize, Serialize};

use crate::broadcast::{AudioBroadcaster, AudioMessage};

const MAX_ACTIVITY: usize = 200;
const MAX_TRANSCRIPTIONS: usize = 50;

#[derive(Clone, Debug, Serialize)]
pub struct ActivityEntry {
    pub timestamp: String,
    pub channel: Option<u8>,
    pub freq: u32,
    pub open: bool,
    pub signal_db: f32,
    pub classification: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct TranscriptEntry {
    pub timestamp: String,
    pub channel: Option<u8>,
    pub freq: u32,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct VoicePaintingData {
    pub description: String,
    pub regions: Vec<PaintRegion>,
    pub timestamp: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaintRegion {
    pub label: String,
    pub freq_lo: f32,
    pub freq_hi: f32,
    pub time_start: f32,
    pub time_end: f32,
    pub color: String,
    pub opacity: f32,
    pub style: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WindowConfig {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub visible: bool,
    pub minimized: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WindowLayout {
    pub radio: WindowConfig,
    pub spectrogram: WindowConfig,
    pub analysis: WindowConfig,
    pub events: WindowConfig,
    pub transcript: WindowConfig,
    pub extracts: WindowConfig,
    pub controls: WindowConfig,
}

impl WindowLayout {
    pub fn default_layout() -> Self {
        Self {
            radio: WindowConfig {
                x: 20,
                y: 20,
                w: 400,
                h: 200,
                visible: true,
                minimized: false,
            },
            spectrogram: WindowConfig {
                x: 20,
                y: 240,
                w: 800,
                h: 300,
                visible: true,
                minimized: false,
            },
            analysis: WindowConfig {
                x: 20,
                y: 560,
                w: 400,
                h: 200,
                visible: true,
                minimized: false,
            },
            events: WindowConfig {
                x: 840,
                y: 20,
                w: 260,
                h: 300,
                visible: true,
                minimized: false,
            },
            transcript: WindowConfig {
                x: 840,
                y: 340,
                w: 260,
                h: 200,
                visible: true,
                minimized: false,
            },
            extracts: WindowConfig {
                x: 840,
                y: 560,
                w: 260,
                h: 200,
                visible: true,
                minimized: false,
            },
            controls: WindowConfig {
                x: 440,
                y: 20,
                w: 380,
                h: 60,
                visible: true,
                minimized: false,
            },
        }
    }
}

pub struct AgenticState {
    pub activity: VecDeque<ActivityEntry>,
    pub transcriptions: VecDeque<TranscriptEntry>,
    pub signal_db: f32,
    pub noise_floor: f32,
    pub squelch_open: bool,
    pub audio_flatness: f32,
    pub channel: Option<u8>,
    pub freq: u32,
    pub recording: bool,
    pub listen: bool,
    pub screenshot_png: Option<Vec<u8>>,
    pub voice_painting: Option<VoicePaintingData>,
    pub layout: WindowLayout,
}

impl AgenticState {
    pub fn new() -> Self {
        Self {
            activity: VecDeque::with_capacity(MAX_ACTIVITY + 1),
            transcriptions: VecDeque::with_capacity(MAX_TRANSCRIPTIONS + 1),
            signal_db: -100.0,
            noise_floor: -20.0,
            squelch_open: false,
            audio_flatness: 0.8,
            channel: None,
            freq: 0,
            recording: false,
            listen: false,
            screenshot_png: None,
            voice_painting: None,
            layout: WindowLayout::default_layout(),
        }
    }
}

pub type SharedAgenticState = Arc<Mutex<AgenticState>>;

pub fn create_agentic_state() -> SharedAgenticState {
    Arc::new(Mutex::new(AgenticState::new()))
}

/// Spawn a background thread that subscribes to the broadcaster and
/// accumulates state for the agentic API layer.
pub fn start_agentic_subscriber(
    state: SharedAgenticState,
    broadcaster: Arc<AudioBroadcaster>,
    running: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let rx = broadcaster.subscribe();

        while running.load(Ordering::Relaxed) {
            match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(msg) => {
                    let mut s = match state.lock() {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    match msg {
                        AudioMessage::SquelchEvent {
                            channel,
                            freq,
                            open,
                            signal_db,
                            classification,
                        } => {
                            let entry = ActivityEntry {
                                timestamp: chrono::Utc::now()
                                    .format("%Y-%m-%dT%H:%M:%SZ")
                                    .to_string(),
                                channel,
                                freq,
                                open,
                                signal_db,
                                classification,
                            };
                            s.activity.push_front(entry);
                            if s.activity.len() > MAX_ACTIVITY {
                                s.activity.pop_back();
                            }
                            s.squelch_open = open;
                            if open {
                                s.channel = channel;
                                s.freq = freq;
                                s.signal_db = signal_db;
                            }
                        }
                        AudioMessage::SignalLevel {
                            channel,
                            freq,
                            signal_db,
                            noise_floor,
                            squelch_open,
                            audio_flatness,
                        } => {
                            s.signal_db = signal_db;
                            s.noise_floor = noise_floor;
                            s.squelch_open = squelch_open;
                            s.audio_flatness = audio_flatness;
                            if squelch_open {
                                s.channel = channel;
                                s.freq = freq;
                            }
                        }
                        AudioMessage::Transcription {
                            channel,
                            freq,
                            text,
                        } => {
                            let entry = TranscriptEntry {
                                timestamp: chrono::Utc::now()
                                    .format("%Y-%m-%dT%H:%M:%SZ")
                                    .to_string(),
                                channel,
                                freq,
                                text,
                            };
                            s.transcriptions.push_front(entry);
                            if s.transcriptions.len() > MAX_TRANSCRIPTIONS {
                                s.transcriptions.pop_back();
                            }
                        }
                        AudioMessage::ChannelActivity { noise_floor, .. } => {
                            s.noise_floor = noise_floor;
                        }
                        AudioMessage::Audio { signal_db, .. } => {
                            s.signal_db = signal_db;
                        }
                        AudioMessage::VoicePaint { painting } => {
                            if let Ok(vp) = serde_json::from_str::<VoicePaintingData>(&painting) {
                                s.voice_painting = Some(vp);
                            }
                        }
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
}
