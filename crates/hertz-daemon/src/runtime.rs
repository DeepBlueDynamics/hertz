//! `Daemon` — owns the config, event bus, recorder, history, dongle roster, and the
//! network server. `Daemon::start` spawns the per-dongle DSP threads (sync) + pump
//! tasks (async) + recorder + server, and returns a handle for graceful shutdown.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use hertz_channels::ChannelDb;
use hertz_types::{DaemonConfig, DongleConfig, DongleRole, Event};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info};

use crate::bus::EventBus;
use crate::control::{DongleControl, DongleRuntimeHandle};
use crate::factory::SdrFactory;
use crate::history::History;
use crate::pipeline_bridge::{spawn_channelized_loop, spawn_monitor_loop, DspThread};
use crate::recorder::RecorderHandle;
use crate::transcribe::{DisabledTranscriber, Transcriber};

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
    pub dsp_threads: Vec<std::thread::JoinHandle<()>>,
    pub history_handle: Option<JoinHandle<()>>,
    pub recorder: Option<RecorderHandle>,
    pub server_handle: Option<JoinHandle<Result<(), std::io::Error>>>,
    pub server_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    pub shutdown_flags: Vec<Arc<AtomicBool>>,
    pub listen_addr: String,
}

impl Daemon {
    /// Boot the daemon: load channels, spawn DSP threads, pumps, recorder, history
    /// subscriber, and the axum server (bound but not necessarily awaited).
    pub async fn start(
        config: DaemonConfig,
        factory: Arc<dyn SdrFactory>,
        channels: ChannelDb,
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

        // One (event,audio) sender-pair feeds the bus's two broadcast channels.
        let (ev_tx, au_tx) = bus.clone().spawn_pumps();
        let ev_tx = Arc::new(ev_tx);
        let au_tx = Arc::new(au_tx);

        // Recorder (one for all dongles). Bridges send transmission jobs to it.
        let recorder = RecorderHandle::spawn(
            bus.clone(),
            std::path::PathBuf::from(&config.daemon.data_dir),
            Box::new(DisabledTranscriber) as Box<dyn Transcriber>,
        );
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

            let sdr_cfg = sdr_config_for(dc, center_hz, freq_hz);
            let worker = match factory.spawn_worker(&dc.serial, sdr_cfg) {
                Ok(w) => Arc::new(w),
                Err(e) => {
                    error!("failed to open dongle {id} ({}): {e}", dc.serial);
                    control.set_online(false, Some(format!("open failed: {e}")));
                    let _ = ev_tx.send(Event::DongleStatus {
                        dongle_id: id.clone(),
                        serial: dc.serial.clone(),
                        online: false,
                        role: dc.role,
                        message: Some(format!("open failed: {e}")),
                    });
                    continue;
                }
            };
            let shutdown = Arc::new(AtomicBool::new(false));
            shutdown_flags.push(shutdown.clone());

            let dsp = DspThread {
                dongle_idx: idx as u8,
                worker: worker.clone(),
                control: control.clone(),
                event_tx: (*ev_tx).clone(),
                audio_tx: (*au_tx).clone(),
                recorder_tx: Some(recorder_tx.clone()),
                shutdown: shutdown.clone(),
                channels: channels.clone(),
            };

            let join = match dc.role {
                DongleRole::Monitor => spawn_monitor_loop(dsp),
                DongleRole::Channelized => {
                    let (rate, center) = wideband_params(&channels, dc, center_hz, freq_hz);
                    spawn_channelized_loop(dsp, center, rate as u32)
                }
                DongleRole::Hopscan => {
                    info!("dongle {id} role hopscan — not implemented in T4, skipping spawn");
                    continue;
                }
            };
            dsp_threads.push(join);
            dongles.push(DongleRuntimeHandle {
                control,
                worker,
                shutdown,
            });
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
            history_handle: Some(history_handle),
            recorder: Some(recorder),
            server_handle,
            server_shutdown,
            shutdown_flags,
            listen_addr,
        })
    }

    /// Signal all DSP threads + workers to stop (the axum graceful-shutdown trigger
    /// fires in [`Daemon::join`], which owns `self`).
    pub fn shutdown(&self) {
        for f in &self.shutdown_flags {
            f.store(true, Ordering::Relaxed);
        }
        for d in &self.dongles {
            let _ = d.worker.shutdown();
        }
    }

    /// Join the DSP OS threads + recorder, fire the axum graceful-shutdown signal,
    /// and abort the long-lived async tasks (server, history, pumps) so this future
    /// never hangs. Those tasks are tied to the runtime lifetime; aborting here lets
    /// `join` return promptly.
    pub async fn join(mut self) {
        // Trigger axum graceful shutdown (send consumes the sender).
        if let Some(tx) = self.server_shutdown.take() {
            let _ = tx.send(());
        }
        for j in self.dsp_threads.drain(..) {
            if let Err(e) = j.join() {
                error!("dsp thread panicked: {e:?}");
            }
        }
        if let Some(r) = self.recorder.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), r.shutdown()).await;
        }
        if let Some(h) = self.history_handle.take() {
            h.abort();
        }
        if let Some(s) = self.server_handle.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), s).await;
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

fn sdr_config_for(dc: &DongleConfig, center_hz: Option<u64>, freq_hz: u64) -> hertz_sdr::SdrConfig {
    let mut cfg = hertz_sdr::SdrConfig::default();
    match dc.role {
        DongleRole::Monitor => {
            cfg.sample_rate = hertz_dsp::pipeline::SDR_RATE;
            cfg.center_freq = freq_hz as u32;
            cfg.bandwidth = 150_000;
            cfg.buffer_size_iq_pairs = 48_000;
            cfg.ring_capacity_bytes = 2 * 240_000 * 2;
        }
        DongleRole::Channelized | DongleRole::Hopscan => {
            cfg.sample_rate = center_hz
                .and_then(|_| Some(2_400_000u32))
                .unwrap_or(2_400_000);
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
