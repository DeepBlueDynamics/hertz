//! Live transcription for the console: while a channel's squelch is open, its audio
//! is streamed to Whistle about a second at a time and the words the engine commits
//! are logged as they arrive (`LIVE | Ch 72 | ...`). The stream ends when the squelch
//! closes.
//!
//! Display only. Routing still uses the transcript of the whole recording: on a 37 s
//! real call the live stream got the opening call-up wrong ("The squirrel,
//! replusive squirrel" vs "Repulsive squirrel, repulsive squirrel") because it
//! commits words before it has heard the rest, and it cost ~10x the CPU.

use std::sync::mpsc;
use std::sync::Arc;

use hertz_types::Event;
use tracing::{info, warn};

use crate::bus::EventBus;
use crate::transcribe::WhistleTranscriber;

/// Demod audio rate on the bus.
const BUS_RATE: u32 = 48_000;
/// Feed the engine once this much 16 kHz audio has queued (~1 s, what it expects).
const FEED_SAMPLES: usize = whistle::SAMPLE_RATE as usize;

enum Msg {
    Audio { channel: String, pcm: Vec<f32> },
    Closed { channel: String },
}

/// Start live transcription. Must be called inside the tokio runtime.
pub fn spawn(bus: &EventBus, whistle: Arc<WhistleTranscriber>) {
    let (tx, rx) = mpsc::channel::<Msg>();
    std::thread::Builder::new()
        .name("live-transcribe".into())
        .spawn(move || worker(rx, whistle))
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
}

fn worker(rx: mpsc::Receiver<Msg>, whistle: Arc<WhistleTranscriber>) {
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
                    finish(engine, &whistle, open.take());
                }
                let s = open.get_or_insert_with(|| Stream {
                    channel,
                    pending: Vec::new(),
                });
                s.pending
                    .extend(whistle::resample(&pcm, BUS_RATE, whistle::SAMPLE_RATE));
                if s.pending.len() >= FEED_SAMPLES {
                    let chunk = std::mem::take(&mut s.pending);
                    feed(engine, &whistle, &s.channel, &chunk);
                }
            }
            Msg::Closed { channel } => {
                if open.as_ref().is_some_and(|s| s.channel == channel) {
                    finish(engine, &whistle, open.take());
                }
            }
        }
    }
}

fn feed(engine: &whistle::Whistle, whistle: &WhistleTranscriber, channel: &str, pcm: &[f32]) {
    match engine.stream_process(pcm, &whistle.options()) {
        Ok(u) if !u.text.trim().is_empty() => info!("LIVE | Ch {channel} | {}", u.text.trim()),
        Ok(_) => {}
        Err(e) => warn!("LIVE | Ch {channel} | stream failed: {e}"),
    }
}

/// Feed what's left and end the stream; the engine commits its tail.
fn finish(engine: &whistle::Whistle, whistle: &WhistleTranscriber, stream: Option<Stream>) {
    let Some(s) = stream else { return };
    if !s.pending.is_empty() {
        feed(engine, whistle, &s.channel, &s.pending);
    }
    match engine.stream_stop() {
        Ok(u) if !u.text.trim().is_empty() => {
            info!("LIVE | Ch {} | {} [end]", s.channel, u.text.trim())
        }
        Ok(_) => info!("LIVE | Ch {} | [end]", s.channel),
        Err(e) => warn!("LIVE | Ch {} | stream stop failed: {e}", s.channel),
    }
}
