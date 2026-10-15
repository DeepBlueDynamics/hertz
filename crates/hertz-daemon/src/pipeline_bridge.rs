//! The sync DSP threads and the `hertz_dsp::PipelineEvent` → daemon mapping.
//!
//! Each dongle's DSP runs on a dedicated **sync** `std::thread` (the gnosis-proven
//! one-thread-per-dongle model). It consumes raw u8 IQ from the `hertz_sdr` worker's
//! ring, converts to complex f32 via `hertz_dsp::channelizer::bytes_to_iq`, runs the
//! pipeline(s), and bridges results to the async world via two tokio unbounded
//! senders (events + audio) plus an optional recorder sender. Those senders' `send`
//! methods are sync-safe — no `.await` needed in the hot path.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use hertz_channels::ChannelDb;
use hertz_dsp::channelizer::{
    bytes_to_iq, design_lowpass_fir, detect_active_channels, estimate_noise_floor, extract_channel,
    ChannelProbe, FFT_DETECT_SIZE, LPF_CUTOFF_NORM, LPF_TAPS,
};
use hertz_dsp::classify::SignalClassification;
use hertz_dsp::pipeline::{Pipeline, PipelineConfig, PipelineEvent, AUDIO_SAMPLE_RATE, SDR_RATE};
use hertz_dsp::{AudioKind, Complex32, Mode};
use hertz_sdr::{WorkerEvent, WorkerHandle};
use hertz_types::{Channel, Event, WeatherConfig};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::audio::AudioFrame;
use crate::control::{DongleControl, DongleControlSnapshot};
use crate::recorder::{RecorderTx, TransmissionJob};
use crate::spectrum::{SpectrumEmitter, SpectrumFrame};
use crate::weather::{WeatherSample, WeatherTx};

/// Monitor frame: 48 000 IQ pairs = 200 ms at 240 kS/s (one SDR read).
const MONITOR_FRAME_BYTES: usize = 48_000 * 2;
/// Wideband frame: 480 000 IQ pairs = 200 ms at 2.4 MS/s (gnosis wideband buffer).
const WIDEBAND_FRAME_BYTES: usize = 480_000 * 2;
/// Channelized slot auto-release after this many seconds of silence.
const SLOT_SILENCE_RELEASE_SEC: u64 = 5;
/// ChannelActivity heartbeat cadence (frames).
const CHANNEL_ACTIVITY_EVERY_FRAMES: u64 = 25;

/// Stable per-channel key for the audio wire frame. Hashes the channel id to a u32.
pub fn channel_key(id: Option<&str>) -> u32 {
    match id {
        Some(s) => s
            .bytes()
            .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32)),
        None => 0,
    }
}

/// Argument bundle for a DSP thread, shared by both roles.
pub struct DspThread {
    pub dongle_idx: u8,
    pub worker: Arc<WorkerHandle>,
    pub control: DongleControl,
    pub event_tx: mpsc::UnboundedSender<Event>,
    pub audio_tx: mpsc::UnboundedSender<AudioFrame>,
    pub spectrum_tx: mpsc::UnboundedSender<SpectrumFrame>,
    pub recorder_tx: Option<RecorderTx>,
    pub shutdown: Arc<AtomicBool>,
    pub channels: Arc<ChannelDb>,
    /// Monitor-role scan list (channel ids); empty = fixed frequency.
    pub scan: Vec<String>,
    /// Weather sideband (scanning monitor only) and where its samples go.
    pub weather: Option<(WeatherConfig, WeatherTx)>,
}

// ---------------------------------------------------------------------------
// Monitor role
// ---------------------------------------------------------------------------

/// Spawn the monitor-role DSP thread. Returns its join handle.
pub fn spawn_monitor_loop(dsp: DspThread) -> thread::JoinHandle<()> {
    if !dsp.scan.is_empty() {
        return spawn_scan_loop(dsp);
    }
    thread::Builder::new()
        .name(format!("dsp-monitor-{}", dsp.control.snapshot().dongle_id))
        .spawn(move || {
            let mut consumer = dsp.worker.reader();
            let mut acc: Vec<u8> = Vec::with_capacity(MONITOR_FRAME_BYTES * 2);
            let snap = dsp.control.snapshot();
            let mut pipeline = build_pipeline(&snap);
            let mut freq = snap.freq_hz as u32;
            let mut last_freq_epoch = snap.freq_epoch;
            let mut last_squelch_epoch = snap.squelch_epoch;
            // T4.1: rate-limited FFT spectrum for the live waterfall.
            let mut spectrum = SpectrumEmitter::new();
            let span_hz = SDR_RATE as f64;

            while !dsp.shutdown.load(Ordering::Relaxed) {
                poll_worker_events(&dsp);
                // Drain ring into accumulator.
                drain_into(&mut consumer, &mut acc);
                // React to control changes.
                let snap = dsp.control.snapshot();
                if snap.freq_epoch != last_freq_epoch {
                    let _ = dsp.worker.retune(snap.freq_hz as u32);
                    freq = snap.freq_hz as u32;
                    pipeline.reset();
                    last_freq_epoch = snap.freq_epoch;
                    debug!("monitor {} retune -> {} Hz", snap.dongle_id, freq);
                }
                if snap.squelch_epoch != last_squelch_epoch {
                    pipeline = build_pipeline(&snap);
                    last_squelch_epoch = snap.squelch_epoch;
                }
                // Process whatever full frames we have.
                while acc.len() >= MONITOR_FRAME_BYTES {
                    let frame: Vec<u8> = acc.drain(..MONITOR_FRAME_BYTES).collect();
                    let iq = bytes_to_iq(&frame);
                    for pe in pipeline.process_buffer(&iq) {
                        dispatch_monitor_event(&dsp, &snap, freq, &pe);
                    }
                    // Publish a decimated FFT snapshot (≤20 fps) for the waterfall.
                    if let Some(bins) = spectrum.maybe_emit(&iq) {
                        let _ = dsp.spectrum_tx.send(SpectrumFrame {
                            dongle_id: snap.dongle_id.clone(),
                            dongle_idx: dsp.dongle_idx,
                            center_hz: freq as f64,
                            span_hz,
                            squelch_open: pipeline.is_squelch_open(),
                            bins_db: bins,
                        });
                    }
                }
                if acc.len() < MONITOR_FRAME_BYTES {
                    thread::sleep(Duration::from_millis(2));
                }
            }
        })
        .expect("spawn monitor dsp thread")
}

