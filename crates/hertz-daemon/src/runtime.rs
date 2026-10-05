//! `Daemon` — owns the config, event bus, recorder, history, dongle roster, and the
//! network server. `Daemon::start` spawns the per-dongle DSP threads (sync) + pump
//! tasks (async) + recorder + server, and returns a handle for graceful shutdown.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle as ThreadJoinHandle;
use std::time::Duration;

use hertz_channels::ChannelDb;
use hertz_sdr::{SdrConfig, WorkerHandle};
use hertz_types::{DaemonConfig, DongleConfig, DongleRole, Event};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::audio::AudioFrame;
use crate::bus::EventBus;
use crate::control::{DongleControl, DongleRuntimeHandle};
use crate::factory::SdrFactory;
use crate::history::History;
use crate::pipeline_bridge::{spawn_channelized_loop, spawn_monitor_loop, DspThread};
use crate::recorder::{RecorderHandle, RecorderTx};
use crate::spectrum::SpectrumFrame;
use crate::transcribe::{DisabledTranscriber, Transcriber, WhistleTranscriber};

/// Shared daemon state surfaced to HTTP handlers.
#[derive(Clone)]
pub struct DaemonState {
    pub config: Arc<DaemonConfig>,
    pub bus: EventBus,
    pub controls: Arc<std::sync::Mutex<HashMap<String, DongleControl>>>,
    pub history: Arc<History>,
    pub channels: Arc<ChannelDb>,
    pub started_at: std::time::Instant,
    pub auth_token: Option<String>,
}

impl DaemonState {
    pub fn dongle_summaries(&self) -> Vec<hertz_types::DongleSummary> {
        let g = self.controls.lock().unwrap();
        let mut out: Vec<_> = g.values().map(|c| c.snapshot().to_summary()).collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    pub fn control_for(&self, id: &str) -> Option<DongleControl> {
        self.controls.lock().unwrap().get(id).cloned()
    }
}

/// Running daemon handle.
pub struct Daemon {
    pub state: DaemonState,
    pub dongles: Vec<DongleRuntimeHandle>,
    pub dsp_threads: Vec<ThreadJoinHandle<()>>,
    /// DSP threads spawned by reconnect watchers after `start` returned (a dongle
    /// that wasn't there at boot came online later). Joined in [`Daemon::join`].
    pub late_dsp_threads: Arc<Mutex<Vec<ThreadJoinHandle<()>>>>,
    /// Dongles that came online via a reconnect watcher (`worker.shutdown()` must
    /// reach them too).
    pub late_dongles: Arc<Mutex<Vec<DongleRuntimeHandle>>>,
    /// Reconnect-watcher threads for dongles whose initial open failed.
    pub watchers: Vec<ThreadJoinHandle<()>>,
    pub history_handle: Option<JoinHandle<()>>,
    pub recorder: Option<RecorderHandle>,
    pub server_handle: Option<JoinHandle<Result<(), std::io::Error>>>,
    pub server_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    pub shutdown_flags: Vec<Arc<AtomicBool>>,
    pub listen_addr: String,
}

impl Daemon {
    /// Boot the daemon with the default ~5 s dongle reconnect interval. See
    /// [`Daemon::start_with_reconnect_interval`].
    pub async fn start(
        config: DaemonConfig,
        factory: Arc<dyn SdrFactory>,
        channels: ChannelDb,
    ) -> anyhow::Result<Self> {
        Self::start_with_reconnect_interval(config, factory, channels, Duration::from_secs(5)).await
    }

