//! Recorder task (PLAN §5 / T4 brief Step 2): subscribes to transmission jobs from
//! the DSP bridges and writes a WAV per transmission under `data_dir/recordings/`
//! via `hertz_dsp::recorder`, then emits `RecordingSaved` on the bus and appends a
//! line to `data_dir/logs/events.log`. Transcription is called on each save (Phase 6
//! fills in real engines; today it's a no-op).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use hertz_types::Event;
use tokio::sync::mpsc;
use tracing::error;

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
    pub fn spawn(bus: EventBus, data_dir: PathBuf, transcriber: Box<dyn Transcriber>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<TransmissionJob>();
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

        let recordings_dir = data_dir.join("recordings");
        let logs_dir = data_dir.join("logs");
        let events_log = logs_dir.join("events.log");
        let _ = std::fs::create_dir_all(&recordings_dir);
        let _ = std::fs::create_dir_all(&logs_dir);

        tokio::spawn(async move {
            let mut counts: HashMap<String, u32> = HashMap::new();
            loop {
                tokio::select! {
                    Some(job) = rx.recv() => { handle_job(&bus, &recordings_dir, &events_log, &mut counts, transcriber.as_ref(), job).await; }
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
    events_log: &Path,
    counts: &mut HashMap<String, u32>,
    transcriber: &dyn Transcriber,
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

            // Transcription seam: Phase 6 fills this in. For T4 the registered
            // transcriber is `DisabledTranscriber` (instant no-op), so a direct
            // call here is fine; real engines will wrap their own blocking I/O.
            if let Ok(text) = transcriber.transcribe(&path) {
                if !text.trim().is_empty() {
                    bus.publish_event(Event::Transcription {
                        dongle_id: dongle_id.clone(),
                        channel: channel.clone(),
                        freq_hz: freq_hz_u64,
                        text,
                    });
                }
            }
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
