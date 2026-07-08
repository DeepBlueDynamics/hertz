//! The shared event bus: a control channel for `Event`s (squelch, signal level,
//! channel activity, recording saved, …) and a **separate** high-rate channel for
//! [`AudioFrame`]s so audio can never evict control events.

use hertz_types::Event;
use tokio::sync::{broadcast, mpsc};

use crate::audio::AudioFrame;

/// Broadcast capacities. Control is generous (subscribers may lag briefly); audio is
/// bounded and drops on backpressure (real-time audio that can't keep up is useless).
const CONTROL_CAPACITY: usize = 1024;
const AUDIO_CAPACITY: usize = 256;

/// The daemon-wide fan-out for events and audio. Cloneable (the senders are
/// internally `Arc`ed); hand clones to DSP pumps, the recorder, and HTTP handlers.
#[derive(Clone)]
pub struct EventBus {
    control_tx: broadcast::Sender<Event>,
    audio_tx: broadcast::Sender<AudioFrame>,
}

impl EventBus {
    pub fn new() -> Self {
        let (control_tx, _) = broadcast::channel(CONTROL_CAPACITY);
        let (audio_tx, _) = broadcast::channel(AUDIO_CAPACITY);
        Self {
            control_tx,
            audio_tx,
        }
    }

    /// Subscribe to control events (never audio).
    pub fn subscribe_events(&self) -> broadcast::Receiver<Event> {
        self.control_tx.subscribe()
    }

    /// Subscribe to the audio stream.
    pub fn subscribe_audio(&self) -> broadcast::Receiver<AudioFrame> {
        self.audio_tx.subscribe()
    }

    /// Publish a control event. Returns the receiver count; a send with no
    /// subscribers is not an error (quietly dropped).
    pub fn publish_event(&self, ev: Event) {
        let _ = self.control_tx.send(ev);
    }

    /// Publish an audio frame.
    pub fn publish_audio(&self, af: AudioFrame) {
        let _ = self.audio_tx.send(af);
    }

    /// A sender pair for a sync DSP thread to feed events + audio into pump tasks.
    /// Returns `(event_sender, audio_sender)` as tokio unbounded senders (their
    /// `send` methods are sync-safe — no `.await` needed), bridging sync→async.
    pub fn spawn_pumps(
        self,
    ) -> (
        mpsc::UnboundedSender<Event>,
        mpsc::UnboundedSender<AudioFrame>,
    ) {
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<Event>();
        let (au_tx, mut au_rx) = mpsc::unbounded_channel::<AudioFrame>();
        let bus_ev = self.clone();
        tokio::spawn(async move {
            while let Some(ev) = ev_rx.recv().await {
                bus_ev.publish_event(ev);
            }
        });
        let bus_au = self;
        tokio::spawn(async move {
            while let Some(af) = au_rx.recv().await {
                bus_au.publish_audio(af);
            }
        });
        (ev_tx, au_tx)
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}
