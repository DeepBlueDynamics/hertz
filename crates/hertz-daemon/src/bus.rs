//! The shared event bus: a control channel for `Event`s (squelch, signal level,
//! channel activity, recording saved, …), a **separate** high-rate channel for
//! [`AudioFrame`]s so audio can never evict control events, and a third channel
//! for [`SpectrumFrame`]s (the live waterfall feed, opt-in over WS).

use hertz_types::Event;
use tokio::sync::{broadcast, mpsc};

use crate::audio::AudioFrame;
use crate::spectrum::SpectrumFrame;

/// Broadcast capacities. Control is generous (subscribers may lag briefly); audio
/// and spectrum are bounded and drop on backpressure (real-time streams that
/// can't keep up are useless — the next frame is what matters).
const CONTROL_CAPACITY: usize = 1024;
const AUDIO_CAPACITY: usize = 256;
const SPECTRUM_CAPACITY: usize = 64;

/// The daemon-wide fan-out for events, audio, and spectrum. Cloneable (the
/// senders are internally `Arc`ed); hand clones to DSP pumps, the recorder, and
/// HTTP handlers.
#[derive(Clone)]
pub struct EventBus {
    control_tx: broadcast::Sender<Event>,
    audio_tx: broadcast::Sender<AudioFrame>,
    spectrum_tx: broadcast::Sender<SpectrumFrame>,
}

impl EventBus {
    pub fn new() -> Self {
        let (control_tx, _) = broadcast::channel(CONTROL_CAPACITY);
        let (audio_tx, _) = broadcast::channel(AUDIO_CAPACITY);
        let (spectrum_tx, _) = broadcast::channel(SPECTRUM_CAPACITY);
        Self {
            control_tx,
            audio_tx,
            spectrum_tx,
        }
    }

    /// Subscribe to control events (never audio/spectrum).
    pub fn subscribe_events(&self) -> broadcast::Receiver<Event> {
        self.control_tx.subscribe()
    }

    /// Subscribe to the audio stream.
    pub fn subscribe_audio(&self) -> broadcast::Receiver<AudioFrame> {
        self.audio_tx.subscribe()
    }

    /// Subscribe to the spectrum stream (waterfall frames).
    pub fn subscribe_spectrum(&self) -> broadcast::Receiver<SpectrumFrame> {
        self.spectrum_tx.subscribe()
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

    /// Publish a spectrum frame.
    pub fn publish_spectrum(&self, sf: SpectrumFrame) {
        let _ = self.spectrum_tx.send(sf);
    }

    /// A sender triple for a sync DSP thread to feed events + audio + spectrum
    /// into pump tasks. Returns `(event, audio, spectrum)` senders as tokio
    /// unbounded senders (their `send` methods are sync-safe — no `.await`
    /// needed), bridging sync→async.
    pub fn spawn_pumps(
        self,
    ) -> (
        mpsc::UnboundedSender<Event>,
        mpsc::UnboundedSender<AudioFrame>,
        mpsc::UnboundedSender<SpectrumFrame>,
    ) {
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<Event>();
        let (au_tx, mut au_rx) = mpsc::unbounded_channel::<AudioFrame>();
        let (sp_tx, mut sp_rx) = mpsc::unbounded_channel::<SpectrumFrame>();
        let bus_ev = self.clone();
        tokio::spawn(async move {
            while let Some(ev) = ev_rx.recv().await {
                bus_ev.publish_event(ev);
            }
        });
        let bus_au = self.clone();
        tokio::spawn(async move {
            while let Some(af) = au_rx.recv().await {
                bus_au.publish_audio(af);
            }
        });
        let bus_sp = self;
        tokio::spawn(async move {
            while let Some(sf) = sp_rx.recv().await {
                bus_sp.publish_spectrum(sf);
            }
        });
        (ev_tx, au_tx, sp_tx)
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}
