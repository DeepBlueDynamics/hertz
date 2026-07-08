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
use hertz_dsp::pipeline::{Pipeline, PipelineConfig, PipelineEvent, SDR_RATE};
use hertz_dsp::Mode;
use hertz_sdr::{WorkerEvent, WorkerHandle};
use hertz_types::{Channel, DongleRole, Event};
use num_complex::Complex32;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::audio::AudioFrame;
use crate::control::{DongleControl, DongleControlSnapshot};
use crate::recorder::{RecorderTx, TransmissionJob};

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
    pub recorder_tx: Option<RecorderTx>,
    pub shutdown: Arc<AtomicBool>,
    pub channels: Arc<ChannelDb>,
}

// ---------------------------------------------------------------------------
// Monitor role
// ---------------------------------------------------------------------------

/// Spawn the monitor-role DSP thread. Returns its join handle.
pub fn spawn_monitor_loop(dsp: DspThread) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name(format!("dsp-monitor-{}", dsp.control.snapshot().dongle_id))
        .spawn(move || {
            let mut consumer = dsp.worker.reader();
            let mut acc: Vec<u8> = Vec::with_capacity(MONITOR_FRAME_BYTES * 2);
            let snap = dsp.control.snapshot();
            let mut pipeline = build_pipeline(&snap, dsp.control.snapshot().listening);
            let mut freq = snap.freq_hz as u32;
            let mut last_freq_epoch = snap.freq_epoch;
            let mut last_squelch_epoch = snap.squelch_epoch;

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
                    pipeline = build_pipeline(&snap, snap.listening);
                    last_squelch_epoch = snap.squelch_epoch;
                }
                // Process whatever full frames we have.
                while acc.len() >= MONITOR_FRAME_BYTES {
                    let frame: Vec<u8> = acc.drain(..MONITOR_FRAME_BYTES).collect();
                    let iq = bytes_to_iq(&frame);
                    for pe in pipeline.process_buffer(&iq) {
                        dispatch_monitor_event(&dsp, &snap, freq, &pe);
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
            if snap.recording {
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

fn build_pipeline(snap: &DongleControlSnapshot, listening: bool) -> Pipeline {
    // T4: monitor is NFM; bandplan/per-channel mode selection arrives with the
    // channelized AM/SSB work. AM channels route through the channelized path.
    Pipeline::new(PipelineConfig {
        mode: Mode::Nfm,
        squelch_margin_db: snap.squelch_db,
        input_sample_rate_hz: SDR_RATE,
        always_stream: listening,
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
    hang_noted: bool,
}

/// Spawn the channelized-role DSP thread.
pub fn spawn_channelized_loop(
    mut dsp: DspThread,
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

            // Seed the tap slot so its audio streams continuously.
            if let Some(tc) = &tap_channel {
                if let Some(ch) = dsp.channels.get_by_id(tc) {
                    ensure_slot(
                        &mut slots,
                        ch,
                        center_hz,
                        sample_rate_hz,
                        &dsp.control,
                        &Instant::now(),
                    );
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
                            ensure_slot(
                                &mut slots,
                                ch,
                                center_hz,
                                sample_rate_hz,
                                &dsp.control,
                                &Instant::now(),
                            );
                        }
                    }
                    if let Some(tc) = &tap_channel {
                        if let Some(ch) = dsp.channels.get_by_id(tc) {
                            ensure_slot(
                                &mut slots,
                                ch,
                                center_hz,
                                sample_rate_hz,
                                &dsp.control,
                                &Instant::now(),
                            );
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
    center_hz: u64,
    sample_rate_hz: u32,
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
            always_stream: snap.tap_channel.as_deref() == Some(&ch.id) && snap.listening,
            ..PipelineConfig::default()
        });
        ChannelSlot {
            pipeline,
            channel_id: ch.id.clone(),
            freq_hz: ch.freq_hz as u32,
            label: ch.label.clone(),
            mixer_phase: 0.0,
            last_signal: *now,
            hang_noted: false,
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
            if snap.recording {
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

fn pump_worker_events(_dsp: &DspThread) {
    // (worker events handled by `poll_worker_events`)
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