    /// Boot the daemon: load channels, spawn DSP threads, pumps, recorder, history
    /// subscriber, and the axum server (bound but not necessarily awaited).
    ///
    /// Dongles whose SDR open fails at startup are marked offline and given a
    /// dedicated reconnect watcher that retries the open by serial every
    /// `reconnect_interval` forever — emitting `DongleStatus` on each transition —
    /// until the device appears, at which point the dongle comes fully online and
    /// its DSP thread is spawned (mirrors `hertz_sdr`'s mid-run reconnect).
    pub async fn start_with_reconnect_interval(
        config: DaemonConfig,
        factory: Arc<dyn SdrFactory>,
        channels: ChannelDb,
        reconnect_interval: Duration,
    ) -> anyhow::Result<Self> {
        let bus = EventBus::new();
        let channels = Arc::new(channels);
        let history = Arc::new(History::open(std::path::Path::new(
            &config.daemon.data_dir,
        ))?);
        let auth_token = config.daemon.get_auth_token();

        let mut controls = HashMap::new();
        let mut dongles = Vec::new();
        let mut dsp_threads = Vec::new();
        let mut shutdown_flags = Vec::new();
        let mut watchers = Vec::new();
        // Late registries shared with reconnect watchers (filled when a dongle
        // comes online after start returns).
        let late_dsp_threads: Arc<Mutex<Vec<ThreadJoinHandle<()>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let late_dongles: Arc<Mutex<Vec<DongleRuntimeHandle>>> = Arc::new(Mutex::new(Vec::new()));

        // One (event,audio,spectrum) sender-triple feeds the bus's three broadcast
        // channels.
        let (ev_tx, au_tx, sp_tx) = bus.clone().spawn_pumps();
        let ev_tx = Arc::new(ev_tx);
        let au_tx = Arc::new(au_tx);
        let sp_tx = Arc::new(sp_tx);

        // Recorder (one for all dongles). Bridges send transmission jobs to it.
        let transcriber = transcriber_for(&config);
        let data_dir = std::path::PathBuf::from(&config.daemon.data_dir);
        let recorder =
            RecorderHandle::spawn(bus.clone(), data_dir.clone(), Arc::clone(&transcriber));
        crate::recorder::spawn_backfill(bus.clone(), transcriber, data_dir);
        let recorder_tx = recorder.tx.clone();

        for (idx, dc) in config.dongles.iter().enumerate() {
            let id = dongle_id(dc, idx);
            let center_hz = group_center(&channels, dc);
            let freq_hz = dc.frequency_hz.unwrap_or(center_hz.unwrap_or(156_800_000));
            let control = DongleControl::new(
                id.clone(),
                dc.serial.clone(),
                dc.role,
                dc.bandplan.clone(),
                dc.tap_channel.clone(),
                dc.groups.clone().unwrap_or_default(),
                freq_hz,
                dc.squelch_db,
                dc.record,
                true,
            );
            controls.insert(id.clone(), control.clone());

            if matches!(dc.role, DongleRole::Hopscan) {
                info!("dongle {id} role hopscan — not implemented, skipping spawn");
                continue;
            }

            let (wb_rate, wb_center) = wideband_params(&channels, dc, center_hz, freq_hz);
            // Warn about bandplan RX channels this capture span can't see (e.g. the
            // WX channels beyond the marine band) — uses the EFFECTIVE center/rate
            // the DSP will actually capture, including the defaults applied above.
            warn_out_of_span_channels(idx, dc, &channels, wb_center, wb_rate as u32);
            let ctx = DongleLaunchCtx {
                dongle_idx: idx as u8,
                role: dc.role,
                control: control.clone(),
                ev_tx: ev_tx.clone(),
                au_tx: au_tx.clone(),
                sp_tx: sp_tx.clone(),
                recorder_tx: recorder_tx.clone(),
                channels: channels.clone(),
                center_hz: wb_center,
                sample_rate_hz: wb_rate as u32,
                dongle_id: id.clone(),
                serial: dc.serial.clone(),
                scan: dc.scan.clone().unwrap_or_default(),
            };

            let sdr_cfg = sdr_config_for(dc, center_hz, freq_hz);
            match factory.spawn_worker(&dc.serial, sdr_cfg.clone()) {
                Ok(w) => {
                    let worker = Arc::new(w);
                    let shutdown = Arc::new(AtomicBool::new(false));
                    shutdown_flags.push(shutdown.clone());
                    let (join, handle) = launch_dsp(&ctx, worker, shutdown);
                    dsp_threads.push(join);
                    dongles.push(handle);
                }
                Err(e) => {
                    error!(
                        "failed to open dongle {id} ({}): {e}; marking offline, will retry every {:?}",
                        dc.serial, reconnect_interval
                    );
                    control.set_online(false, Some(format!("open failed: {e}")));
                    let _ = ev_tx.send(Event::DongleStatus {
                        dongle_id: id.clone(),
                        serial: dc.serial.clone(),
                        online: false,
                        role: dc.role,
                        message: Some(format!("open failed: {e}")),
                    });
                    // Startup reconnect watcher: retry the open forever until the
                    // device appears, then come online + spawn its DSP thread.
                    let shutdown = Arc::new(AtomicBool::new(false));
                    shutdown_flags.push(shutdown.clone());
                    let watcher = spawn_reconnect_watcher(
                        ctx,
                        factory.clone(),
                        sdr_cfg,
                        shutdown,
                        reconnect_interval,
                        late_dsp_threads.clone(),
                        late_dongles.clone(),
                    );
                    watchers.push(watcher);
                }
            }
        }

        let state = DaemonState {
            config: Arc::new(config.clone()),
            bus: bus.clone(),
            controls: Arc::new(std::sync::Mutex::new(controls)),
            history: history.clone(),
            channels,
            started_at: std::time::Instant::now(),
            auth_token,
        };

        // History subscriber: records activity + transcripts from the bus.
        let history_handle = spawn_history_subscriber(state.clone());

        let listen_addr = config.daemon.listen.clone();
        info!("hertzd listening on {listen_addr}");
        let (server_handle, server_shutdown) =
            match crate::server::serve(state.clone(), &listen_addr).await {
                Ok((h, s)) => (Some(h), Some(s)),
                Err(e) => {
                    error!("failed to bind {listen_addr}: {e}");
                    (None, None)
                }
            };

        Ok(Self {
            state,
            dongles,
            dsp_threads,
            late_dsp_threads,
            late_dongles,
            watchers,
            history_handle: Some(history_handle),
            recorder: Some(recorder),
            server_handle,
            server_shutdown,
            shutdown_flags,
            listen_addr,
        })
    }

