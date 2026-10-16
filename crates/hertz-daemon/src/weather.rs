//! Weather sideband. A scanning dongle with `weather` set steps off its scan every
//! so often, records a short sample of each NOAA weather channel (the scan loop in
//! pipeline_bridge does the tuning), and hands the audio here. This worker writes it
//! to `data/weather/<channel>.wav` (the latest sample per channel), transcribes it
//! off the DSP thread, and publishes `Event::Weather`, which Lume, the TUI and the
//! console pick up.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender};
use hertz_dsp::pipeline::AUDIO_SAMPLE_RATE;
use hertz_types::{Event, WeatherConfig};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::bus::EventBus;
use crate::transcribe::Transcriber;

/// Demodulated audio from one weather channel (48 kHz mono).
pub struct WeatherSample {
    pub dongle_id: String,
    pub channel: String,
    pub freq_hz: u64,
    pub samples: Vec<f32>,
    /// Mean signal level while sampling (dB, same scale as the squelch).
    pub signal_db: Option<f32>,
}

pub type WeatherTx = Sender<WeatherSample>;

/// When the live transcriber last heard a weather channel begin its "Weather Radio
/// Roundup" (unix ms; 0 = never). The roundup is the town-by-town current
/// conditions; a sample that ends partway through it loses the last towns (New
/// Braunfels was cut off after Schertz), so the sampler runs on until it's done.
static ROUNDUP_STARTED_MS: AtomicU64 = AtomicU64::new(0);
/// Roughly how long a roundup takes to read.
pub const ROUNDUP_LENGTH: Duration = Duration::from_secs(75);

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Called by live transcription when a weather broadcast starts its roundup.
pub fn roundup_started() {
    if roundup_left().is_none() {
        info!("WEATHER | roundup started; the sample runs on until it's read");
    }
    ROUNDUP_STARTED_MS.store(unix_ms(), Ordering::Relaxed);
}

/// Forget any earlier roundup (at the start of a sample).
pub fn roundup_reset() {
    ROUNDUP_STARTED_MS.store(0, Ordering::Relaxed);
}

/// How much of a roundup heard recently is still to come, if one is under way.
pub fn roundup_left() -> Option<Duration> {
    let started = ROUNDUP_STARTED_MS.load(Ordering::Relaxed);
    if started == 0 {
        return None;
    }
    let elapsed = Duration::from_millis(unix_ms().saturating_sub(started));
    ROUNDUP_LENGTH.checked_sub(elapsed).filter(|d| !d.is_zero())
}

/// The latest transcribed sample per weather channel, for the `weather_latest`
/// tool. One weather worker runs per daemon, so a process-global store is correct
/// (the same shape as [`ROUNDUP_STARTED_MS`]).
#[derive(Clone, Debug, Serialize)]
pub struct WeatherLatest {
    pub channel: String,
    pub freq_hz: u64,
    pub transcript: String,
    pub signal_db: Option<f32>,
    pub unix_ms: u64,
    pub wav: String,
}

static LATEST: LazyLock<Mutex<HashMap<String, WeatherLatest>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Record a finished sample (called by the weather worker).
pub fn record_latest(l: WeatherLatest) {
    LATEST.lock().unwrap().insert(l.channel.clone(), l);
}

/// The latest sample per channel, newest-first by `unix_ms`.
pub fn latest_samples() -> Vec<WeatherLatest> {
    let mut v: Vec<WeatherLatest> = LATEST.lock().unwrap().values().cloned().collect();
    v.sort_by(|a, b| b.unix_ms.cmp(&a.unix_ms));
    v
}

/// Clear the latest samples (tests).
#[cfg(test)]
pub fn latest_reset() {
    LATEST.lock().unwrap().clear();
}

/// Weather settings shared by the scan loops (readers) and MCP / REST (writers):
/// turn sampling on and off and change channels or interval while hertzd runs.
/// One set for every scanning dongle, like the `--weather` flag.
#[derive(Clone)]
pub struct WeatherControl(Arc<Mutex<WeatherState>>);

#[derive(Clone, Debug)]
struct WeatherState {
    settings: WeatherSettings,
    /// Bumped on every change; a scan loop seeing a new epoch reschedules.
    epoch: u64,
    /// Scan loops that can sample (scanning monitor dongles).
    dongles: usize,
}

/// What `weather_status` reports.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct WeatherSettings {
    pub enabled: bool,
    pub channels: Vec<String>,
    pub every_minutes: u64,
    pub sample_seconds: u64,
    /// Whether a sample is being taken now (receive is offline).
    pub sampling: bool,
    /// When the next sample is due (unix ms), while on.
    pub next_sample_unix_ms: Option<u64>,
}

/// A change from `weather_set`; fields left out keep their value.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct WeatherChange {
    pub enabled: bool,
    pub channels: Option<Vec<String>>,
    pub every_minutes: Option<u64>,
}

