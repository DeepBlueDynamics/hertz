use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use serde::Serialize;
use std::sync::Mutex;

/// Messages broadcast to all subscribers (web clients, audio stream, etc.)
#[derive(Clone, Debug)]
pub enum AudioMessage {
    /// Raw audio samples from a channel
    Audio {
        channel: Option<u8>,
        freq: u32,
        samples: Vec<f32>,
        signal_db: f32,
    },
    /// Periodic channel activity snapshot
    ChannelActivity {
        active: Vec<ChannelInfo>,
        noise_floor: f32,
    },
    /// Squelch open/close event
    SquelchEvent {
        channel: Option<u8>,
        freq: u32,
        open: bool,
        signal_db: f32,
        classification: String,
    },
    /// Signal level info sent every frame (for always-active viz)
    SignalLevel {
        channel: Option<u8>,
        freq: u32,
        signal_db: f32,
        noise_floor: f32,
        squelch_open: bool,
        audio_flatness: f32,
    },
    /// Transcription result from a completed transmission
    Transcription {
        channel: Option<u8>,
        freq: u32,
        text: String,
    },
    /// Voice painting analysis result (JSON string)
    VoicePaint {
        painting: String,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct ChannelInfo {
    pub channel: u8,
    pub label: String,
    pub freq: u32,
    pub signal_db: f32,
    pub classification: String,
}

struct Subscriber {
    tx: Sender<AudioMessage>,
    #[allow(dead_code)]
    id: u64,
}

/// Thread-safe audio broadcaster. Multiple consumers subscribe and receive
/// a copy of every AudioMessage. Disconnected subscribers are auto-pruned.
pub struct AudioBroadcaster {
    subscribers: Mutex<Vec<Subscriber>>,
    next_id: Mutex<u64>,
}

impl AudioBroadcaster {
    pub fn new() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            next_id: Mutex::new(1),
        }
    }

    /// Register a new subscriber. Returns a Receiver that will get all
    /// future broadcast messages. Buffer holds up to 64 messages before
    /// dropping (non-blocking sends).
    pub fn subscribe(&self) -> Receiver<AudioMessage> {
        let (tx, rx) = bounded(64);
        let mut subs = self.subscribers.lock().unwrap();
        let mut id = self.next_id.lock().unwrap();
        subs.push(Subscriber { tx, id: *id });
        *id += 1;
        rx
    }

    /// Broadcast a message to all subscribers. Disconnected or full
    /// subscribers are removed automatically.
    pub fn broadcast(&self, msg: AudioMessage) {
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|sub| {
            match sub.tx.try_send(msg.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    // Slow consumer — drop the message but keep subscriber
                    true
                }
                Err(TrySendError::Disconnected(_)) => {
                    // Subscriber gone — remove
                    false
                }
            }
        });
    }

    /// Number of active subscribers
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.lock().unwrap().len()
    }
}