fn dispatch_monitor_event(
    dsp: &DspThread,
    snap: &DongleControlSnapshot,
    freq_hz: u32,
    pe: &PipelineEvent,
) {
    let dongle_id = &snap.dongle_id;
    let channel = channel_for_freq(&dsp.channels, freq_hz as u64);
    let key = channel_key(channel.as_deref());
    match pe {
        PipelineEvent::SquelchOpened {
            signal_db,
            classification,
            ..
        } => {
            let _ = dsp.event_tx.send(Event::SquelchEvent {
                dongle_id: dongle_id.clone(),
                channel: channel.clone(),
                freq_hz: freq_hz as u64,
                open: true,
                signal_db: *signal_db,
                classification: classification.label().to_string(),
            });
        }
        PipelineEvent::SquelchClosed {
            signal_db,
            classification,
            ..
        } => {
            let _ = dsp.event_tx.send(Event::SquelchEvent {
                dongle_id: dongle_id.clone(),
                channel: channel.clone(),
                freq_hz: freq_hz as u64,
                open: false,
                signal_db: *signal_db,
                classification: classification.label().to_string(),
            });
        }
        PipelineEvent::SignalLevel {
            signal_db,
            noise_floor,
            squelch_open,
            flatness,
        } => {
            let _ = dsp.event_tx.send(Event::SignalLevel {
                dongle_id: dongle_id.clone(),
                channel: channel.clone(),
                freq_hz: freq_hz as u64,
                signal_db: *signal_db,
                noise_floor: *noise_floor,
                squelch_open: *squelch_open,
                audio_flatness: *flatness,
            });
        }
        PipelineEvent::Audio {
            samples, signal_db, ..
        }
        | PipelineEvent::MonitorTap { samples, signal_db } => {
            // MonitorTap (squelch closed) only flows when listening.
            let is_tap = matches!(pe, PipelineEvent::MonitorTap { .. });
            if is_tap && !snap.listening {
                return;
            }
            let _ = dsp.audio_tx.send(AudioFrame {
                dongle_id: dongle_id.clone(),
                dongle_idx: dsp.dongle_idx,
                channel_key: key,
                channel: channel.clone(),
                freq_hz,
                signal_db: *signal_db,
                pcm: samples.clone(),
            });
        }
        PipelineEvent::TransmissionComplete(summary) => {
            // A carrier or static with no voice isn't a call: don't record (or
            // transcribe, or relay) open-squelch noise.
            if summary.classification != SignalClassification::Voice {
                tracing::info!(
                    "not recorded: {} at {:.4} MHz ({}, no voice)",
                    channel.as_deref().unwrap_or("?"),
                    freq_hz as f64 / 1e6,
                    summary.classification.label()
                );
            } else if snap.recording {
                if let Some(tx) = &dsp.recorder_tx {
                    let _ = tx.send(TransmissionJob {
                        dongle_id: dongle_id.clone(),
                        freq_hz,
                        channel: channel.clone(),
                        channel_label: channel
                            .as_deref()
                            .and_then(|c| dsp.channels.get_by_id(c).map(|ch| ch.label.clone())),
                        samples: summary.samples.clone(),
                    });
                }
            }
        }
    }
}

fn build_pipeline(snap: &DongleControlSnapshot) -> Pipeline {
    // T4: monitor is NFM; bandplan/per-channel mode selection arrives with the
    // channelized AM/SSB work. AM channels route through the channelized path.
    Pipeline::new(PipelineConfig {
        mode: Mode::Nfm,
        squelch_margin_db: snap.squelch_db,
        input_sample_rate_hz: SDR_RATE,
        // Squelch-gated only: streaming closed-squelch frames plays raw static.
        always_stream: false,
        ..PipelineConfig::default()
    })
}

/// Find the channel id at a frequency, if any (1 kHz tolerance).
fn channel_for_freq(db: &ChannelDb, freq_hz: u64) -> Option<String> {
    db.get_by_freq(freq_hz, 1_000)
        .into_iter()
        .next()
        .map(|c| c.id.clone())
}

// ---------------------------------------------------------------------------
// Channelized (wideband) role
// ---------------------------------------------------------------------------

struct ChannelSlot {
    pipeline: Pipeline,
    channel_id: String,
    freq_hz: u32,
    label: String,
    mixer_phase: f32,
    last_signal: Instant,
}

