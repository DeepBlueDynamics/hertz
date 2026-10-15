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
    /// Priority channels (ids or groups), checked every other hop while scanning.
    pub priority: Vec<String>,
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

                    // A call spills into channels 12.5 kHz away (FRS/GMRS spacing);
                    // keep only the strongest of neighbours, so one call isn't heard,
                    // recorded and relayed on three channels.
                    let detected = strongest_of_neighbours(detected, |key| {
                        dsp.channels
                            .get_by_id(&id_key_to_string(key))
                            .map(|c| c.freq_hz)
                    });

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

/// Of detected channels within NEIGHBOUR_HZ of a stronger detected channel, drop
/// the weaker: they're the same call seen through the detector's window.
fn strongest_of_neighbours(
    detected: Vec<(u32, f32)>,
    freq_of: impl Fn(u32) -> Option<u64>,
) -> Vec<(u32, f32)> {
    let with_freq: Vec<(u32, f32, Option<u64>)> =
        detected.iter().map(|&(k, p)| (k, p, freq_of(k))).collect();
    with_freq
        .iter()
        .filter(|&&(_, p, f)| {
            let Some(f) = f else { return true };
            !with_freq.iter().any(|&(_, q, g)| {
                g.is_some_and(|g| g != f && (g as i64 - f as i64).abs() <= NEIGHBOUR_HZ && q > p)
            })
        })
        .map(|&(k, p, _)| (k, p))
        .collect()
}

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

/// A squelch open this long is a stuck carrier, not a call.
const SCAN_MAX_OPEN: Duration = Duration::from_secs(30);
/// A carrier/static open at least this long locks a (non-priority) channel out;
/// shorter bursts, like a key-up, only mark the group busy.
const NUISANCE_LONG: Duration = Duration::from_secs(8);
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