impl WeatherControl {
    pub fn new(cfg: Option<&WeatherConfig>) -> Self {
        let cfg = cfg.cloned().unwrap_or(WeatherConfig {
            channels: vec!["WX1".into()],
            every_minutes: 15,
            sample_seconds: 180,
            first_after_seconds: 0,
            enabled: false,
        });
        Self(Arc::new(Mutex::new(WeatherState {
            settings: WeatherSettings {
                enabled: cfg.enabled,
                channels: cfg.channels,
                every_minutes: cfg.every_minutes.max(1),
                sample_seconds: cfg.sample_seconds.max(1),
                sampling: false,
                next_sample_unix_ms: None,
            },
            epoch: 0,
            dongles: 0,
        })))
    }

    pub fn get(&self) -> WeatherSettings {
        self.0.lock().unwrap().settings.clone()
    }

    /// Scanning dongles that take weather samples.
    pub fn dongles(&self) -> usize {
        self.0.lock().unwrap().dongles
    }

    pub(crate) fn add_dongle(&self) {
        self.0.lock().unwrap().dongles += 1;
    }

    /// The settings and their epoch, for a scan loop.
    pub(crate) fn snapshot(&self) -> (WeatherSettings, u64) {
        let g = self.0.lock().unwrap();
        (g.settings.clone(), g.epoch)
    }

    /// Apply a change. Turning it on (or changing it while on) samples right away.
    pub fn set(&self, change: WeatherChange) -> WeatherSettings {
        let mut g = self.0.lock().unwrap();
        let s = &mut g.settings;
        s.enabled = change.enabled;
        if let Some(c) = change.channels.filter(|c| !c.is_empty()) {
            s.channels = c.iter().map(|c| c.trim().to_uppercase()).collect();
        }
        if let Some(m) = change.every_minutes {
            s.every_minutes = m.max(1);
        }
        if !s.enabled {
            s.next_sample_unix_ms = None;
        }
        g.epoch += 1;
        info!(
            "weather: turned {} ({}, 3 min every {} min)",
            if g.settings.enabled { "on" } else { "off" },
            g.settings.channels.join(","),
            g.settings.every_minutes
        );
        g.settings.clone()
    }

    /// A scan loop reports when it will sample next, and whether it is sampling.
    pub(crate) fn report(&self, next: Option<Duration>, sampling: bool) {
        let mut g = self.0.lock().unwrap();
        g.settings.sampling = sampling;
        g.settings.next_sample_unix_ms = next
            .filter(|_| g.settings.enabled)
            .map(|d| unix_ms() + d.as_millis() as u64);
    }

    /// Whether the settings changed since `epoch` and sampling is now off.
    pub(crate) fn turned_off_since(&self, epoch: u64) -> bool {
        let g = self.0.lock().unwrap();
        g.epoch != epoch && !g.settings.enabled
    }
}

/// Audio RMS above this is open-squelch static, not a station: an FM carrier quiets
/// the noise. Measured on this receiver: received calls 0.05-0.13, no carrier 0.65.
/// Transcribing static makes Whistle invent words ("Thank you."), so it's skipped.
const STATIC_RMS: f32 = 0.35;
/// ...unless the channel carries real power: static read -21 to -23 dB here, the
/// Seguin station on WX3 -12.4 dB with noisy audio (rms 0.56) you can still follow.
const STATION_DB: f32 = -17.0;

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

/// Start the worker; the scan loop sends samples to the returned sender. Samples
/// that arrive while one is being transcribed wait (the queue holds a few).
pub fn spawn(bus: EventBus, transcriber: Arc<dyn Transcriber>, data_dir: PathBuf) -> WeatherTx {
    let (tx, rx) = crossbeam_channel::bounded(8);
    std::thread::Builder::new()
        .name("weather".into())
        .spawn(move || run(rx, bus, transcriber, data_dir.join("weather")))
        .expect("spawn weather worker");
    tx
}

fn run(
    rx: Receiver<WeatherSample>,
    bus: EventBus,
    transcriber: Arc<dyn Transcriber>,
    dir: PathBuf,
) {
    for sample in rx {
        let seconds = sample.samples.len() as f32 / AUDIO_SAMPLE_RATE as f32;
        let level = sample
            .signal_db
            .map(|db| format!("signal {db:.1} dB"))
            .unwrap_or_else(|| "no signal reading".into());
        let audio_rms = rms(&sample.samples);
        let has_station = sample.signal_db.is_some_and(|db| db > STATION_DB);
        if audio_rms > STATIC_RMS && !has_station {
            info!(
                "WEATHER | {} | {:.3} MHz | {level} | no carrier (static, audio rms {audio_rms:.2})",
                sample.channel,
                sample.freq_hz as f64 / 1e6
            );
            continue;
        }
        let path = dir.join(format!("{}.wav", sample.channel));
        if let Err(e) = write_wav(&path, &sample.samples) {
            warn!("WEATHER | {} | can't write {path:?}: {e:#}", sample.channel);
            continue;
        }
        let text = match transcriber.transcribe(&path) {
            Ok(t) => t.trim().to_string(),
            Err(e) => {
                warn!("WEATHER | {} | transcription failed: {e:#}", sample.channel);
                continue;
            }
        };
        if text.is_empty() {
            info!(
                "WEATHER | {} | {:.3} MHz | {level} | no speech heard in {seconds:.0} s",
                sample.channel,
                sample.freq_hz as f64 / 1e6
            );
            continue;
        }
        info!(
            "WEATHER | {} | {:.3} MHz | {level} | {seconds:.0} s: {text}",
            sample.channel,
            sample.freq_hz as f64 / 1e6
        );
        let wav = path.to_string_lossy().into_owned();
        record_latest(WeatherLatest {
            channel: sample.channel.clone(),
            freq_hz: sample.freq_hz,
            transcript: text.clone(),
            signal_db: sample.signal_db,
            unix_ms: unix_ms(),
            wav: wav.clone(),
        });
        bus.publish_event(Event::Weather {
            dongle_id: sample.dongle_id,
            channel: sample.channel,
            freq_hz: sample.freq_hz,
            text,
            seconds,
            signal_db: sample.signal_db,
            recording: Some(wav),
        });
    }
}