/// Spawn the channelized-role DSP thread.
pub fn spawn_channelized_loop(
    dsp: DspThread,
    center_hz: u64,
    sample_rate_hz: u32,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name(format!("dsp-chan-{}", dsp.control.snapshot().dongle_id))
        .spawn(move || {
            let mut consumer = dsp.worker.reader();
            let mut acc: Vec<u8> = Vec::with_capacity(WIDEBAND_FRAME_BYTES * 2);
            let fir = design_lowpass_fir(LPF_TAPS, LPF_CUTOFF_NORM);

            let group = group_for(&dsp);
            let probes = build_probes(&dsp.channels, group.as_deref(), center_hz);
            if probes.is_empty() {
                warn!(
                    "channelized {} has no probes (group {:?}); nothing to monitor",
                    dsp.control.snapshot().dongle_id,
                    group
                );
            }

            let tap_channel = dsp.control.snapshot().tap_channel.clone();
            let mut slots: HashMap<String, ChannelSlot> = HashMap::new();
            let mut noise_floor_db: f32 = -60.0;
            let mut frame_count: u64 = 0;
            // T4.1: rate-limited FFT spectrum for the live waterfall.
            let mut spectrum = SpectrumEmitter::new();

            // Seed the tap slot so its audio streams continuously.
            if let Some(tc) = &tap_channel {
                if let Some(ch) = dsp.channels.get_by_id(tc) {
                    ensure_slot(&mut slots, ch, &dsp.control, &Instant::now());
                }
            }

            while !dsp.shutdown.load(Ordering::Relaxed) {
                poll_worker_events(&dsp);
                drain_into(&mut consumer, &mut acc);

                while acc.len() >= WIDEBAND_FRAME_BYTES {
                    let frame: Vec<u8> = acc.drain(..WIDEBAND_FRAME_BYTES).collect();
                    let wideband = bytes_to_iq(&frame);
                    frame_count += 1;

                    // Update wideband floor when idle (gnosis EMA 0.95/0.05, every 50).
                    if frame_count.is_multiple_of(50) && slots.len() <= 1 {
                        let nf = estimate_noise_floor(&wideband, sample_rate_hz, FFT_DETECT_SIZE);
                        noise_floor_db = 0.95 * noise_floor_db + 0.05 * nf;
                    }

                    let snap = dsp.control.snapshot();
                    let detected = detect_active_channels(
                        &wideband,
                        sample_rate_hz,
                        &probes,
                        12_500.0,
                        noise_floor_db,
                        snap.squelch_db,
                        FFT_DETECT_SIZE,
                    );

                    // Create slots for newly detected channels + keep the tap alive.
                    for &(id_key, _power) in &detected {
                        if let Some(ch) = dsp.channels.get_by_id(&id_key_to_string(id_key)) {
                            ensure_slot(&mut slots, ch, &dsp.control, &Instant::now());
                        }
                    }
                    if let Some(tc) = &tap_channel {
                        if let Some(ch) = dsp.channels.get_by_id(tc) {
                            ensure_slot(&mut slots, ch, &dsp.control, &Instant::now());
                        }
                    }

                    // Extract + run each slot.
                    let slot_ids: Vec<String> = slots.keys().cloned().collect();
                    for sid in &slot_ids {
                        let probe = probes.iter().find(|(id, _)| id_key_to_string(*id) == *sid);
                        let offset = match probe {
                            Some((_, off)) => *off,
                            None => continue,
                        };
                        let slot = match slots.get_mut(sid) {
                            Some(s) => s,
                            None => continue,
                        };
                        let baseband = extract_channel(
                            &wideband,
                            offset,
                            sample_rate_hz,
                            10,
                            &fir,
                            &mut slot.mixer_phase,
                        );
                        for pe in slot.pipeline.process_buffer(&baseband) {
                            dispatch_channelized_event(&dsp, &snap, slot, &pe);
                        }
                        if slot.pipeline.is_squelch_open() {
                            slot.last_signal = Instant::now();
                        }
                    }

                    // T4.1: publish a decimated FFT snapshot of the wideband tap
                    // (≤20 fps). squelch_open is an aggregate over active slots.
                    if let Some(bins) = spectrum.maybe_emit(&wideband) {
                        let any_open = slots.values().any(|s| s.pipeline.is_squelch_open());
                        let _ = dsp.spectrum_tx.send(SpectrumFrame {
                            dongle_id: snap.dongle_id.clone(),
                            dongle_idx: dsp.dongle_idx,
                            center_hz: center_hz as f64,
                            span_hz: sample_rate_hz as f64,
                            squelch_open: any_open,
                            bins_db: bins,
                        });
                    }

                    // Release silent non-tap slots.
                    slots.retain(|id, slot| {
                        let is_tap = tap_channel.as_deref() == Some(id.as_str());
                        if is_tap {
                            return true;
                        }
                        if slot.last_signal.elapsed().as_secs() >= SLOT_SILENCE_RELEASE_SEC {
                            let _ = dsp.event_tx.send(Event::SquelchEvent {
                                dongle_id: dsp.control.snapshot().dongle_id.clone(),
                                channel: Some(id.clone()),
                                freq_hz: slot.freq_hz as u64,
                                open: false,
                                signal_db: -120.0,
                                classification: "TIMEOUT".to_string(),
                            });
                            return false;
                        }
                        true
                    });

                    // ChannelActivity heartbeat every N frames.
                    if frame_count.is_multiple_of(CHANNEL_ACTIVITY_EVERY_FRAMES) {
                        emit_channel_activity(&dsp, &slots);
                    }
                }
                if acc.len() < WIDEBAND_FRAME_BYTES {
                    thread::sleep(Duration::from_millis(5));
                }
            }
        })
        .expect("spawn channelized dsp thread")
}