    /// Signal all DSP threads + workers (including ones still retrying, and ones
    /// that came online late) to stop. The axum graceful-shutdown trigger fires in
    /// [`Daemon::join`], which owns `self`.
    pub fn shutdown(&self) {
        for f in &self.shutdown_flags {
            f.store(true, Ordering::Relaxed);
        }
        for d in &self.dongles {
            let _ = d.worker.shutdown();
        }
        // Dongles that reconnected after start have workers not in `self.dongles`.
        if let Ok(late) = self.late_dongles.lock() {
            for d in late.iter() {
                let _ = d.worker.shutdown();
            }
        }
    }

    /// Join the DSP OS threads + reconnect watchers + recorder, fire the axum
    /// graceful-shutdown signal, and abort the long-lived async tasks (server,
    /// history, pumps) so this future never hangs. Those tasks are tied to the
    /// runtime lifetime; aborting here lets `join` return promptly.
    pub async fn join(mut self) {
        // Trigger axum graceful shutdown (send consumes the sender).
        if let Some(tx) = self.server_shutdown.take() {
            let _ = tx.send(());
        }
        // Startup DSP threads.
        for j in self.dsp_threads.drain(..) {
            if let Err(e) = j.join() {
                error!("dsp thread panicked: {e:?}");
            }
        }
        // Reconnect watchers (exit promptly once their flag is set), then any DSP
        // threads they spawned after start returned.
        for w in self.watchers.drain(..) {
            if let Err(e) = w.join() {
                error!("reconnect watcher panicked: {e:?}");
            }
        }
        let late = std::mem::take(&mut *self.late_dsp_threads.lock().unwrap());
        for j in late {
            if let Err(e) = j.join() {
                error!("late dsp thread panicked: {e:?}");
            }
        }
        if let Some(r) = self.recorder.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), r.shutdown()).await;
        }
        if let Some(h) = self.history_handle.take() {
            h.abort();
        }
        if let Some(s) = self.server_handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), s).await;
        }
    }
}

