//! Local speaker output + console squelch log for `hertzd --listen` — the
//! vhf_monitor experience: squelch-gated demod audio straight to the default output
//! device, and a one-line console entry per squelch open/close.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use hertz_types::Event;
use tracing::{info, warn};

use crate::bus::EventBus;

/// Demod audio rate on the bus (pipeline `AUDIO_SAMPLE_RATE`).
const BUS_AUDIO_RATE: f64 = 48_000.0;
/// Playback volume — vhf_monitor's `MONITOR_VOLUME`.
const MONITOR_VOLUME: f32 = 0.035;
/// ~2 s of buffered audio at the bus rate.
const RING_SAMPLES: usize = 96_000;
/// Jitter cushion: audio arrives in 200 ms DSP frames, so start (and restart after
/// an underrun) only once this much is queued — otherwise every frame-timing wobble
/// plays as a gap. vhf_monitor gets the same cushion from its 1 s prebuffer.
const PRIME_SAMPLES: usize = 14_400; // 300 ms
/// Wait before reopening the output after it fails (device unplugged, monitor asleep).
const REOPEN_DELAY: Duration = Duration::from_secs(2);

/// Ring consumer shared across stream rebuilds (only one stream holds it at a time).
type SharedConsumer = Arc<Mutex<rtrb::Consumer<f32>>>;

/// Start speaker playback and the console squelch log. Must be called inside the
/// tokio runtime. Playback failure (no output device) is logged, not fatal.
pub fn spawn(bus: &EventBus) {
    let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(RING_SAMPLES);

    // cpal streams are !Send on some hosts: build and own the stream on its own
    // thread, and rebuild it on the current default device whenever it fails.
    let consumer: SharedConsumer = Arc::new(Mutex::new(consumer));
    thread::Builder::new()
        .name("speaker".into())
        .spawn(move || loop {
            let (err_tx, err_rx) = mpsc::channel();
            match build_stream(Arc::clone(&consumer), err_tx) {
                Ok(_stream) => {
                    // Hold the stream until cpal reports it dead.
                    if let Ok(e) = err_rx.recv() {
                        warn!("speaker stream error: {e}; reopening output");
                    }
                }
                Err(e) => warn!("speaker output unavailable: {e}; retrying"),
            }
            thread::sleep(REOPEN_DELAY);
        })
        .expect("spawn speaker thread");

    let mut audio_rx = bus.subscribe_audio();
    tokio::spawn(async move {
        loop {
            match audio_rx.recv().await {
                Ok(frame) => {
                    for s in frame.pcm {
                        let _ = producer.push(s);
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
                    freq_hz,
                    open,
                    signal_db,
                    classification,
                    ..
                }) => {
                    // Open-time class is a single-frame guess; the close event carries
                    // the whole-transmission verdict.
                    let class = if open { "" } else { classification.as_str() };
                    info!(
                        "SQUELCH {} | Ch {} | {:.3} MHz | Signal: {:.1}dB | {}",
                        if open { "OPEN  " } else { "CLOSED" },
                        channel.as_deref().unwrap_or("?"),
                        freq_hz as f64 / 1e6,
                        signal_db,
                        class
                    );
                }
                Ok(Event::Transcription { channel, text, .. }) => {
                    info!(
                        "TRANSCRIPT | Ch {} | {text}",
                        channel.as_deref().unwrap_or("?")
                    );
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
    });
}

fn build_stream(
    consumer: SharedConsumer,
    err_tx: mpsc::Sender<cpal::StreamError>,
) -> anyhow::Result<cpal::Stream> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no default output device"))?;
    let config = device.default_output_config()?;
    let device_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    let stream_config = cpal::StreamConfig {
        channels: config.channels(),
        sample_rate: cpal::SampleRate(device_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    // Linear-interpolating resampler from the bus rate to the device rate.
    let step = BUS_AUDIO_RATE / device_rate as f64;
    let mut pos = 1.0f64;
    let mut prev = 0.0f32;
    let mut next = 0.0f32;
    let mut primed = false;

    let stream = device.build_output_stream(
        &stream_config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            // Uncontended: only the live stream touches the consumer.
            let Ok(mut consumer) = consumer.lock() else {
                data.fill(0.0);
                return;
            };
            if !primed && consumer.slots() >= PRIME_SAMPLES {
                primed = true;
            }
            for frame in data.chunks_mut(channels) {
                if !primed {
                    frame.fill(0.0);
                    continue;
                }
                while pos >= 1.0 {
                    prev = next;
                    match consumer.pop() {
                        Ok(s) => next = s,
                        Err(_) => {
                            // Underrun: go quiet and re-prime.
                            primed = false;
                            next = 0.0;
                        }
                    }
                    pos -= 1.0;
                }
                let s = (prev + (next - prev) * pos as f32) * MONITOR_VOLUME;
                pos += step;
                for out in frame.iter_mut() {
                    *out = s;
                }
            }
        },
        move |e| {
            let _ = err_tx.send(e);
        },
        None,
    )?;
    stream.play()?;
    info!(
        "speaker: playing squelch-gated audio on '{}' @ {device_rate} Hz",
        device.name().unwrap_or_default()
    );
    Ok(stream)
}