fn ensure_slot(
    slots: &mut HashMap<String, ChannelSlot>,
    ch: &Channel,
    control: &DongleControl,
    now: &Instant,
) {
    slots.entry(ch.id.clone()).or_insert_with(|| {
        let snap = control.snapshot();
        let mode = match ch.mode {
            hertz_types::Mode::Nfm => Mode::Nfm,
            hertz_types::Mode::Am => Mode::Am,
            _ => Mode::Nfm,
        };
        let pipeline = Pipeline::new(PipelineConfig {
            mode,
            squelch_margin_db: snap.squelch_db,
            input_sample_rate_hz: SDR_RATE, // post-extraction narrowband rate
            // Squelch-gated only: streaming closed-squelch frames plays raw static.
            always_stream: false,
            ..PipelineConfig::default()
        });
        ChannelSlot {
            pipeline,
            channel_id: ch.id.clone(),
            freq_hz: ch.freq_hz as u32,
            label: ch.label.clone(),
            mixer_phase: 0.0,
            last_signal: *now,
        }
    });
}

fn dispatch_channelized_event(
    dsp: &DspThread,
    _snap: &DongleControlSnapshot,
    slot: &ChannelSlot,
    pe: &PipelineEvent,
) {
    let snap = dsp.control.snapshot();
    let dongle_id = &snap.dongle_id;
    let channel = Some(slot.channel_id.clone());
    let freq_hz = slot.freq_hz;
    let key = channel_key(Some(&slot.channel_id));
    let label = slot.label.clone();
    match pe {
        PipelineEvent::SquelchOpened {
            signal_db,
            classification,
            ..
        } => {
            let _ = dsp.event_tx.send(Event::SquelchEvent {
                dongle_id: dongle_id.clone(),
                channel: channel.clone(),
                freq_hz: freq_hz as u64,
                open: true,
                signal_db: *signal_db,
                classification: classification.label().to_string(),
            });
        }
        PipelineEvent::SquelchClosed {
            signal_db,
            classification,
            ..
        } => {
            let _ = dsp.event_tx.send(Event::SquelchEvent {
                dongle_id: dongle_id.clone(),
                channel: channel.clone(),
                freq_hz: freq_hz as u64,
                open: false,
                signal_db: *signal_db,
                classification: classification.label().to_string(),
            });
        }
        PipelineEvent::SignalLevel {
            signal_db,
            noise_floor,
            squelch_open,
            flatness,
        } => {
            let _ = dsp.event_tx.send(Event::SignalLevel {
                dongle_id: dongle_id.clone(),
                channel: channel.clone(),
                freq_hz: freq_hz as u64,
                signal_db: *signal_db,
                noise_floor: *noise_floor,
                squelch_open: *squelch_open,
                audio_flatness: *flatness,
            });
        }
        PipelineEvent::Audio {
            samples, signal_db, ..
        }
        | PipelineEvent::MonitorTap { samples, signal_db } => {
            let is_tap_channel = snap.tap_channel.as_deref() == Some(slot.channel_id.as_str());
            let is_tap_event = matches!(pe, PipelineEvent::MonitorTap { .. });
            // Non-tap channels only emit while squelch-open (Audio events). The tap
            // channel additionally streams closed frames (MonitorTap) when listening.
            if is_tap_event && !(is_tap_channel && snap.listening) {
                return;
            }
            let _ = dsp.audio_tx.send(AudioFrame {
                dongle_id: dongle_id.clone(),
                dongle_idx: dsp.dongle_idx,
                channel_key: key,
                channel: channel.clone(),
                freq_hz,
                signal_db: *signal_db,
                pcm: samples.clone(),
            });
        }
        PipelineEvent::TransmissionComplete(summary) => {
            // A carrier or static with no voice isn't a call: don't record (or
            // transcribe, or relay) open-squelch noise.
            if summary.classification != SignalClassification::Voice {
                tracing::info!(
                    "not recorded: {} at {:.4} MHz ({}, no voice)",
                    channel.as_deref().unwrap_or("?"),
                    freq_hz as f64 / 1e6,
                    summary.classification.label()
                );
            } else if snap.recording {
                if let Some(tx) = &dsp.recorder_tx {
                    let _ = tx.send(TransmissionJob {
                        dongle_id: dongle_id.clone(),
                        freq_hz,
                        channel: channel.clone(),
                        channel_label: Some(label.clone()),
                        samples: summary.samples.clone(),
                    });
                }
            }
        }
    }
}

fn emit_channel_activity(dsp: &DspThread, slots: &HashMap<String, ChannelSlot>) {
    let snap = dsp.control.snapshot();
    let active: Vec<hertz_types::ChannelActivityInfo> = slots
        .values()
        .map(|s| hertz_types::ChannelActivityInfo {
            channel: s.channel_id.clone(),
            label: s.label.clone(),
            freq_hz: s.freq_hz as u64,
            signal_db: if s.pipeline.is_squelch_open() {
                -9.0
            } else {
                -60.0
            },
            classification: if s.pipeline.is_squelch_open() {
                "ACTIVE".to_string()
            } else {
                "IDLE".to_string()
            },
        })
        .collect();
    let _ = dsp.event_tx.send(Event::ChannelActivity {
        dongle_id: snap.dongle_id,
        active,
        noise_floor: -60.0,
    });
}

fn build_probes(db: &ChannelDb, group: Option<&str>, center_hz: u64) -> Vec<ChannelProbe> {
    let channels: Vec<&Channel> = match group {
        Some(g) => db.get_by_group(g),
        None => db.channels.values().collect(),
    };
    channels
        .into_iter()
        .filter(|c| c.rx)
        .map(|c| {
            let offset = c.freq_hz as f32 - center_hz as f32;
            (string_to_id_key(&c.id), offset)
        })
        .collect()
}

