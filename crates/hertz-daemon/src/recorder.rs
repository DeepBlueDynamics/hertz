//! Recorder task: subscribes to transmission jobs from
//! the DSP bridges and writes a WAV per transmission under `data_dir/recordings/`
//! via `hertz_dsp::recorder`, then emits `RecordingSaved` on the bus and appends a
//! line to `data_dir/logs/events.log`. Transcription is called on each save (Phase 6
//! fills in real engines; today it's a no-op).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hertz_types::Event;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::bus::EventBus;
use crate::transcribe::Transcriber;

/// One completed transmission ready to be recorded.
#[derive(Clone, Debug)]
pub struct TransmissionJob {
    pub dongle_id: String,
    pub freq_hz: u32,
    pub channel: Option<String>,
    pub channel_label: Option<String>,
    /// Full audio (prebuffer + open frames, fades already applied by the pipeline).
    pub samples: Vec<f32>,
}

/// Handle used by DSP bridges to submit recordings.
pub type RecorderTx = mpsc::UnboundedSender<TransmissionJob>;

pub struct RecorderHandle {
    pub tx: RecorderTx,
    shutdown: mpsc::Sender<()>,
}

impl RecorderHandle {
    /// Spawn the recorder task. Returns a handle whose `tx` the bridges send to.
    pub fn spawn(bus: EventBus, data_dir: PathBuf, transcriber: Arc<dyn Transcriber>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<TransmissionJob>();
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

        let recordings_dir = data_dir.join("recordings");
        let logs_dir = data_dir.join("logs");
        let events_log = logs_dir.join("events.log");
        let transcripts_dir = data_dir.join("transcriptions");
        let _ = std::fs::create_dir_all(&recordings_dir);
        let _ = std::fs::create_dir_all(&logs_dir);

        tokio::spawn(async move {
            let mut counts: HashMap<String, u32> = HashMap::new();
            loop {
                tokio::select! {
                    Some(job) = rx.recv() => { handle_job(&bus, &recordings_dir, &transcripts_dir, &events_log, &mut counts, &transcriber, job).await; }
                    _ = shutdown_rx.recv() => break,
                }
            }
        });

        RecorderHandle {
            tx,
            shutdown: shutdown_tx,
        }
    }

    pub async fn shutdown(self) {
        let _ = self.shutdown.send(()).await;
    }
}

async fn handle_job(
    bus: &EventBus,
    recordings_dir: &Path,
    transcripts_dir: &Path,
    events_log: &Path,
    counts: &mut HashMap<String, u32>,
    transcriber: &Arc<dyn Transcriber>,
    job: TransmissionJob,
) {
    let idx = {
        let e = counts.entry(job.dongle_id.clone()).or_insert(0);
        *e += 1;
        *e
    };
    let dir = recordings_dir.to_path_buf();
    // Capture metadata up front; `job` is moved into the blocking closure.
    let samples_len = job.samples.len();
    let duration_sec = samples_len as f32 / hertz_dsp::pipeline::AUDIO_SAMPLE_RATE as f32;
    let dongle_id = job.dongle_id.clone();
    let channel = job.channel.clone();
    let freq_hz_u64 = job.freq_hz as u64;
    let res = tokio::task::spawn_blocking(move || {
        let channel_num: Option<u32> = job.channel.as_deref().and_then(|c| c.parse::<u32>().ok());
        hertz_dsp::recorder::write_recording(
            &job.samples,
            job.freq_hz,
            channel_num,
            job.channel_label.as_deref(),
            idx,
            &dir,
        )
    })
    .await;

    match res {
        Ok(Ok(path)) => {
            let filename = path
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default();
            append_event_line(
                events_log,
                &format!(
                    "RECORDING_SAVED | {} | {:?} | {:.3} s",
                    dongle_id, path, duration_sec
                ),
            );
            bus.publish_event(Event::RecordingSaved {
                dongle_id: dongle_id.clone(),
                channel: channel.clone(),
                freq_hz: freq_hz_u64,
                filename: filename.clone(),
                filepath: path.to_string_lossy().to_string(),
                duration_sec,
            });

            // Transcribe off the recorder task: a slow engine must not delay saving
            // the next transmission.
            tokio::spawn(transcribe_job(
                bus.clone(),
                Arc::clone(transcriber),
                transcripts_dir.to_path_buf(),
                TranscribeMeta {
                    path,
                    dongle_id,
                    channel,
                    freq_hz: freq_hz_u64,
                    duration_sec,
                    live: true,
                },
            ));
        }
        Ok(Err(e)) => error!("recorder write failed: {e}"),
        Err(e) => error!("recorder task panicked: {e}"),
    }
}

fn append_event_line(path: &Path, line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
        let _ = f.flush();
    }
}