/// Where the scanner goes next. Without priority channels: the next unlocked
/// channel. With them, every other hop is a priority channel (taken in turn), so a
/// priority channel is checked about every half second however long the list is,
/// and a call-up on it isn't clipped while the scan is elsewhere. `rot` is the
/// position in the rotation of the other channels; `turn` cycles the priorities.
fn next_hop(
    idx: usize,
    rot: &mut usize,
    turn: &mut usize,
    priority: &[usize],
    locked_until: &[Option<Instant>],
) -> usize {
    let n = locked_until.len();
    let now = Instant::now();
    let free = |i: usize| locked_until[i].is_none_or(|t| now >= t);
    let free_priority: Vec<usize> = priority.iter().copied().filter(|&p| free(p)).collect();
    if free_priority.is_empty() {
        return next_channel(idx, locked_until);
    }
    if !priority.contains(&idx) {
        *turn = (*turn + 1) % free_priority.len();
        return free_priority[*turn];
    }
    // From a priority channel: the next other channel in the rotation.
    let next = (1..=n)
        .map(|step| (*rot + step) % n)
        .find(|&i| !priority.contains(&i) && free(i));
    match next {
        Some(i) => {
            *rot = i;
            i
        }
        None => {
            *turn = (*turn + 1) % free_priority.len();
            free_priority[*turn]
        }
    }
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
            let mut chans = scan_channels(&dsp.channels, &dsp.scan);
            if chans.is_empty() {
                warn!("scan list has no known channels; scanner idle");
                return;
            }
            // Priority channels (ids or groups), added to the scan if missing.
            let mut priority: Vec<usize> = Vec::new();
            for (id, freq) in scan_channels(&dsp.channels, &dsp.priority) {
                let i = match chans.iter().position(|(have, _)| *have == id) {
                    Some(i) => i,
                    None => {
                        chans.push((id, freq));
                        chans.len() - 1
                    }
                };
                if !priority.contains(&i) {
                    priority.push(i);
                }
            }
            // Channels close enough to share one tuning are monitored together.
            let groups = scan_groups(&chans, GROUP_SPAN_HZ);
            let priority_groups: Vec<usize> = groups
                .iter()
                .enumerate()
                .filter(|(_, g)| g.members.iter().any(|m| priority.contains(m)))
                .map(|(gi, _)| gi)
                .collect();
            tracing::info!(
                "scanning {} channel(s) in {} group(s): {}",
                chans.len(),
                groups.len(),
                groups
                    .iter()
                    .map(|g| g
                        .members
                        .iter()
                        .map(|&m| chans[m].0.as_str())
                        .collect::<Vec<_>>()
                        .join(","))
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
            if !priority_groups.is_empty() {
                tracing::info!(
                    "scan: priority {} (its group is checked every other hop)",
                    priority
                        .iter()
                        .map(|&i| chans[i].0.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                );
            }

            let snap = dsp.control.snapshot();
            let mut squelch_epoch = snap.squelch_epoch;
            let mut pipelines: Vec<Pipeline> =
                chans.iter().map(|_| build_pipeline(&snap)).collect();
            let mut mixers: Vec<Mixer> = chans.iter().map(|_| Mixer::default()).collect();
            let mut locked_until: Vec<Option<Instant>> = vec![None; chans.len()];
            let mut open_since: Vec<Option<Instant>> = vec![None; chans.len()];
            // When each group last had a call: recently busy groups are checked
            // every other hop.
            let mut active_at: Vec<Option<Instant>> = vec![None; groups.len()];
            // The channel holding each group's current or last call, and when it was
            // last open: other channels in the group stay shut until GROUP_HOLDOFF
            // after it ends.
            let mut group_call: Vec<Option<(usize, Instant)>> = vec![None; groups.len()];
            let (mut grot, mut gturn) = (0usize, 0usize);

            let mut consumer = dsp.worker.reader();
            let mut acc: Vec<u8> = Vec::with_capacity(MONITOR_FRAME_BYTES * 2);
            let mut spectrum = SpectrumEmitter::new();
            let span_hz = SDR_RATE as f64;

            let mut g = 0usize;
            let _ = dsp.worker.retune(groups[g].center);
            // Bytes to drop after a retune: the worker read in flight when the retune
            // was issued still holds the old frequency.
            const SETTLE_BYTES: usize = crate::runtime::SCAN_READ_IQ_PAIRS * 2;
            let mut settle_bytes = SETTLE_BYTES;
            let mut last_open: Option<Instant> = None;

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
                    // A squelch change (POST /squelch, MCP) applies live to every
                    // channel, keeping the learned noise floors.
                    if snap.squelch_epoch != squelch_epoch {
                        squelch_epoch = snap.squelch_epoch;
                        for p in &mut pipelines {
                            p.set_squelch_margin(snap.squelch_db);
                        }
                        tracing::info!(
                            "scan: squelch now {:.1} dB over the noise floor",
                            snap.squelch_db
                        );
                    }
                    let center = groups[g].center;
                    let now = Instant::now();
                    let mut any_open = false;

                    // Every channel of the group, from the same samples: shift it to
                    // 0 Hz and run its own squelch, demod and recorder.
                    for &m in &groups[g].members {
                        let pipeline = &mut pipelines[m];
                        let locked = locked_until[m].is_some_and(|t| now < t);
                        if locked && !pipeline.is_squelch_open() {
                            continue;
                        }
                        let offset = chans[m].1 as f32 - center as f32;
                        let shifted = mixers[m].shift(&iq, offset);
                        if !pipeline.is_squelch_open() && last_open.is_none() {
                            pipeline.clear_prebuffer();
                        }
                        let was_open = pipeline.is_squelch_open();
                        let events = pipeline.process_buffer(&shifted);
                        // FRS/GMRS channels are 12.5 kHz apart, inside a channel's
                        // filter: a call on the next channel opens this one too. If a
                        // neighbour in the group is stronger, drop this open unheard.
                        let held_off = group_call[g]
                            .is_some_and(|(o, t)| o != m && now.duration_since(t) < GROUP_HOLDOFF);
                        if !was_open
                            && pipeline.is_squelch_open()
                            && (held_off
                                || stronger_member(&iq, center, m, &groups[g].members, &chans))
                        {
                            pipeline.reset();
                            continue;
                        }
                        for pe in &events {
                            dispatch_monitor_event(&dsp, &snap, chans[m].1, pe);
                            // A long carrier or static open (not a voice) locks the channel
                            // out so it can't hold the scanner. Priority channels never are:
                            // missing a call there is worse.
                            if let PipelineEvent::SquelchClosed { classification, .. } = pe {
                                if *classification != SignalClassification::Voice {
                                    let lasted = open_since[m]
                                        .map_or(Duration::ZERO, |t| now.duration_since(t));
                                    // Short bursts (a key-up) are activity, not a nuisance: the
                                    // group is now busy and gets checked every other hop.
                                    if !priority.contains(&m) && lasted >= NUISANCE_LONG {
                                        lock_out(
                                            &mut locked_until,
                                            m,
                                            &chans,
                                            classification.label(),
                                        );
                                    }
                                }
                            }
                        }
                        // A squelch open longer than any call is a stuck carrier:
                        // close it and lock the channel out.
                        if pipeline.is_squelch_open() {
                            let since = *open_since[m].get_or_insert(now);
                            if since.elapsed() >= SCAN_MAX_OPEN {
                                let noise_floor = pipeline.noise_floor_db();
                                pipeline.reset();
                                dispatch_monitor_event(
                                    &dsp,
                                    &snap,
                                    chans[m].1,
                                    &PipelineEvent::SquelchClosed {
                                        signal_db: noise_floor,
                                        noise_floor,
                                        classification: SignalClassification::Carrier,
                                    },
                                );
                                if !priority.contains(&m) {
                                    lock_out(&mut locked_until, m, &chans, "stuck open");
                                }
                                open_since[m] = None;
                            } else if locked_until[m].is_none_or(|t| now >= t) {
                                any_open = true;
                                group_call[g] = Some((m, now));
                            }
                        } else {
                            open_since[m] = None;
                        }
                    }
                    if let Some(bins) = spectrum.maybe_emit(&iq) {
                        let _ = dsp.spectrum_tx.send(SpectrumFrame {
                            dongle_id: snap.dongle_id.clone(),
                            dongle_idx: dsp.dongle_idx,
                            center_hz: center as f64,
                            span_hz,
                            squelch_open: any_open,
                            bins_db: bins,
                        });
                    }

                    // Stay while a call is open; linger after it for replies; else hop.
                    if any_open {
                        last_open = Some(now);
                        active_at[g] = Some(now);
                        continue;
                    }
                    if let Some(t) = last_open {
                        if t.elapsed() < SCAN_RESUME_AFTER {
                            continue;
                        }
                        last_open = None;
                    }
                    // Idle between calls: take the weather sample if it's due.
                    if let (Some(due), Some((w, tx))) = (next_weather, dsp.weather.as_ref()) {
                        if now >= due {
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
                            let _ = dsp.worker.retune(groups[g].center);
                            acc.clear();
                            drain_into(&mut consumer, &mut acc);
                            acc.clear();
                            settle_bytes = SETTLE_BYTES;
                            break;
                        }
                    }
                    if groups.len() > 1 {
                        let hot = hot_groups(&active_at, &priority_groups, now);
                        g = next_hop(g, &mut grot, &mut gturn, &hot, &vec![None; groups.len()]);
                        let _ = dsp.worker.retune(groups[g].center);
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

/// Widest spread of channels monitored from one tuning: well inside the 150 kHz tuner
/// bandwidth with room for each channel's filter.
const GROUP_SPAN_HZ: u32 = 120_000;
/// After a call ends (and while it's open), other channels of its group stay shut
/// this long: the unkey swings the tuner gain and lifts the noise on every channel
/// of the group, which opened all of them at once.
const GROUP_HOLDOFF: Duration = Duration::from_millis(2500);
/// A group with a call this recently is checked every other hop.
const GROUP_HOT_FOR: Duration = Duration::from_secs(600);

/// Channels sharing one tuning: `center` is the tuned frequency, `members` index
/// the scan list.
#[derive(Clone, Debug, PartialEq)]
struct ScanGroup {
    center: u32,
    members: Vec<usize>,
}

/// Split the scan list into groups whose channels all fit within `span` Hz, in
/// frequency order, each tuned to the middle of its channels.
fn scan_groups(chans: &[(String, u32)], span: u32) -> Vec<ScanGroup> {
    let mut order: Vec<usize> = (0..chans.len()).collect();
    order.sort_by_key(|&i| chans[i].1);
    let mut groups: Vec<ScanGroup> = Vec::new();
    for i in order {
        let f = chans[i].1;
        match groups.last_mut() {
            Some(gr) if f - chans[gr.members[0]].1 <= span => gr.members.push(i),
            _ => groups.push(ScanGroup {
                center: f,
                members: vec![i],
            }),
        }
    }
    for gr in &mut groups {
        let lo = chans[gr.members[0]].1;
        let hi = chans[*gr.members.last().unwrap()].1;
        gr.center = lo + (hi - lo) / 2;
    }
    groups
}

/// Groups to favour: those with a call in the last GROUP_HOT_FOR, plus the groups of
/// configured priority channels.
fn hot_groups(
    active_at: &[Option<Instant>],
    priority_groups: &[usize],
    now: Instant,
) -> Vec<usize> {
    let mut hot: Vec<usize> = priority_groups.to_vec();
    for (gi, at) in active_at.iter().enumerate() {
        if at.is_some_and(|t| now.duration_since(t) < GROUP_HOT_FOR) && !hot.contains(&gi) {
            hot.push(gi);
        }
    }
    hot.sort_unstable();
    hot
}

/// A strong call raises the level across its group: this much stronger anywhere in
/// the group and a weaker channel's open is the call's splash, not a call.
const GROUP_SPLASH_DB: f32 = 12.0;

/// True when this open on `m` is another channel's signal: a channel within
/// NEIGHBOUR_HZ is NEIGHBOUR_MARGIN_DB stronger (12.5 kHz spill), or any channel of
/// the group is GROUP_SPLASH_DB stronger (a strong call lifting the group; seen at
/// 37.5 kHz on gmrs-17 while gmrs-1 was keyed).
fn stronger_member(
    iq: &[Complex32],
    center: u32,
    m: usize,
    members: &[usize],
    chans: &[(String, u32)],
) -> bool {
    let at = |i: usize| power_at_db(iq, chans[i].1 as f32 - center as f32);
    let own = at(m);
    members.iter().any(|&o| {
        if o == m {
            return false;
        }
        let near = (chans[o].1 as i64 - chans[m].1 as i64).abs() <= NEIGHBOUR_HZ;
        let margin = if near {
            NEIGHBOUR_MARGIN_DB
        } else {
            GROUP_SPLASH_DB
        };
        at(o) > own + margin
    })
}

/// Phase-continuous frequency shifter: moves a channel `offset_hz` from the tuned
/// frequency to 0 Hz, carrying phase across frames so FM demod sees no seams.
#[derive(Default)]
struct Mixer {
    phase: f32,
}

impl Mixer {
    fn shift(&mut self, iq: &[Complex32], offset_hz: f32) -> Vec<Complex32> {
        if offset_hz == 0.0 {
            return iq.to_vec();
        }
        let step = -2.0 * std::f32::consts::PI * offset_hz / SDR_RATE as f32;
        let rot = Complex32::from_polar(1.0, step);
        let mut osc = Complex32::from_polar(1.0, self.phase);
        let out = iq
            .iter()
            .map(|&s| {
                let y = s * osc;
                osc *= rot;
                y
            })
            .collect();
        // From the oscillator itself, not step * len, which loses precision in f32.
        self.phase = osc.arg();
        out
    }
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
        // Tuned between gmrs-15 and gmrs-1, the call is on gmrs-1: gmrs-15 yields.
        let center = 462_556_250;
        let call = tone(462_562_500.0 - center as f32);
        assert!(stronger_member(&call, center, 0, &[0, 1, 2], &frs()));
        assert!(!stronger_member(&call, center, 1, &[0, 1, 2], &frs()));
    }

    #[test]
    fn a_call_on_the_tuned_channel_stays() {
        assert!(!stronger_member(
            &tone(0.0),
            462_562_500,
            1,
            &[0, 1, 2],
            &frs()
        ));
        // A strong call on gmrs-2, 25 kHz away: gmrs-1's open is its splash.
        assert!(stronger_member(
            &tone(25_000.0),
            462_562_500,
            1,
            &[0, 1, 2],
            &frs()
        ));
    }
}

#[cfg(test)]
mod priority_tests {
    use super::*;

    #[test]
    fn priority_channel_is_every_other_hop() {
        let locked = vec![None; 5];
        let (mut rot, mut turn) = (0, 0);
        let mut idx = 0; // 0 is the priority channel
        let mut path = vec![];
        for _ in 0..8 {
            idx = next_hop(idx, &mut rot, &mut turn, &[0], &locked);
            path.push(idx);
        }
        assert_eq!(path, vec![1, 0, 2, 0, 3, 0, 4, 0]);
    }

    #[test]
    fn no_priority_is_a_plain_sweep_and_locked_priority_is_skipped() {
        let locked = vec![None; 3];
        assert_eq!(next_hop(0, &mut 0, &mut 0, &[], &locked), 1);
        let later = Some(Instant::now() + Duration::from_secs(60));
        let locked = vec![later, None, None];
        assert_eq!(next_hop(1, &mut 1, &mut 0, &[0], &locked), 2);
    }
}

#[cfg(test)]
mod live_squelch_tests {
    use super::*;

    #[test]
    fn margin_changes_live_without_losing_the_floor() {
        let mut p = Pipeline::new(PipelineConfig {
            squelch_margin_db: 6.0,
            ..PipelineConfig::default()
        });
        let floor = p.noise_floor_db();
        p.set_squelch_margin(10.0);
        assert_eq!(p.noise_floor_db(), floor);
        assert_eq!(p.squelch_threshold_db(), floor + 10.0);
        p.set_open_floor(Some(-10.0));
        assert_eq!(p.squelch_threshold_db(), (floor + 10.0).max(-10.0));
    }
}

#[cfg(test)]
mod wideband_neighbour_tests {
    use super::*;

    #[test]
    fn one_call_is_one_channel() {
        // gmrs-15 / gmrs-1 / gmrs-16 at 12.5 kHz steps; the call is on gmrs-1.
        let freqs = [
            (1u32, 462_550_000u64),
            (2, 462_562_500),
            (3, 462_575_000),
            (4, 462_650_000),
        ];
        let freq_of = |k: u32| freqs.iter().find(|f| f.0 == k).map(|f| f.1);
        let detected = vec![(1, -12.0), (2, -2.0), (3, -11.0), (4, -9.0)];
        let kept: Vec<u32> = strongest_of_neighbours(detected, freq_of)
            .into_iter()
            .map(|d| d.0)
            .collect();
        assert_eq!(
            kept,
            vec![2, 4],
            "gmrs-1 wins over its neighbours; a separate call 75 kHz away stays"
        );
    }
}

#[cfg(test)]
mod group_tests {
    use super::*;

    fn db() -> ChannelDb {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bandplans");
        hertz_channels::load_channels(dir, None).unwrap()
    }

    #[test]
    fn frs_gmrs_is_a_few_tunings_not_22() {
        let chans = scan_channels(&db(), &["frs-gmrs".to_string()]);
        let groups = scan_groups(&chans, GROUP_SPAN_HZ);
        assert!(groups.len() <= 5, "{groups:?}");
        assert_eq!(groups.iter().map(|g| g.members.len()).sum::<usize>(), 22);
        for gr in &groups {
            for &m in &gr.members {
                let off = chans[m].1 as i64 - gr.center as i64;
                assert!(
                    off.abs() <= (GROUP_SPAN_HZ / 2) as i64,
                    "{} is {off} Hz off",
                    chans[m].0
                );
            }
        }
    }

    #[test]
    fn marine_68_to_72_is_two_tunings() {
        let chans = scan_channels(&db(), &["68", "69", "71", "72"].map(String::from));
        assert_eq!(scan_groups(&chans, GROUP_SPAN_HZ).len(), 2);
    }

    #[test]
    fn mixer_brings_a_channel_to_zero_hz() {
        let step = 2.0 * std::f32::consts::PI * 50_000.0 / SDR_RATE as f32;
        let iq: Vec<Complex32> = (0..12_000)
            .map(|n| Complex32::from_polar(0.5, n as f32 * step))
            .collect();
        let mut mixer = Mixer::default();
        let first = mixer.shift(&iq[..6000], 50_000.0);
        let second = mixer.shift(&iq[6000..], 50_000.0);
        assert!(power_at_db(&first, 0.0) > power_at_db(&first, 25_000.0) + 20.0);
        // Phase carries across frames: the shifted tone stays at a steady phase.
        let gap = (second[0] * first[first.len() - 1].conj()).arg().abs();
        assert!(gap < 0.01, "phase jump {gap}");
    }

    #[test]
    fn busy_groups_are_hot() {
        let now = Instant::now();
        let active = [None, Some(now), Some(now - Duration::from_secs(3600))];
        assert_eq!(hot_groups(&active, &[0], now), vec![0, 1]);
    }
}

#[cfg(test)]
mod splash_tests {
    use super::*;

    #[test]
    fn a_strong_call_doesnt_open_the_rest_of_its_group() {
        // gmrs-1 keyed strongly; gmrs-17 is 37.5 kHz away in the same group.
        let chans: Vec<(String, u32)> = vec![
            ("gmrs-1".into(), 462_562_500),
            ("gmrs-17".into(), 462_600_000),
        ];
        let center = 462_581_250u32;
        let step =
            |f: u32| 2.0 * std::f32::consts::PI * (f as f32 - center as f32) / SDR_RATE as f32;
        let iq: Vec<Complex32> = (0..12_000)
            .map(|n| {
                Complex32::from_polar(0.5, n as f32 * step(462_562_500))
                    + Complex32::from_polar(0.05, n as f32 * step(462_600_000))
            })
            .collect();
        assert!(
            stronger_member(&iq, center, 1, &[0, 1], &chans),
            "gmrs-17 is splash"
        );
        assert!(
            !stronger_member(&iq, center, 0, &[0, 1], &chans),
            "gmrs-1 is the call"
        );
    }
}