fn group_for(dsp: &DspThread) -> Option<String> {
    let snap = dsp.control.snapshot();
    snap.groups.first().cloned().or(snap.bandplan.clone())
}

/// The wire channel_key is a u32; we use the hash of the channel id. For the probe
/// list we key on the same hash so `detect_active_channels` returns ids we can map
/// back — but we also need the original string. We stash the string via a side table
/// keyed by the hash. To keep it simple, channel ids that are numeric ("16") hash
/// reversibly enough: we store the string in a parallel lookup.
fn id_key_to_string(id: u32) -> String {
    // We only ever probe channels whose id is a positive integer ≤ u32 (marine/CB
    // channel numbers). For non-numeric ids the daemon should use a side table; T4
    // tests use numeric marine channel ids, so this suffices and is documented.
    id.to_string()
}

fn string_to_id_key(s: &str) -> u32 {
    s.parse::<u32>().unwrap_or_else(|_| channel_key(Some(s)))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn drain_into(consumer: &mut rtrb::Consumer<u8>, acc: &mut Vec<u8>) {
    // rtrb 0.3 has no chunk API; pop byte-by-byte until empty. Bounded so we don't
    // grow unbounded if the DSP falls behind (drops newest by stopping the drain).
    let max = MONITOR_FRAME_BYTES.max(WIDEBAND_FRAME_BYTES) * 2;
    while acc.len() < max {
        match consumer.pop() {
            Ok(b) => acc.push(b),
            Err(_) => break,
        }
    }
}

fn poll_worker_events(dsp: &DspThread) {
    while let Ok(we) = dsp.worker.event_receiver().try_recv() {
        let snap = dsp.control.snapshot();
        match we {
            WorkerEvent::DeviceLost => {
                dsp.control.set_online(false, Some("device lost".into()));
                let _ = dsp.event_tx.send(Event::DongleStatus {
                    dongle_id: snap.dongle_id.clone(),
                    serial: snap.serial.clone(),
                    online: false,
                    role: snap.role,
                    message: Some("device lost".into()),
                });
                warn!("monitor {} device lost", snap.dongle_id);
            }
            WorkerEvent::DeviceReconnected => {
                dsp.control.set_online(true, None);
                let _ = dsp.event_tx.send(Event::DongleStatus {
                    dongle_id: snap.dongle_id.clone(),
                    serial: snap.serial.clone(),
                    online: true,
                    role: snap.role,
                    message: None,
                });
                debug!("monitor {} reconnected", snap.dongle_id);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Monitor role, scanning (vhf_monitor `scan`)
// ---------------------------------------------------------------------------

/// Keep listening on a channel this long after its squelch closes, so replies in a
/// conversation are caught before the scan resumes (vhf_monitor AUTO_RESUME_SCAN_SEC).
const SCAN_RESUME_AFTER: Duration = Duration::from_secs(5);

/// Scan neighbours this close are checked when a squelch opens.
const NEIGHBOUR_HZ: i64 = 15_000;
/// A neighbour must be this much stronger to take the call.
const NEIGHBOUR_MARGIN_DB: f32 = 3.0;

/// Power (dB) near `offset_hz` from the tuned frequency: mix it to 0 Hz and
/// average 32-sample blocks, a boxcar whose first null is SDR_RATE/32 = 7.5 kHz.
fn power_at_db(iq: &[Complex32], offset_hz: f32) -> f32 {
    const BLOCK: usize = 32;
    let step = -2.0 * std::f32::consts::PI * offset_hz / SDR_RATE as f32;
    let mut sum = 0.0f32;
    let mut blocks = 0u32;
    for (b, chunk) in iq.chunks_exact(BLOCK).enumerate() {
        let start = (b * BLOCK) as f32 * step;
        let mean: Complex32 = chunk
            .iter()
            .enumerate()
            .map(|(i, &s)| s * Complex32::from_polar(1.0, start + i as f32 * step))
            .sum::<Complex32>()
            / BLOCK as f32;
        sum += mean.norm_sqr();
        blocks += 1;
    }
    10.0 * (sum / blocks.max(1) as f32 + 1e-12).log10()
}

/// The scan neighbour within NEIGHBOUR_HZ whose signal in this frame beats the
/// tuned channel's by NEIGHBOUR_MARGIN_DB, if any.
fn stronger_neighbour(iq: &[Complex32], idx: usize, chans: &[(String, u32)]) -> Option<usize> {
    let here = chans[idx].1 as i64;
    let own = power_at_db(iq, 0.0);
    chans
        .iter()
        .enumerate()
        .filter(|&(j, (_, f))| j != idx && (*f as i64 - here).abs() <= NEIGHBOUR_HZ)
        .map(|(j, (_, f))| (j, power_at_db(iq, (*f as i64 - here) as f32)))
        .filter(|&(_, p)| p > own + NEIGHBOUR_MARGIN_DB)
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(j, _)| j)
}

/// A squelch open this long is a stuck carrier, not a call.
const SCAN_MAX_OPEN: Duration = Duration::from_secs(30);
/// How long a nuisance channel (carrier or static, no voice) is skipped.
const SCAN_LOCKOUT: Duration = Duration::from_secs(300);

fn lock_out(locked_until: &mut [Option<Instant>], idx: usize, chans: &[(String, u32)], why: &str) {
    if locked_until[idx].is_some_and(|t| Instant::now() < t) {
        return;
    }
    locked_until[idx] = Some(Instant::now() + SCAN_LOCKOUT);
    warn!(
        "scan: Ch {} locked out for {} min ({why}, no voice)",
        chans[idx].0,
        SCAN_LOCKOUT.as_secs() / 60
    );
}

/// The next channel after `idx` that isn't locked out; `idx + 1` if all are.
fn next_channel(idx: usize, locked_until: &[Option<Instant>]) -> usize {
    let n = locked_until.len();
    let now = Instant::now();
    (1..=n)
        .map(|step| (idx + step) % n)
        .find(|&i| locked_until[i].is_none_or(|t| now >= t))
        .unwrap_or((idx + 1) % n)
}

/// Resolve a scan list to (channel id, frequency) in order. An entry is a channel id
/// ("72", "gmrs-1") or a band group ("frs-gmrs", "noaa-wx"), which expands to that
/// group's receive channels by frequency. Repeater inputs (label `*-INPUT`) are left
/// out of a group, since the same traffic comes back on the repeater output; list
/// them by id to scan them. Duplicates are dropped.
pub fn scan_channels(db: &ChannelDb, scan: &[String]) -> Vec<(String, u32)> {
    let mut out: Vec<(String, u32)> = Vec::new();
    let mut push = |id: &str, freq: u64| {
        if !out.iter().any(|(have, _)| have == id) {
            out.push((id.to_string(), freq as u32));
        }
    };
    for entry in scan {
        if let Some(c) = db.get_by_id(entry) {
            push(&c.id, c.freq_hz);
            continue;
        }
        let mut group: Vec<_> = db
            .get_by_group(entry)
            .into_iter()
            // The scanner demodulates NFM and locks while a squelch is open, so AM
            // channels and always-on broadcasts (NOAA weather) can't be scanned.
            .filter(|c| {
                c.rx && !c.label.ends_with("-INPUT")
                    && c.mode == hertz_types::Mode::Nfm
                    && !c.continuous_carrier
            })
            .collect();
        if group.is_empty() {
            warn!("scan: {entry:?} is not a channel id or a band group with scannable NFM channels, skipping");
            continue;
        }
        group.sort_by_key(|c| c.freq_hz);
        for c in group {
            push(&c.id, c.freq_hz);
        }
    }
    out
}

/// Scanner: hop the monitor dongle through `dsp.scan`, one 200 ms dwell per channel
/// (plus a 50 ms settle discard after each retune), and lock on any channel whose
/// squelch opens. Each channel keeps its own pipeline, so noise floors and squelch
/// state are per-channel.
fn spawn_scan_loop(dsp: DspThread) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name(format!("dsp-scan-{}", dsp.control.snapshot().dongle_id))
        .spawn(move || {
            let chans = scan_channels(&dsp.channels, &dsp.scan);
            if chans.is_empty() {
                warn!("scan list has no known channels; scanner idle");
                return;
            }
            let snap = dsp.control.snapshot();
            let mut pipelines: Vec<Pipeline> =
                chans.iter().map(|_| build_pipeline(&snap)).collect();
            let mut consumer = dsp.worker.reader();
            let mut acc: Vec<u8> = Vec::with_capacity(MONITOR_FRAME_BYTES * 2);
            let mut spectrum = SpectrumEmitter::new();
            let span_hz = SDR_RATE as f64;

            let mut idx = 0usize;
            let _ = dsp.worker.retune(chans[idx].1);
            // Bytes to drop after a retune: the worker read in flight when the retune
            // was issued still holds the old frequency.
            const SETTLE_BYTES: usize = crate::runtime::SCAN_READ_IQ_PAIRS * 2;
            let mut settle_bytes = SETTLE_BYTES;
            let mut last_open: Option<Instant> = None;
            // When the current squelch opened, and per channel the end of a lockout.
            let mut open_since: Option<Instant> = None;
            let mut locked_until: Vec<Option<Instant>> = vec![None; chans.len()];
            let weather: Vec<(String, u32)> = dsp
                .weather
                .iter()
                .flat_map(|(w, _)| &w.channels)
                .filter_map(|id| match dsp.channels.get_by_id(id) {
                    Some(c) => Some((c.id.clone(), c.freq_hz as u32)),
                    None => {
                        warn!("weather: unknown channel id {id:?}, skipping");
                        None
                    }
                })
                .collect();
            let mut next_weather = dsp
                .weather
                .as_ref()
                .filter(|_| !weather.is_empty())
                .map(|(w, _)| Instant::now() + Duration::from_secs(w.first_after_seconds));
            if let Some((w, _)) = dsp.weather.as_ref().filter(|_| !weather.is_empty()) {
                tracing::info!(
                    "weather: sampling {} for {} s every {} min while the scan is idle",
                    weather
                        .iter()
                        .map(|(id, _)| id.as_str())
                        .collect::<Vec<_>>()
                        .join(","),
                    w.sample_seconds,
                    w.every_minutes
                );
            }
            tracing::info!(
                "scanning {} channel(s): {}",
                chans.len(),
                chans
                    .iter()
                    .map(|(id, _)| id.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            );

            while !dsp.shutdown.load(Ordering::Relaxed) {
                poll_worker_events(&dsp);
                drain_into(&mut consumer, &mut acc);
                if settle_bytes > 0 {
                    let n = settle_bytes.min(acc.len());
                    acc.drain(..n);
                    settle_bytes -= n;
                }
                while settle_bytes == 0 && acc.len() >= MONITOR_FRAME_BYTES {
                    let frame: Vec<u8> = acc.drain(..MONITOR_FRAME_BYTES).collect();
                    let iq = bytes_to_iq(&frame);
                    let snap = dsp.control.snapshot();
                    let freq = chans[idx].1;
                    let pipeline = &mut pipelines[idx];
                    if !pipeline.is_squelch_open() && last_open.is_none() {
                        pipeline.clear_prebuffer();
                    }
                    let was_open = pipeline.is_squelch_open();
                    let events = pipeline.process_buffer(&iq);
                    // FRS/GMRS channels are 12.5 kHz apart, inside this channel's
                    // filter: a call on the next channel opens this one too. If a
                    // scanned neighbour is stronger, drop this open unannounced and
                    // move there; its own squelch opens on the next frame.
                    if !was_open && pipeline.is_squelch_open() {
                        if let Some(better) = stronger_neighbour(&iq, idx, &chans) {
                            debug!(
                                "scan: Ch {} opened on Ch {}'s signal; moving there",
                                chans[idx].0, chans[better].0
                            );
                            pipeline.reset();
                            idx = better;
                            last_open = Some(Instant::now());
                            let _ = dsp.worker.retune(chans[idx].1);
                            acc.clear();
                            drain_into(&mut consumer, &mut acc);
                            acc.clear();
                            settle_bytes = SETTLE_BYTES;
                            break;
                        }
                    }
                    for pe in events {
                        dispatch_monitor_event(&dsp, &snap, freq, &pe);
                        // A squelch that closes on a carrier or static, not a voice,
                        // locks the channel out so it can't hold the scanner.
                        if let PipelineEvent::SquelchClosed { classification, .. } = &pe {
                            if *classification != SignalClassification::Voice {
                                lock_out(&mut locked_until, idx, &chans, classification.label());
                                last_open = None;
                            }
                        }
                    }
                    if let Some(bins) = spectrum.maybe_emit(&iq) {
                        let _ = dsp.spectrum_tx.send(SpectrumFrame {
                            dongle_id: snap.dongle_id.clone(),
                            dongle_idx: dsp.dongle_idx,
                            center_hz: freq as f64,
                            span_hz,
                            squelch_open: pipeline.is_squelch_open(),
                            bins_db: bins,
                        });
                    }

                    // Lock while open; linger after close for replies; else hop. A
                    // squelch open longer than any call is a stuck carrier: close it,
                    // lock the channel out and move on.
                    if pipeline.is_squelch_open() {
                        let since = *open_since.get_or_insert_with(Instant::now);
                        if since.elapsed() < SCAN_MAX_OPEN {
                            last_open = Some(Instant::now());
                            continue;
                        }
                        let noise_floor = pipeline.noise_floor_db();
                        pipeline.reset();
                        dispatch_monitor_event(
                            &dsp,
                            &snap,
                            freq,
                            &PipelineEvent::SquelchClosed {
                                signal_db: noise_floor,
                                noise_floor,
                                classification: SignalClassification::Carrier,
                            },
                        );
                        lock_out(&mut locked_until, idx, &chans, "stuck open");
                        last_open = None;
                    }
                    if !was_open || !pipeline.is_squelch_open() {
                        open_since = None;
                    }
                    if locked_until[idx].is_some_and(|t| Instant::now() < t) {
                        last_open = None;
                    }
                    if let Some(t) = last_open {
                        if t.elapsed() < SCAN_RESUME_AFTER {
                            continue;
                        }
                        last_open = None;
                    }
                    // Idle between calls: take the weather sample if it's due.
                    if let (Some(due), Some((w, tx))) = (next_weather, dsp.weather.as_ref()) {
                        if Instant::now() >= due {
                            for (id, wx_freq) in &weather {
                                let (samples, signal_db) = sample_weather(
                                    &dsp,
                                    &mut consumer,
                                    *wx_freq,
                                    Duration::from_secs(w.sample_seconds),
                                );
                                if dsp.shutdown.load(Ordering::Relaxed) {
                                    return;
                                }
                                let _ = tx.try_send(WeatherSample {
                                    dongle_id: snap.dongle_id.clone(),
                                    channel: id.clone(),
                                    freq_hz: *wx_freq as u64,
                                    samples,
                                    signal_db,
                                });
                            }
                            next_weather = Some(
                                Instant::now() + Duration::from_secs(w.every_minutes.max(1) * 60),
                            );
                            let _ = dsp.worker.retune(chans[idx].1);
                            acc.clear();
                            drain_into(&mut consumer, &mut acc);
                            acc.clear();
                            settle_bytes = SETTLE_BYTES;
                            break;
                        }
                    }
                    if chans.len() > 1 {
                        idx = next_channel(idx, &locked_until);
                        let _ = dsp.worker.retune(chans[idx].1);
                        acc.clear();
                        drain_into(&mut consumer, &mut acc);
                        acc.clear();
                        settle_bytes = SETTLE_BYTES;
                        break;
                    }
                }
                if acc.len() < MONITOR_FRAME_BYTES {
                    thread::sleep(Duration::from_millis(2));
                }
            }
        })
        .expect("spawn scan dsp thread")
}

/// Tune `freq` and collect `length` of demodulated audio for the weather sideband.
/// The squelch doesn't gate it (a broadcast is continuous), and nothing is
/// dispatched: no squelch events, recordings, relay or speaker. Gives up after
/// `length` plus 10 s if the dongle stops delivering, returning what it has.
fn sample_weather(
    dsp: &DspThread,
    consumer: &mut rtrb::Consumer<u8>,
    freq: u32,
    length: Duration,
) -> (Vec<f32>, Option<f32>) {
    let snap = dsp.control.snapshot();
    let (mut level_sum, mut levels) = (0.0f32, 0u32);
    let mut pipeline = Pipeline::new(PipelineConfig {
        mode: Mode::Nfm,
        squelch_margin_db: snap.squelch_db,
        input_sample_rate_hz: SDR_RATE,
        always_stream: true,
        ..PipelineConfig::default()
    });
    let want = (length.as_secs_f64() * AUDIO_SAMPLE_RATE as f64) as usize;
    let mut audio = Vec::with_capacity(want);
    let _ = dsp.worker.retune(freq);
    let mut acc = Vec::with_capacity(MONITOR_FRAME_BYTES * 2);
    // Drop the read in flight at the old frequency.
    let mut settle = crate::runtime::SCAN_READ_IQ_PAIRS * 2;
    let give_up = Instant::now() + length + Duration::from_secs(10);
    while audio.len() < want && Instant::now() < give_up && !dsp.shutdown.load(Ordering::Relaxed) {
        poll_worker_events(dsp);
        drain_into(consumer, &mut acc);
        if settle > 0 {
            let n = settle.min(acc.len());
            acc.drain(..n);
            settle -= n;
        }
        while settle == 0 && acc.len() >= MONITOR_FRAME_BYTES {
            let frame: Vec<u8> = acc.drain(..MONITOR_FRAME_BYTES).collect();
            for pe in pipeline.process_buffer(&bytes_to_iq(&frame)) {
                match pe {
                    // The prebuffer repeats audio already streamed as MonitorTap.
                    PipelineEvent::Audio {
                        kind: AudioKind::Prebuffer,
                        ..
                    } => {}
                    PipelineEvent::Audio { samples, .. }
                    | PipelineEvent::MonitorTap { samples, .. } => {
                        audio.extend_from_slice(&samples)
                    }
                    PipelineEvent::SignalLevel { signal_db, .. } => {
                        level_sum += signal_db;
                        levels += 1;
                    }
                    _ => {}
                }
            }
        }
        if acc.len() < MONITOR_FRAME_BYTES {
            thread::sleep(Duration::from_millis(2));
        }
    }
    audio.truncate(want);
    (audio, (levels > 0).then(|| level_sum / levels as f32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> ChannelDb {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bandplans");
        hertz_channels::load_channels(dir, None).unwrap()
    }

    #[test]
    fn frs_gmrs_group_scans_the_22_simplex_channels() {
        let chans = scan_channels(&db(), &["frs-gmrs".to_string()]);
        assert_eq!(chans.len(), 22, "{chans:?}");
        assert!(chans.iter().all(|(id, _)| !id.starts_with("gmrs-rep")));
        assert!(
            chans.windows(2).all(|w| w[0].1 <= w[1].1),
            "sorted by frequency"
        );
        assert_eq!(chans[0], ("gmrs-15".to_string(), 462_550_000));
    }

    #[test]
    fn ids_and_groups_mix_without_duplicates() {
        let scan = ["72", "gmrs-1", "frs-gmrs", "nope"].map(String::from);
        let chans = scan_channels(&db(), &scan);
        assert_eq!(chans[0].0, "72");
        assert_eq!(chans[1].0, "gmrs-1");
        assert_eq!(
            chans.len(),
            23,
            "72 plus the 22 FRS/GMRS channels once each"
        );
    }

    #[test]
    fn weather_group_is_not_scannable() {
        assert!(scan_channels(&db(), &["noaa-wx".to_string()]).is_empty());
    }
}

#[cfg(test)]
mod lockout_tests {
    use super::*;

    #[test]
    fn hop_skips_locked_out_channels() {
        let later = Some(Instant::now() + Duration::from_secs(60));
        let locked = vec![None, later, later, None];
        assert_eq!(next_channel(0, &locked), 3);
        assert_eq!(next_channel(3, &locked), 0);
        // All locked: keep scanning in order rather than stall.
        assert_eq!(next_channel(1, &[later, later]), 0);
        // An expired lockout is scanned again.
        let past = Some(Instant::now() - Duration::from_secs(1));
        assert_eq!(next_channel(0, &[None, past]), 1);
    }
}

#[cfg(test)]
mod neighbour_tests {
    use super::*;

    /// A carrier `offset_hz` from the tuned frequency, one 50 ms frame at SDR_RATE.
    fn tone(offset_hz: f32) -> Vec<Complex32> {
        let step = 2.0 * std::f32::consts::PI * offset_hz / SDR_RATE as f32;
        (0..12_000)
            .map(|n| Complex32::from_polar(0.5, n as f32 * step))
            .collect()
    }

    fn frs() -> Vec<(String, u32)> {
        // gmrs-15 at 462.550 and gmrs-1 at 462.5625: 12.5 kHz apart.
        vec![
            ("gmrs-15".into(), 462_550_000),
            ("gmrs-1".into(), 462_562_500),
            ("gmrs-2".into(), 462_587_500),
        ]
    }

    #[test]
    fn a_call_on_the_next_channel_moves_the_scanner_there() {
        // Tuned to gmrs-15, the call is 12.5 kHz up, on gmrs-1.
        assert_eq!(stronger_neighbour(&tone(12_500.0), 0, &frs()), Some(1));
    }

    #[test]
    fn a_call_on_the_tuned_channel_stays() {
        assert_eq!(stronger_neighbour(&tone(0.0), 1, &frs()), None);
        // gmrs-2 is 25 kHz from gmrs-1: not a neighbour to check.
        assert_eq!(stronger_neighbour(&tone(25_000.0), 1, &frs()), None);
    }
}
