//! The DSP pipeline — ported from `TransmissionState` in
//! `plan/reference/gnosis-radio/src/pipeline/state.rs`.
//!
//! This is the crown-jewels entropy squelch with hang counter, voice gate, fades,
//! prebuffer, AFC, and noise-floor adaptation. The gnosis pipeline printed, logged,
//! streamed, and recorded inline; this port instead emits [`PipelineEvent`] values
//! from [`Pipeline::process_buffer`]. No println!, no file I/O, no TcpStream — the
//! daemon (another crate) wires events to its bus, recorder, and logs.

use std::collections::VecDeque;

use num_complex::Complex32;

use crate::analysis::{ClickSuppressor, FrequencyEstimator, SignalDetector, SpectralSquelch};
use crate::classify::SignalClassification;
use crate::demod::{DemodImpl, Mode};
use crate::entropy::EntropyPool;

// ---- Sacred, field-tuned constants (carried verbatim from gnosis) --------

/// Narrowband SDR input rate (post-channelizer).
pub const SDR_RATE: u32 = 240_000;
/// gnosis per-buffer size: 48 000 IQ pairs = 200 ms at 240 kS/s.
pub const BUFFER_SIZE: usize = 48_000;
/// First-stage decimation (240 kHz → 48 kHz audio).
pub const DECIMATION_1: usize = 5;
/// Audio sample rate.
pub const AUDIO_SAMPLE_RATE: u32 = SDR_RATE / DECIMATION_1 as u32; // 48 000

/// Seconds of demod audio kept while squelch is closed (drained on open).
pub const PREBUFFER_SECONDS: f32 = 1.5;

/// Hang counter in frames (~2 s at 200 ms/frame).
pub const SQUELCH_HANG_FRAMES: u64 = 10;
/// Squared fade-in ramp length (~100 ms at 48 kHz).
pub const FADE_IN_SAMPLES: usize = 4800;
/// Max frames to wait for voice before passing audio.
pub const VOICE_GATE_MAX_FRAMES: u64 = 3;

/// Flatness EMA coefficient (gnosis 0.85/0.15).
pub const FLATNESS_EMA_ALPHA: f32 = 0.85;
/// Open squelch when flatness drops below this.
pub const FLATNESS_OPEN: f32 = 0.55;
/// "Definitely noise" when flatness rises above this.
pub const FLATNESS_NOISE: f32 = 0.70;

/// AFC smoothing / threshold / max step (Hz). Carried from gnosis.
pub const AFC_SMOOTHING: f32 = 0.85;
pub const AFC_THRESHOLD_HZ: f32 = 0.5;
pub const AFC_MAX_STEP_HZ: f32 = 5000.0;

/// Default squelch margin (dB above noise floor). Monitor mode default.
pub const DEFAULT_SQUELCH_MARGIN_MONITOR_DB: f32 = 6.0;
/// Wideband/scan mode default (gnosis).
pub const DEFAULT_SQUELCH_MARGIN_SCAN_DB: f32 = 12.0;

/// Seed noise floor for channelized pipelines (post channel-FIR).
pub const SEED_NOISE_FLOOR_DB: f32 = -32.0;

// ---- Events --------------------------------------------------------------

/// What kind of audio a [`PipelineEvent::Audio`] carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioKind {
    /// Drained prebuffer at squelch-open (captures pre-trigger onset).
    Prebuffer,
    /// Normal squelch-open transmission audio.
    Transmission,
}

/// Summary of a completed transmission, emitted with
/// [`PipelineEvent::TransmissionComplete`].
#[derive(Clone, Debug)]
pub struct TransmissionSummary {
    /// Full audio (prebuffer + all squelch-open frames, fades applied).
    pub samples: Vec<f32>,
    pub signal_db: f32,
    pub classification: SignalClassification,
}