fn dongle_id(dc: &DongleConfig, idx: usize) -> String {
    format!("{}-{}", dc.serial, idx)
}

fn group_center(channels: &ChannelDb, dc: &DongleConfig) -> Option<u64> {
    let group = dc
        .groups
        .as_ref()
        .and_then(|g| g.first())
        .or(dc.bandplan.as_ref())?;
    channels.groups.get(group).and_then(|g| g.center_hz)
}

fn wideband_params(
    channels: &ChannelDb,
    dc: &DongleConfig,
    center_hz: Option<u64>,
    freq_hz: u64,
) -> (u64, u64) {
    let group = dc
        .groups
        .as_ref()
        .and_then(|g| g.first())
        .or(dc.bandplan.as_ref());
    let rate = group
        .and_then(|g| channels.groups.get(g).and_then(|m| m.sample_rate))
        .unwrap_or(2_400_000);
    let center = center_hz.unwrap_or(freq_hz);
    (rate as u64, center)
}

/// Worker read size (IQ pairs) for a scanning monitor dongle: 50 ms at 240 kS/s.
pub const SCAN_READ_IQ_PAIRS: usize = 12_000;

fn sdr_config_for(dc: &DongleConfig, center_hz: Option<u64>, freq_hz: u64) -> hertz_sdr::SdrConfig {
    let mut cfg = hertz_sdr::SdrConfig::default();
    match dc.role {
        DongleRole::Monitor => {
            cfg.sample_rate = hertz_dsp::pipeline::SDR_RATE;
            cfg.center_freq = freq_hz as u32;
            cfg.bandwidth = 150_000;
            // Scanning: 50 ms reads so the post-retune settle discard is short.
            cfg.buffer_size_iq_pairs = if dc.scan.as_ref().is_some_and(|s| !s.is_empty()) {
                SCAN_READ_IQ_PAIRS
            } else {
                48_000
            };
            cfg.ring_capacity_bytes = 2 * 240_000 * 2;
        }
        DongleRole::Channelized | DongleRole::Hopscan => {
            cfg.sample_rate = center_hz.map(|_| 2_400_000u32).unwrap_or(2_400_000);
            cfg.center_freq = center_hz.unwrap_or(freq_hz) as u32;
            cfg.bandwidth = cfg.sample_rate;
            cfg.buffer_size_iq_pairs = 480_000;
            cfg.ring_capacity_bytes = 2 * 2_400_000 * 2;
        }
    }
    let _ = dc;
    cfg
}