fn write_wav(path: &std::path::Path, samples: &[f32]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: AUDIO_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for &s in samples {
        w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)?;
    }
    w.finalize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    struct Fixed(&'static str);
    impl Transcriber for Fixed {
        fn transcribe(&self, wav: &Path) -> anyhow::Result<String> {
            assert!(wav.exists());
            Ok(self.0.to_string())
        }
        fn name(&self) -> &'static str {
            "fixed"
        }
    }

    #[tokio::test]
    async fn static_is_not_transcribed() {
        let bus = EventBus::new();
        let mut events = bus.subscribe_events();
        let dir = tempfile::tempdir().unwrap();
        let tx = spawn(
            bus.clone(),
            Arc::new(Fixed("Thank you.")),
            dir.path().into(),
        );
        let sample = |channel: &str, level: f32| WeatherSample {
            dongle_id: "d".into(),
            channel: channel.into(),
            freq_hz: 162_550_000,
            samples: (0..48_000)
                .map(|i| if i % 2 == 0 { level } else { -level })
                .collect(),
            signal_db: Some(-21.0),
        };
        tx.send(sample("WX1", 0.65)).unwrap(); // static
        tx.send(sample("WX2", 0.1)).unwrap(); // a station
        let ev = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        let Event::Weather { channel, .. } = ev else {
            panic!("expected Weather, got {ev:?}");
        };
        assert_eq!(channel, "WX2", "the static sample made no event");
    }

    #[tokio::test]
    async fn sample_becomes_a_weather_event() {
        let bus = EventBus::new();
        let mut events = bus.subscribe_events();
        let dir = tempfile::tempdir().unwrap();
        let tx = spawn(
            bus.clone(),
            Arc::new(Fixed("Small craft advisory until 6 PM.")),
            dir.path().into(),
        );
        tx.send(WeatherSample {
            dongle_id: "d".into(),
            channel: "WX1".into(),
            freq_hz: 162_550_000,
            samples: vec![0.0; AUDIO_SAMPLE_RATE as usize],
            signal_db: Some(-3.0),
        })
        .unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        let Event::Weather {
            channel,
            text,
            seconds,
            recording,
            ..
        } = ev
        else {
            panic!("expected Weather, got {ev:?}");
        };
        assert_eq!(
            (channel.as_str(), text.as_str()),
            ("WX1", "Small craft advisory until 6 PM.")
        );
        assert!((seconds - 1.0).abs() < 0.01);
        assert!(recording.unwrap().ends_with("WX1.wav"));
        assert!(dir.path().join("weather").join("WX1.wav").exists());
    }

    #[test]
    fn latest_samples_are_newest_first_per_channel() {
        latest_reset();
        assert!(latest_samples().is_empty());
        record_latest(WeatherLatest {
            channel: "WX1".into(),
            freq_hz: 162_550_000,
            transcript: "first".into(),
            signal_db: Some(-3.0),
            unix_ms: 1000,
            wav: "/w/WX1.wav".into(),
        });
        record_latest(WeatherLatest {
            channel: "WX2".into(),
            freq_hz: 162_400_000,
            transcript: "second".into(),
            signal_db: None,
            unix_ms: 2000,
            wav: "/w/WX2.wav".into(),
        });
        record_latest(WeatherLatest {
            channel: "WX1".into(),
            freq_hz: 162_550_000,
            transcript: "third".into(),
            signal_db: Some(-4.0),
            unix_ms: 3000,
            wav: "/w/WX1.wav".into(),
        });
        let s = latest_samples();
        assert_eq!(s.len(), 2, "WX1 was overwritten by its newer sample");
        assert_eq!(s[0].channel, "WX1", "newest first");
        assert_eq!(s[0].transcript, "third");
        assert_eq!(s[1].channel, "WX2");
        latest_reset();
    }
}

#[cfg(test)]
mod roundup_tests {
    use super::*;

    #[test]
    fn a_roundup_holds_the_sample_open_for_its_length() {
        roundup_reset();
        assert_eq!(roundup_left(), None);
        roundup_started();
        let left = roundup_left().expect("under way");
        assert!(left <= ROUNDUP_LENGTH && left > ROUNDUP_LENGTH - Duration::from_secs(2));
        roundup_reset();
        assert_eq!(roundup_left(), None);
    }
}