/// One atomic observation from the pipeline. The daemon wires these to its event bus,
/// recorder (on `TransmissionComplete`), and logs. Returned in order from
/// [`Pipeline::process_buffer`].
#[derive(Clone, Debug)]
pub enum PipelineEvent {
    /// Squelch just opened on a structured signal.
    SquelchOpened {
        signal_db: f32,
        classification: SignalClassification,
        flatness: f32,
    },
    /// Squelch just closed (hang expired).
    SquelchClosed {
        signal_db: f32,
        noise_floor: f32,
        classification: SignalClassification,
    },
    /// Demodulated audio. The first `Audio` event after [`Self::SquelchOpened`] is
    /// the drained prebuffer (`kind == Prebuffer`); subsequent events are
    /// transmission audio.
    Audio {
        samples: Vec<f32>,
        signal_db: f32,
        kind: AudioKind,
    },
    /// A full transmission finished — the daemon typically records this.
    TransmissionComplete(TransmissionSummary),
    /// Always-on monitor-tap audio (raw demod, static included) emitted when
    /// `always_stream` is set and squelch is closed, so UIs can graph the live dongle
    /// output continuously.
    MonitorTap { samples: Vec<f32>, signal_db: f32 },
    /// Periodic level metering (every 2 frames in gnosis).
    SignalLevel {
        signal_db: f32,
        noise_floor: f32,
        squelch_open: bool,
        flatness: f32,
    },
}

// ---- Config --------------------------------------------------------------

/// Configuration for [`Pipeline`].
#[derive(Clone, Debug)]
pub struct PipelineConfig {
    pub mode: Mode,
    /// Per-channel IQ input rate (post-channelizer); default [`SDR_RATE`].
    pub input_sample_rate_hz: u32,
    /// Squelch margin in dB above the noise floor.
    pub squelch_margin_db: f32,
    /// Initial noise floor (dB) — refined adaptively afterwards.
    pub initial_noise_floor_db: f32,
    /// Seconds of demod audio kept while squelch is closed.
    pub prebuffer_seconds: f32,
    /// When true, emit [`PipelineEvent::MonitorTap`] for squelch-closed frames too
    /// (gnosis always-on tap channel).
    pub always_stream: bool,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Nfm,
            input_sample_rate_hz: SDR_RATE,
            squelch_margin_db: DEFAULT_SQUELCH_MARGIN_MONITOR_DB,
            initial_noise_floor_db: SEED_NOISE_FLOOR_DB,
            prebuffer_seconds: PREBUFFER_SECONDS,
            always_stream: false,
        }
    }
}

// ---- Pipeline ------------------------------------------------------------

pub struct Pipeline {
    state: TransmissionState,
}

impl Pipeline {
    pub fn new(config: PipelineConfig) -> Self {
        Self {
            state: TransmissionState::new(config),
        }
    }

    /// Shorthand for an NFM monitor-mode pipeline at the gnosis defaults.
    pub fn nfm_monitor() -> Self {
        Self::new(PipelineConfig {
            mode: Mode::Nfm,
            squelch_margin_db: DEFAULT_SQUELCH_MARGIN_MONITOR_DB,
            ..PipelineConfig::default()
        })
    }

    /// Reset all state (demod history, prebuffer, squelch) — e.g. on channel change.
    pub fn reset(&mut self) {
        self.state.reset();
    }

    /// Whether the squelch is currently open.
    pub fn is_squelch_open(&self) -> bool {
        self.state.squelch_open
    }

    /// Current noise-floor estimate (dB).
    pub fn noise_floor_db(&self) -> f32 {
        self.state.noise_floor
    }

    /// The active demodulation mode.
    pub fn mode(&self) -> Mode {
        self.state.mode
    }

    /// Drain up to `max_bytes` of harvested RF-noise entropy.
    pub fn drain_entropy(&mut self, max_bytes: usize) -> Vec<u8> {
        self.state.entropy.drain(max_bytes)
    }

    /// Process a buffer of complex baseband IQ and return the events it produced.
    /// This is the hot path — pure, allocation-bounded to the events vector.
    pub fn process_buffer(&mut self, iq: &[Complex32]) -> Vec<PipelineEvent> {
        self.state.process(iq)
    }
}

struct TransmissionState {
    mode: Mode,
    demod: DemodImpl,
    signal_detector: SignalDetector,
    spectral_squelch: SpectralSquelch,
    click_suppressor: ClickSuppressor,
    freq_estimator: FrequencyEstimator,
    entropy: EntropyPool,
    prebuffer: VecDeque<f32>,
    transmission_buffer: Vec<f32>,
    squelch_open: bool,
    squelch_hang_counter: u64,
    last_signal_class: SignalClassification,
    afc_error_avg: f32,
    frame_count: u64,
    prebuffer_capacity: usize,
    frames_since_squelch_open: u64,
    voice_detected_this_transmission: bool,
    recording_stopped: bool,
    audio_flatness_ema: f32,
    noise_floor: f32,
    squelch_thresh: f32,
    squelch_margin: f32,
    always_stream: bool,
}