fn spawn_history_subscriber(state: DaemonState) -> JoinHandle<()> {
    let mut rx = state.bus.subscribe_events();
    let history = state.history.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => record_history(&history, &ev),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("history subscriber lagged by {n} events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

fn record_history(history: &History, ev: &Event) {
    let now = now_ts();
    match ev {
        Event::SquelchEvent {
            dongle_id,
            channel,
            freq_hz,
            signal_db,
            classification,
            open,
        } => {
            history.record_activity(hertz_types::wire::ActivityEntry {
                ts_sec: now,
                dongle_id: dongle_id.clone(),
                channel: channel.clone(),
                freq_hz: *freq_hz,
                signal_db: *signal_db,
                classification: classification.clone(),
                open: *open,
            });
        }
        Event::Transcription {
            dongle_id,
            channel,
            freq_hz,
            text,
        } => {
            history.record_transcript(hertz_types::wire::TranscriptEntry {
                ts_sec: now,
                dongle_id: dongle_id.clone(),
                channel: channel.clone(),
                freq_hz: *freq_hz,
                text: text.clone(),
            });
        }
        _ => {}
    }
}

fn now_ts() -> f32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f32())
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Per-dongle launch + startup reconnect
// ---------------------------------------------------------------------------

/// Owned context for (re)launching a dongle's DSP thread. Bundles everything the
/// [`launch_dsp`] helper and the reconnect watcher need, without borrowing the
/// config (so a watcher outlives `start`'s borrow of `config.dongles`).
#[derive(Clone)]
struct DongleLaunchCtx {
    dongle_idx: u8,
    role: DongleRole,
    control: DongleControl,
    ev_tx: Arc<mpsc::UnboundedSender<Event>>,
    au_tx: Arc<mpsc::UnboundedSender<AudioFrame>>,
    sp_tx: Arc<mpsc::UnboundedSender<SpectrumFrame>>,
    recorder_tx: RecorderTx,
    channels: Arc<ChannelDb>,
    /// Wideband center/rate for the channelized role (ignored by the monitor arm).
    center_hz: u64,
    sample_rate_hz: u32,
    dongle_id: String,
    serial: String,
    /// Monitor-role scan list (channel ids); empty = fixed frequency.
    scan: Vec<String>,
}

/// Build the [`DspThread`] for an already-opened worker and spawn its DSP thread.
/// The shutdown flag is shared with the caller (the startup loop or a reconnect
/// watcher) so one signal stops both.
fn launch_dsp(
    ctx: &DongleLaunchCtx,
    worker: Arc<WorkerHandle>,
    shutdown: Arc<AtomicBool>,
) -> (ThreadJoinHandle<()>, DongleRuntimeHandle) {
    let dsp = DspThread {
        dongle_idx: ctx.dongle_idx,
        worker: worker.clone(),
        control: ctx.control.clone(),
        event_tx: (*ctx.ev_tx).clone(),
        audio_tx: (*ctx.au_tx).clone(),
        spectrum_tx: (*ctx.sp_tx).clone(),
        recorder_tx: Some(ctx.recorder_tx.clone()),
        shutdown: shutdown.clone(),
        channels: ctx.channels.clone(),
        scan: ctx.scan.clone(),
    };
    let join = match ctx.role {
        DongleRole::Monitor => spawn_monitor_loop(dsp),
        DongleRole::Channelized => spawn_channelized_loop(dsp, ctx.center_hz, ctx.sample_rate_hz),
        DongleRole::Hopscan => unreachable!("hopscan dongles are never launched"),
    };
    let handle = DongleRuntimeHandle {
        control: ctx.control.clone(),
        worker,
        shutdown,
    };
    (join, handle)
}

/// Spawn a reconnect watcher for a dongle whose initial open failed. Retries the
/// open by serial every `retry_interval` forever; on success it marks the dongle
/// online, emits `DongleStatus`, launches the DSP thread (sharing `shutdown`), and
/// registers it in the late registries for [`Daemon::join`] / [`Daemon::shutdown`].
fn spawn_reconnect_watcher(
    ctx: DongleLaunchCtx,
    factory: Arc<dyn SdrFactory>,
    sdr_cfg: SdrConfig,
    shutdown: Arc<AtomicBool>,
    retry_interval: Duration,
    late_dsp_threads: Arc<Mutex<Vec<ThreadJoinHandle<()>>>>,
    late_dongles: Arc<Mutex<Vec<DongleRuntimeHandle>>>,
) -> ThreadJoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("reconnect-{}", ctx.dongle_id))
        .spawn(move || {
            // Watch the shutdown flag at a fine granularity even when the retry
            // interval is long, so Daemon::join doesn't have to wait for it.
            let poll = Duration::from_millis(200);
            loop {
                let mut waited = Duration::ZERO;
                while waited < retry_interval {
                    if shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                    let step = poll.min(retry_interval - waited);
                    std::thread::sleep(step);
                    waited += step;
                }
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                match factory.spawn_worker(&ctx.serial, sdr_cfg.clone()) {
                    Ok(w) => {
                        let worker = Arc::new(w);
                        // Don't bring a dongle online mid-shutdown.
                        if shutdown.load(Ordering::Relaxed) {
                            let _ = worker.shutdown();
                            return;
                        }
                        let (join, handle) = launch_dsp(&ctx, worker, shutdown.clone());
                        ctx.control.set_online(true, None);
                        let _ = ctx.ev_tx.send(Event::DongleStatus {
                            dongle_id: ctx.dongle_id.clone(),
                            serial: ctx.serial.clone(),
                            online: true,
                            role: ctx.role,
                            message: None,
                        });
                        info!(
                            "dongle {} ({}) came online after reconnect retry",
                            ctx.dongle_id, ctx.serial
                        );
                        late_dsp_threads.lock().unwrap().push(join);
                        late_dongles.lock().unwrap().push(handle);
                        return; // watcher's job is done; mid-run drops are handled
                                // by the SDR worker's own reconnect loop.
                    }
                    Err(e) => {
                        debug!(
                            "dongle {} ({}) still unavailable: {}",
                            ctx.dongle_id, ctx.serial, e
                        );
                        // keep retrying
                    }
                }
            }
        })
        .expect("spawn reconnect watcher thread")
}