/// What a transcript file and event need to know about its recording.
struct TranscribeMeta {
    path: PathBuf,
    dongle_id: String,
    channel: Option<String>,
    freq_hz: u64,
    duration_sec: f32,
    /// A transmission that just happened (publishes a live event). Backfilled
    /// recordings only write their transcript file, so restarts don't replay old
    /// calls to listeners like the Hyperia relay.
    live: bool,
}

/// Transcribe one recording, write `transcriptions/<recording>.txt`, publish the event.
async fn transcribe_job(
    bus: EventBus,
    transcriber: Arc<dyn Transcriber>,
    transcripts_dir: PathBuf,
    meta: TranscribeMeta,
) {
    let wav = meta.path.clone();
    let engine = transcriber.name();
    let started = std::time::Instant::now();
    let text = match tokio::task::spawn_blocking(move || transcriber.transcribe(&wav)).await {
        Ok(Ok(text)) => text.trim().to_string(),
        Ok(Err(e)) => {
            warn!("transcription failed for {:?}: {e:#}", meta.path);
            return;
        }
        Err(e) => {
            error!("transcription task panicked: {e}");
            return;
        }
    };
    if engine == "disabled" {
        return;
    }
    let stem = meta
        .path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let body = format!(
        "FILE: {}\nCHANNEL: {}\nFREQUENCY: {:.3} MHz\nDURATION: {:.2} s\nENGINE: {engine}\nTRANSCRIBED: {}\n---\n{}\n",
        meta.path.display(),
        meta.channel.as_deref().unwrap_or("-"),
        meta.freq_hz as f64 / 1e6,
        meta.duration_sec,
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
        if text.is_empty() { "(no speech detected)" } else { &text },
    );
    let out = transcripts_dir.join(format!("{stem}.txt"));
    if let Err(e) =
        std::fs::create_dir_all(&transcripts_dir).and_then(|_| std::fs::write(&out, body))
    {
        warn!("write transcript {out:?}: {e}");
    }
    info!(
        "transcribed {stem} in {:.1}s: {}",
        started.elapsed().as_secs_f32(),
        if text.is_empty() {
            "(no speech)"
        } else {
            &text
        }
    );
    if meta.live && !text.is_empty() {
        bus.publish_event(Event::Transcription {
            dongle_id: meta.dongle_id,
            channel: meta.channel,
            freq_hz: meta.freq_hz,
            text,
            recording: Some(meta.path.to_string_lossy().to_string()),
        });
    }
}

/// Transcribe every recording under `data_dir/recordings` that has no transcript yet
/// (oldest first). Runs once at startup; transcription itself serializes on the engine.
pub fn spawn_backfill(bus: EventBus, transcriber: Arc<dyn Transcriber>, data_dir: PathBuf) {
    if transcriber.name() == "disabled" {
        return;
    }
    tokio::spawn(async move {
        let recordings_dir = data_dir.join("recordings");
        let transcripts_dir = data_dir.join("transcriptions");
        let mut pending: Vec<PathBuf> = std::fs::read_dir(&recordings_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "wav"))
            .filter(|p| {
                p.file_stem()
                    .map(|s| {
                        !transcripts_dir
                            .join(format!("{}.txt", s.to_string_lossy()))
                            .exists()
                    })
                    .unwrap_or(false)
            })
            .collect();
        pending.sort();
        if pending.is_empty() {
            return;
        }
        info!(
            "transcribing {} existing recording(s) without transcripts",
            pending.len()
        );
        for path in pending {
            let meta = meta_from_filename(path);
            transcribe_job(
                bus.clone(),
                Arc::clone(&transcriber),
                transcripts_dir.clone(),
                meta,
            )
            .await;
        }
    });
}

/// Recover channel/frequency/duration for an existing recording from its name
/// (`transmission_<date>_<time>_Ch71_<label>_156.575MHz_2.wav`) and WAV header.
fn meta_from_filename(path: PathBuf) -> TranscribeMeta {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let channel = stem
        .split('_')
        .find_map(|p| p.strip_prefix("Ch"))
        .map(str::to_string);
    let freq_hz = stem
        .split('_')
        .find_map(|p| p.strip_suffix("MHz"))
        .and_then(|m| m.parse::<f64>().ok())
        .map(|mhz| (mhz * 1e6).round() as u64)
        .unwrap_or(0);
    let duration_sec = hound::WavReader::open(&path)
        .map(|r| r.duration() as f32 / r.spec().sample_rate.max(1) as f32)
        .unwrap_or(0.0);
    TranscribeMeta {
        path,
        dongle_id: String::new(),
        channel,
        freq_hz,
        duration_sec,
        live: false,
    }
}