impl TransmissionState {
    fn new(config: PipelineConfig) -> Self {
        let demod = match config.mode {
            Mode::Nfm => DemodImpl::Nfm(crate::demod::NfmDemod::new(config.input_sample_rate_hz)),
            Mode::Am => DemodImpl::Am(crate::demod::AmDemod::new(config.input_sample_rate_hz)),
        };
        let prebuffer_capacity =
            ((AUDIO_SAMPLE_RATE as f32) * config.prebuffer_seconds).round() as usize;
        let prebuffer_capacity = prebuffer_capacity.max(1);
        let noise_floor = config.initial_noise_floor_db;
        Self {
            mode: config.mode,
            demod,
            signal_detector: SignalDetector::new(AUDIO_SAMPLE_RATE as f32),
            spectral_squelch: SpectralSquelch::new(),
            click_suppressor: ClickSuppressor::new(0.3),
            freq_estimator: FrequencyEstimator::new(config.input_sample_rate_hz as f32),
            entropy: EntropyPool::new(),
            prebuffer: VecDeque::with_capacity(prebuffer_capacity.max(1)),
            transmission_buffer: Vec::new(),
            squelch_open: false,
            squelch_hang_counter: 0,
            last_signal_class: SignalClassification::Static,
            afc_error_avg: 0.0,
            frame_count: 0,
            prebuffer_capacity,
            frames_since_squelch_open: 0,
            voice_detected_this_transmission: false,
            recording_stopped: false,
            audio_flatness_ema: 0.8, // start assuming noise
            noise_floor,
            squelch_thresh: noise_floor + config.squelch_margin_db,
            squelch_margin: config.squelch_margin_db,
            always_stream: config.always_stream,
        }
    }

    fn reset(&mut self) {
        // Rebuild the demod to reset filter history + AFC rotator phase.
        let rate = match self.mode {
            Mode::Nfm => SDR_RATE,
            Mode::Am => SDR_RATE,
        };
        let _ = rate;
        self.demod = match self.mode {
            Mode::Nfm => DemodImpl::Nfm(crate::demod::NfmDemod::new(SDR_RATE)),
            Mode::Am => DemodImpl::Am(crate::demod::AmDemod::new(SDR_RATE)),
        };
        self.freq_estimator.reset();
        self.afc_error_avg = 0.0;
        self.squelch_open = false;
        self.squelch_hang_counter = 0;
        self.prebuffer.clear();
        self.transmission_buffer.clear();
        self.frames_since_squelch_open = 0;
        self.voice_detected_this_transmission = false;
        self.recording_stopped = false;
        self.audio_flatness_ema = 0.8;
    }

    /// Spectral flatness of demodulated audio in the vocal range (300–3400 Hz).
    fn compute_audio_flatness(&mut self, audio: &[f32]) -> f32 {
        if audio.len() < 256 {
            return 0.5;
        }
        let audio_complex: Vec<Complex32> = audio
            .iter()
            .take(2048)
            .map(|&s| Complex32::new(s, 0.0))
            .collect();
        let (_, mags) = self.signal_detector.power_spectrum(&audio_complex);
        let bin_width = AUDIO_SAMPLE_RATE as f32 / 2048.0;
        let lo_bin = (300.0 / bin_width).floor() as usize;
        let hi_bin = (3400.0 / bin_width).ceil() as usize;
        let hi_bin = hi_bin.min(mags.len());
        if lo_bin >= hi_bin {
            return 0.5;
        }
        self.spectral_squelch
            .spectral_flatness(&mags[lo_bin..hi_bin])
    }

