//! Live transcription for the console: while a channel's squelch is open, its audio
//! is streamed to Whistle about a second at a time and the words the engine commits
//! are logged as they arrive (`LIVE | Ch 72 | ...`). The stream ends when the squelch
//! closes.
//!
//! Routing still starts from the transcript of the whole recording: on a 37 s real
//! call the live stream got the opening call-up wrong ("The squirrel, replusive
//! squirrel" vs "Repulsive squirrel, repulsive squirrel") because it commits words
//! before it has heard the rest, and it cost ~10x the CPU. But neither pass is
//! always right (on another call live heard "it'll figure it out" where the full
//! pass wrote "you'll"), so each finished call's live text is kept in
//! [`LiveTranscripts`] for the relay: it is shown alongside the delivered transcript
//! when they differ, and tried for routing when the full transcript names no one.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hertz_types::Event;
use tracing::{info, warn};

use crate::bus::EventBus;
use crate::transcribe::WhistleTranscriber;

/// Demod audio rate on the bus.
const BUS_RATE: u32 = 48_000;
/// Feed the engine once this much 16 kHz audio has queued (~1 s, what it expects).
const FEED_SAMPLES: usize = whistle::SAMPLE_RATE as usize;

/// Each channel's most recent finished live transcript and when it finished.
pub type LiveTranscripts = Arc<Mutex<HashMap<String, (String, Instant)>>>;

enum Msg {
    Audio { channel: String, pcm: Vec<f32> },
    Closed { channel: String },
}

/// Start live transcription. Must be called inside the tokio runtime.
pub fn spawn(bus: &EventBus, whistle: Arc<WhistleTranscriber>, store: LiveTranscripts) {
    let (tx, rx) = mpsc::channel::<Msg>();
    std::thread::Builder::new()
        .name("live-transcribe".into())
        .spawn(move || worker(rx, whistle, store))
        .expect("spawn live transcription thread");

    let mut audio_rx = bus.subscribe_audio();
    let audio_tx = tx.clone();
    tokio::spawn(async move {
        loop {
            match audio_rx.recv().await {
                Ok(frame) => {
                    let msg = Msg::Audio {
                        channel: frame.channel.unwrap_or_else(|| "?".into()),
                        pcm: frame.pcm,
                    };
                    if audio_tx.send(msg).is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
    });
    let mut event_rx = bus.subscribe_events();
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(Event::SquelchEvent {
                    channel,
                    open: false,
                    ..
                }) => {
                    let msg = Msg::Closed {
                        channel: channel.unwrap_or_else(|| "?".into()),
                    };
                    if tx.send(msg).is_err() {
                        break;
                    }
                }
                // A weather sample has no squelch close: it ends with its Weather event.
                Ok(Event::Weather { channel, .. }) => {
                    if tx.send(Msg::Closed { channel }).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
    });
    info!("live: streaming transcription to the console while a squelch is open");
}

/// One open stream: the channel it belongs to and audio not yet fed.
struct Stream {
    channel: String,
    pending: Vec<f32>,
    /// Words committed so far.
    text: Vec<String>,
}

fn worker(rx: mpsc::Receiver<Msg>, whistle: Arc<WhistleTranscriber>, store: LiveTranscripts) {
    let mut open: Option<Stream> = None;
    for msg in rx {
        let engine = match whistle.engine() {
            Ok(e) => e,
            Err(e) => {
                warn!("live: whistle unavailable: {e:#}");
                continue;
            }
        };
        match msg {
            Msg::Audio { channel, pcm } => {
                // The scanner moved to another channel without a close: finish the old one.
                if open.as_ref().is_some_and(|s| s.channel != channel) {
                    finish(engine, &whistle, open.take(), &store);
                }
                let s = open.get_or_insert_with(|| Stream {
                    channel,
                    pending: Vec::new(),
                    text: Vec::new(),
                });
                s.pending
                    .extend(whistle::resample(&pcm, BUS_RATE, whistle::SAMPLE_RATE));
                if s.pending.len() >= FEED_SAMPLES {
                    let chunk = std::mem::take(&mut s.pending);
                    if let Some(t) = feed(engine, &whistle, &s.channel, &chunk) {
                        s.text.push(t);
                    }
                }
            }
            Msg::Closed { channel } => {
                if open.as_ref().is_some_and(|s| s.channel == channel) {
                    finish(engine, &whistle, open.take(), &store);
                }
            }
        }
    }
}

/// Feed audio; returns the words this pass committed, if any.
fn feed(
    engine: &whistle::Whistle,
    whistle: &WhistleTranscriber,
    channel: &str,
    pcm: &[f32],
) -> Option<String> {
    match engine.stream_process(pcm, &whistle.options()) {
        Ok(u) if !u.text.trim().is_empty() => {
            info!("LIVE | Ch {channel} | {}", u.text.trim());
            Some(u.text.trim().to_string())
        }
        Ok(_) => None,
        Err(e) => {
            warn!("LIVE | Ch {channel} | stream failed: {e}");
            None
        }
    }
}

/// Feed what's left and end the stream; the engine commits its tail.
/// Feed what's left, end the stream (the engine commits its tail), and keep the
/// whole call's live text for the relay.
fn finish(
    engine: &whistle::Whistle,
    whistle: &WhistleTranscriber,
    stream: Option<Stream>,
    store: &LiveTranscripts,
) {
    let Some(mut s) = stream else { return };
    if !s.pending.is_empty() {
        let chunk = std::mem::take(&mut s.pending);
        if let Some(t) = feed(engine, whistle, &s.channel, &chunk) {
            s.text.push(t);
        }
    }
    match engine.stream_stop() {
        Ok(u) if !u.text.trim().is_empty() => {
            info!("LIVE | Ch {} | {} [end]", s.channel, u.text.trim());
            s.text.push(u.text.trim().to_string());
        }
        Ok(_) => info!("LIVE | Ch {} | [end]", s.channel),
        Err(e) => warn!("LIVE | Ch {} | stream stop failed: {e}", s.channel),
    }
    let text = s.text.join(" ");
    if !text.is_empty() {
        if let Ok(mut m) = store.lock() {
            m.insert(s.channel, (text, Instant::now()));
        }
    }
}