/// Warn at startup about bandplan RX channels that fall outside a channelized
/// dongle's capture span (e.g. WX channels beyond the marine band) — they can
/// never be received on this dongle. `center_hz`/`sample_rate_hz` are the
/// EFFECTIVE capture parameters the DSP will use (defaults already applied), so
/// this matches what the dongle actually hears. No-op for the monitor/hopscan roles.
fn warn_out_of_span_channels(
    idx: usize,
    dc: &DongleConfig,
    channels: &ChannelDb,
    center_hz: u64,
    sample_rate_hz: u32,
) {
    if !matches!(dc.role, DongleRole::Channelized) {
        return;
    }
    let rate = sample_rate_hz as u64;
    let lo = center_hz.saturating_sub(rate / 2);
    let hi = center_hz + rate / 2;

    let group = dc
        .groups
        .as_ref()
        .and_then(|g| g.first())
        .or(dc.bandplan.as_ref());
    let chans: Vec<&hertz_types::Channel> = match group {
        Some(g) => channels.get_by_group(g),
        None => channels.channels.values().collect(),
    };
    let outside: Vec<&hertz_types::Channel> = chans
        .into_iter()
        .filter(|c| c.rx && (c.freq_hz < lo || c.freq_hz > hi))
        .collect();
    if outside.is_empty() {
        return;
    }
    let list = outside
        .iter()
        .map(|c| format!("{} '{}' ({:.4} MHz)", c.id, c.label, c.freq_hz as f64 / 1e6))
        .collect::<Vec<_>>()
        .join(", ");
    warn!(
        "dongle {}-{} ({:?}, group {:?}) capture span {:.4}–{:.4} MHz excludes {} bandplan RX channel(s): {}",
        dc.serial,
        idx,
        dc.role,
        group,
        lo as f64 / 1e6,
        hi as f64 / 1e6,
        outside.len(),
        list
    );
}

/// Pick the transcription engine from config. `whistle` runs Cactus Whistle on-device;
/// anything else, or no section, disables transcription.
fn transcriber_for(config: &DaemonConfig) -> Arc<dyn Transcriber> {
    match &config.transcription {
        Some(t) if t.engine == "whistle" => {
            WhistleTranscriber::spawn(&t.language, t.keywords.clone())
        }
        Some(t) => {
            warn!(
                "transcription engine {:?} not supported; transcription disabled",
                t.engine
            );
            Arc::new(DisabledTranscriber)
        }
        None => Arc::new(DisabledTranscriber),
    }
}