    fn process(&mut self, iq: &[Complex32]) -> Vec<PipelineEvent> {
        let mut events = Vec::new();
        self.frame_count += 1;

        // === Demodulate. signal_db is IN-CHANNEL power (post channel filter). ===
        let (mut audio, signal_db) = self.demod.process(iq);
        self.click_suppressor.process(&mut audio);

        // === Entropy-based squelch metric ===
        let audio_flatness = self.compute_audio_flatness(&audio);
        self.audio_flatness_ema = FLATNESS_EMA_ALPHA * self.audio_flatness_ema
            + (1.0 - FLATNESS_EMA_ALPHA) * audio_flatness;
        let flatness = self.audio_flatness_ema;

        // AM-vs-NFM: a pure AM carrier demodulates to silence (flat envelope →
        // DC-blocked away), so its demod-audio flatness reads ~1.0 ("noise-like")
        // and a flatness-only squelch would never open on it. For AM we also treat
        // strong carrier power (signal_db above threshold) as a signal; NFM stays
        // flatness-only, exactly as gnosis (a pure NFM carrier is an edge case
        // gnosis's non-viz path likewise left as Static).
        let signal_present = if self.mode == Mode::Am {
            flatness < FLATNESS_OPEN || signal_db > self.squelch_thresh
        } else {
            flatness < FLATNESS_OPEN
        };
        let is_noise = if self.mode == Mode::Am {
            flatness > FLATNESS_NOISE && signal_db <= self.squelch_thresh
        } else {
            flatness > FLATNESS_NOISE
        };

        // === Entropy harvest (LSBs of demod are random from ADC quantization). ===
        self.entropy.harvest(&audio);

        // === Classification (non-viz path). For AM the `signal_present` test above
        // already includes strong-carrier power, so a pure AM carrier (silent demod)
        // is classified Carrier here; NFM stays flatness-driven exactly as gnosis.
        self.last_signal_class = if flatness < 0.45 {
            SignalClassification::Voice
        } else if signal_present {
            SignalClassification::Carrier
        } else {
            SignalClassification::Static
        };

        // === Noise-floor adaptation ===
        if is_noise {
            let alpha = if self.squelch_open { 0.05 } else { 0.15 };
            self.noise_floor = (1.0 - alpha) * self.noise_floor + alpha * signal_db;
            self.squelch_thresh = self.noise_floor + self.squelch_margin;
        } else if !signal_present && signal_db < self.squelch_thresh {
            let alpha = 0.02;
            self.noise_floor = (1.0 - alpha) * self.noise_floor + alpha * signal_db;
            self.squelch_thresh = self.noise_floor + self.squelch_margin;
        }

        // === Squelch state machine ===
        if self.squelch_open {
            self.frames_since_squelch_open += 1;
            if self.last_signal_class == SignalClassification::Voice {
                self.voice_detected_this_transmission = true;
            }
            // Hang counter: structured signal holds; noise counts down ×2; else −1.
            if signal_present {
                self.squelch_hang_counter = SQUELCH_HANG_FRAMES;
            } else if is_noise {
                if self.squelch_hang_counter > 2 {
                    self.squelch_hang_counter -= 2;
                } else {
                    self.squelch_hang_counter = self.squelch_hang_counter.saturating_sub(1);
                }
            } else if self.squelch_hang_counter > 0 {
                self.squelch_hang_counter -= 1;
            }
            // Recording-stop-in-hang-tail logic.
            if self.voice_detected_this_transmission {
                if !signal_present && self.squelch_hang_counter < 7 {
                    self.recording_stopped = true;
                }
                if flatness < 0.45 {
                    self.recording_stopped = false;
                }
            }
            if self.squelch_hang_counter == 0 {
                // CLOSE.
                let classification = self.last_signal_class;
                let noise_floor = self.noise_floor;
                events.push(PipelineEvent::SquelchClosed {
                    signal_db,
                    noise_floor,
                    classification,
                });
                if !self.transmission_buffer.is_empty() {
                    events.push(PipelineEvent::TransmissionComplete(TransmissionSummary {
                        samples: std::mem::take(&mut self.transmission_buffer),
                        signal_db,
                        classification,
                    }));
                }
                self.squelch_open = false;
                self.frames_since_squelch_open = 0;
                self.voice_detected_this_transmission = false;
                self.recording_stopped = false;
            }
        } else if signal_present {
            // OPEN.
            self.squelch_open = true;
            self.squelch_hang_counter = SQUELCH_HANG_FRAMES;
            self.frames_since_squelch_open = 0;
            self.voice_detected_this_transmission = false;
            self.recording_stopped = false;
            self.transmission_buffer.clear();
            events.push(PipelineEvent::SquelchOpened {
                signal_db,
                classification: self.last_signal_class,
                flatness,
            });
            // Drain prebuffer (with squared fade-in) into the transmission + first Audio.
            if !self.prebuffer.is_empty() {
                let mut pre: Vec<f32> = self.prebuffer.drain(..).collect();
                let fade_len = pre.len().min(FADE_IN_SAMPLES);
                for (i, s) in pre.iter_mut().enumerate().take(fade_len) {
                    let g = i as f32 / fade_len as f32;
                    *s *= g * g;
                }
                self.transmission_buffer.extend_from_slice(&pre);
                events.push(PipelineEvent::Audio {
                    samples: pre,
                    signal_db,
                    kind: AudioKind::Prebuffer,
                });
            }
        }

        // === Prebuffer fill while closed ===
        if !self.squelch_open {
            for &s in &audio {
                if self.prebuffer.len() >= self.prebuffer_capacity {
                    self.prebuffer.pop_front();
                }
                self.prebuffer.push_back(s);
            }
        }

        // === Output gating (voice gate + hang-tail fade) ===
        let past_voice_gate = self.frames_since_squelch_open <= 1
            || self.voice_detected_this_transmission
            || self.frames_since_squelch_open > VOICE_GATE_MAX_FRAMES;
        let in_hang_tail = self.voice_detected_this_transmission
            && self.squelch_hang_counter < SQUELCH_HANG_FRAMES
            && self.squelch_hang_counter > 0
            && !signal_present
            && self.squelch_hang_counter < 7;
        let should_output = self.squelch_open && !is_noise && past_voice_gate && !in_hang_tail;

        if should_output {
            // Squared fade-in ramp on the first frames after open.
            if self.frames_since_squelch_open <= 2 {
                let base = if self.frames_since_squelch_open <= 1 {
                    0
                } else {
                    audio.len()
                };
                for (i, s) in audio.iter_mut().enumerate() {
                    let total_i = base + i;
                    if total_i < FADE_IN_SAMPLES {
                        let gain = total_i as f32 / FADE_IN_SAMPLES as f32;
                        *s *= gain * gain;
                    }
                }
            }
            // Fade-out approaching close.
            if self.squelch_hang_counter <= 3 && self.squelch_hang_counter > 0 {
                let fade_factor = self.squelch_hang_counter as f32 / 3.0;
                let audio_len = audio.len() as f32;
                for (i, s) in audio.iter_mut().enumerate() {
                    let ramp = fade_factor * (1.0 - i as f32 / audio_len.max(1.0));
                    *s *= ramp.max(0.0);
                }
            }
            if !self.recording_stopped {
                self.transmission_buffer.extend_from_slice(&audio);
            }
            events.push(PipelineEvent::Audio {
                samples: audio.clone(),
                signal_db,
                kind: AudioKind::Transmission,
            });
        }

        // === Always-on monitor tap (raw demod, static included) ===
        if self.always_stream && !self.squelch_open {
            events.push(PipelineEvent::MonitorTap {
                samples: audio.clone(),
                signal_db,
            });
        }

        // === AFC (NFM only) ===
        if self.mode == Mode::Nfm {
            if signal_db > self.noise_floor + 1.0
                && self.last_signal_class != SignalClassification::Static
            {
                let mut offset = self.freq_estimator.estimate(iq);
                if !offset.is_finite() {
                    offset = 0.0;
                }
                offset = offset.clamp(-AFC_MAX_STEP_HZ, AFC_MAX_STEP_HZ);
                self.afc_error_avg =
                    AFC_SMOOTHING * self.afc_error_avg + (1.0 - AFC_SMOOTHING) * offset;
            } else {
                self.freq_estimator.reset();
                self.afc_error_avg *= AFC_SMOOTHING;
            }
            if self.afc_error_avg.abs() > AFC_THRESHOLD_HZ {
                self.demod.set_freq_offset(self.afc_error_avg);
            } else if self.afc_error_avg.abs() < AFC_THRESHOLD_HZ * 0.5 {
                self.demod.set_freq_offset(0.0);
            }
        }

        // === Periodic signal-level metering (every 2 frames) ===
        if self.frame_count.is_multiple_of(2) {
            events.push(PipelineEvent::SignalLevel {
                signal_db,
                noise_floor: self.noise_floor,
                squelch_open: self.squelch_open,
                flatness,
            });
        }

        events
    }
}
