use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;

use crossbeam_channel::Sender;
use num_complex::Complex32;

use super::recorder;
use super::squelch::{classify_signal, SignalClassification};
use super::streamer::send_stream_samples;
use crate::broadcast::{AudioBroadcaster, AudioMessage};
use crate::dsp::{
    AudioNotchFilter, ClickSuppressor, FrequencyEstimator, ImpulseNoiseFilter, NoiseFloorTracker,
    SignalDetector, SpectralSquelch,
};
use crate::viz::{print_viz, render_spectrogram, render_spectrum, render_waveform, VizConfig};
use crate::{AFC_MAX_STEP_HZ, AFC_SMOOTHING, AFC_THRESHOLD_HZ, DECIMATION_1, SDR_RATE};

/// Shared entropy pool — pipeline writes, HTTP API drains.
/// Capped at 4KB. No broadcast overhead.
pub type SharedEntropyPool = Arc<std::sync::Mutex<Vec<u8>>>;

pub fn create_entropy_pool() -> SharedEntropyPool {
    Arc::new(std::sync::Mutex::new(Vec::with_capacity(4096)))
}

const SQUELCH_HANG_FRAMES: u64 = 10; // ~2 seconds at 240kHz with 48k buffer
const FADE_IN_SAMPLES: usize = 4800; // ~100ms at 48kHz
const VOICE_GATE_MAX_FRAMES: u64 = 3; // Max frames to wait for voice before passing audio

pub struct PipelineConfig {
    pub prebuffer_capacity: usize,
    pub viz_enabled: bool,
}

pub struct Pipeline {
    state: TransmissionState,
}

impl Pipeline {
    pub fn new(config: PipelineConfig) -> Self {
        Self {
            state: TransmissionState::new(config.prebuffer_capacity, config.viz_enabled),
        }
    }

    pub fn reset(&mut self) {
        self.state.reset();
    }

    pub fn process_buffer(
        &mut self,
        buffer: &[u8],
        ctx: &mut PipelineContext,
    ) -> Result<(), String> {
        self.state.process(buffer, ctx)
    }

    pub fn is_squelch_open(&self) -> bool {
        self.state.squelch_open
    }
}

pub struct PipelineContext<'a> {
    pub freq: &'a mut u32,
    pub display_channel: Option<u8>,
    pub channel_label: Option<&'a str>,
    pub squelch_margin: f32,
    pub noise_floor: &'a mut f32,
    pub squelch_thresh: &'a mut f32,
    pub log_file: &'a mut File,
    pub stream_conn: &'a mut Option<TcpStream>,
    pub stream_source: &'a str,
    pub recording_enabled: bool,
    pub listen_enabled: bool,
    pub viz_config: &'a VizConfig,
    pub tx: &'a Sender<Vec<f32>>,
    pub transmission_count: &'a mut u32,
    pub broadcaster: Option<Arc<AudioBroadcaster>>,
    pub entropy_pool: Option<SharedEntropyPool>,
    /// Always-on monitor tap: when true, demodulated audio is broadcast every
    /// frame — static included — so UIs can graph the raw dongle output
    /// continuously instead of only during squelch-open transmissions.
    pub always_stream: bool,
}

struct TransmissionState {
    demod: NFMDemod,
    signal_detector: SignalDetector,
    spectral_squelch: SpectralSquelch,
    impulse_filter: ImpulseNoiseFilter,
    freq_estimator: FrequencyEstimator,
    noise_tracker: NoiseFloorTracker,
    audio_notch: AudioNotchFilter,
    click_suppressor: ClickSuppressor,
    audio_spectrogram_history: Vec<(Vec<f32>, Vec<f32>)>,
    prebuffer: VecDeque<f32>,
    recording_buffer: Vec<f32>,
    squelch_open: bool,
    squelch_hang_counter: u64,
    last_signal_class: SignalClassification,
    peak_mag_min: f32,
    peak_mag_max: f32,
    afc_error_avg: f32,
    frame_count: u64,
    prebuffer_capacity: usize,
    // Audio cleanup fields
    frames_since_squelch_open: u64,
    voice_detected_this_transmission: bool,
    recording_stopped: bool, // stop recording during hang tail
    // Entropy-based squelch
    audio_flatness_ema: f32,
}

impl TransmissionState {
    fn new(prebuffer_capacity: usize, _viz_enabled: bool) -> Self {
        Self {
            demod: NFMDemod::new(),
            signal_detector: SignalDetector::new(SDR_RATE as f32),
            spectral_squelch: SpectralSquelch::new(),
            impulse_filter: ImpulseNoiseFilter::new(20.0),
            freq_estimator: FrequencyEstimator::new(SDR_RATE as f32),
            noise_tracker: NoiseFloorTracker::new(20),
            audio_notch: AudioNotchFilter::new(48_000.0),
            click_suppressor: ClickSuppressor::new(0.3),
            audio_spectrogram_history: Vec::with_capacity(60),
            prebuffer: VecDeque::with_capacity(prebuffer_capacity.max(1)),
            recording_buffer: Vec::new(),
            squelch_open: false,
            squelch_hang_counter: 0,
            last_signal_class: SignalClassification::Static,
            peak_mag_min: 0.0,
            peak_mag_max: -100.0,
            afc_error_avg: 0.0,
            frame_count: 0,
            prebuffer_capacity,
            frames_since_squelch_open: 0,
            voice_detected_this_transmission: false,
            recording_stopped: false,
            audio_flatness_ema: 0.8, // Start assuming noise
        }
    }

    fn reset(&mut self) {
        self.demod = NFMDemod::new();
        self.freq_estimator.reset();
        self.afc_error_avg = 0.0;
        self.squelch_open = false;
        self.squelch_hang_counter = 0;
        self.prebuffer.clear();
        self.recording_buffer.clear();
        self.frames_since_squelch_open = 0;
        self.voice_detected_this_transmission = false;
        self.recording_stopped = false;
        self.audio_flatness_ema = 0.8;
    }

    /// Compute spectral flatness of demodulated audio in the vocal range (300-3400 Hz).
    /// Returns 0.0 for pure tone, 1.0 for white noise.
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
        // Vocal range bins at 48kHz: bin_width = 48000/2048 ≈ 23.4 Hz
        let bin_width: f32 = 48000.0 / 2048.0;
        let lo_bin = (300.0 / bin_width).floor() as usize;
        let hi_bin = (3400.0 / bin_width).ceil() as usize;
        let hi_bin = hi_bin.min(mags.len());
        if lo_bin >= hi_bin {
            return 0.5;
        }
        self.spectral_squelch.spectral_flatness(&mags[lo_bin..hi_bin])
    }

    /// Harvest entropy from audio samples into the shared pool.
    /// ADC quantization noise makes the least significant bits genuinely random.
    /// When pool is full, overwrites oldest bytes to keep entropy fresh.
    fn harvest_entropy(audio: &[f32], pool: &SharedEntropyPool) {
        let mut pool = match pool.lock() {
            Ok(p) => p,
            Err(_) => return,
        };
        let full = pool.len() >= 4096;
        if full {
            // Rotate out oldest 32 bytes to make room for fresh ones
            let n = 32.min(pool.len());
            pool.drain(..n);
        }
        let mut byte: u8 = 0;
        let mut bit_count = 0;
        let mut collected = 0u32;
        for &sample in audio.iter().step_by(37) {
            let bits = sample.to_bits();
            byte = (byte << 1) | (bits as u8 & 1);
            bit_count += 1;
            if bit_count == 8 {
                pool.push(byte);
                byte = 0;
                bit_count = 0;
                collected += 1;
                if collected >= 32 {
                    break;
                }
            }
        }
    }

    fn process(&mut self, buffer: &[u8], ctx: &mut PipelineContext) -> Result<(), String> {
        let iq_samples: Vec<Complex32> = buffer
            .chunks_exact(2)
            .map(|chunk| {
                let i = (chunk[0] as f32 - 127.5) / 127.5;
                let q = (chunk[1] as f32 - 127.5) / 127.5;
                Complex32::new(i, q)
            })
            .collect();

        self.frame_count += 1;

        // === Demodulate first — we need audio for entropy-based squelch ===
        // signal_db is IN-CHANNEL power (post channel filter), not the raw
        // ±120 kHz slice — adjacent transmissions must not move this meter.
        let (mut audio, signal_db) = self.demod.process(buffer);
        self.click_suppressor.process(&mut audio);

        // === Compute audio entropy (spectral flatness in vocal range) ===
        let audio_flatness = self.compute_audio_flatness(&audio);
        self.audio_flatness_ema =
            0.85 * self.audio_flatness_ema + 0.15 * audio_flatness;
        let flatness = self.audio_flatness_ema;

        // Entropy-based signal detection:
        // Low flatness = structured signal (voice/carrier), high = noise
        let signal_present = flatness < 0.55;
        let is_noise = flatness > 0.70;

        // Harvest entropy from every frame — LSBs of FM demod are random
        // from ADC quantization noise regardless of signal presence.
        if let Some(ref pool) = ctx.entropy_pool {
            Self::harvest_entropy(&audio, pool);
        }

        if ctx.viz_config.enabled && self.frame_count % 5 == 0 {
            let (freqs, mut mags) = self.signal_detector.power_spectrum(&iq_samples);
            mags = self.impulse_filter.suppress(&mags);
            let spectral_floor = self.signal_detector.estimate_noise_floor(&mags);
            self.noise_tracker.update(spectral_floor);
            let (_peak_freq, peak_mag, peak_idx) = self.signal_detector.find_peak(&freqs, &mags);
            self.peak_mag_min = self.peak_mag_min.max(peak_mag);
            self.peak_mag_max = self.peak_mag_max.min(peak_mag);

            let flatness = self.spectral_squelch.get_flatness(&mags);
            let voice_candidate = self.spectral_squelch.should_open(&mags);
            let harmonic_bins = mags
                .iter()
                .enumerate()
                .filter(|(idx, &mag)| *idx != peak_idx && mag - spectral_floor > 6.0)
                .count();
            let voice_detected = voice_candidate && harmonic_bins >= 4;
            self.last_signal_class = classify_signal(
                signal_db,
                *ctx.noise_floor,
                peak_mag,
                spectral_floor,
                voice_detected,
                flatness,
                harmonic_bins,
            );

            println!(
                "\n[Spectrum] Peak: {:.1} dB [min:{:.1} max:{:.1}] | Noise: {:.1} dB | Flatness: {:.3} -> {}",
                peak_mag,
                self.peak_mag_max,
                self.peak_mag_min,
                spectral_floor,
                flatness,
                self.last_signal_class.label()
            );

            let viz_start = freqs.len() / 2 - 500;
            let viz_end = freqs.len() / 2 + 500;
            if viz_end < freqs.len() {
                let viz_freqs = &freqs[viz_start..viz_end];
                let viz_mags = &mags[viz_start..viz_end];
                let viz_output = render_spectrum(
                    viz_freqs,
                    viz_mags,
                    &format!("RF Spectrum ({:.3} MHz)", *ctx.freq as f64 / 1e6),
                    ctx.viz_config,
                );
                print_viz(&viz_output, ctx.viz_config);
            }
        } else if !ctx.viz_config.enabled {
            // Use audio entropy for classification when viz is off
            self.last_signal_class = if flatness < 0.45 {
                SignalClassification::Voice
            } else if signal_present {
                SignalClassification::Carrier
            } else {
                SignalClassification::Static
            };
        }

        // Entropy-based noise floor adaptation:
        // When audio is noise-like (high flatness), adapt aggressively.
        // When signal is present, adapt slowly. This prevents the noise
        // floor from freezing when dB alone can't distinguish signal from noise.
        if is_noise {
            let alpha = if self.squelch_open { 0.05 } else { 0.15 };
            *ctx.noise_floor = (1.0 - alpha) * *ctx.noise_floor + alpha * signal_db;
            *ctx.squelch_thresh = *ctx.noise_floor + ctx.squelch_margin;
        } else if !signal_present && signal_db < *ctx.squelch_thresh {
            let alpha = 0.02;
            *ctx.noise_floor = (1.0 - alpha) * *ctx.noise_floor + alpha * signal_db;
            *ctx.squelch_thresh = *ctx.noise_floor + ctx.squelch_margin;
        }

        if self.squelch_open && self.frame_count % 10 == 0 {
            println!(
                "[Frame {}] Signal: {:.1}dB | Noise: {:.1}dB | Thresh: {:.1}dB | Flat: {:.3} | Hang: {} | Squelch: OPEN",
                self.frame_count,
                signal_db,
                *ctx.noise_floor,
                *ctx.squelch_thresh,
                flatness,
                self.squelch_hang_counter
            );
        }

        if self.squelch_open {
            self.frames_since_squelch_open += 1;

            // Track voice detection for this transmission
            if matches!(self.last_signal_class, SignalClassification::Voice) {
                self.voice_detected_this_transmission = true;
            }

            // Entropy-based hang counter: structured signal holds, noise counts down
            if signal_present {
                // Structured audio (low flatness) — reset hang timer
                self.squelch_hang_counter = SQUELCH_HANG_FRAMES;
            } else if is_noise {
                // Definitely noise — count down faster
                if self.squelch_hang_counter > 2 {
                    self.squelch_hang_counter -= 2;
                } else if self.squelch_hang_counter > 0 {
                    self.squelch_hang_counter -= 1;
                }
            } else {
                // Ambiguous region — normal countdown
                if self.squelch_hang_counter > 0 {
                    self.squelch_hang_counter -= 1;
                }
            }

            // Stop recording during hang tail when not structured signal
            if self.voice_detected_this_transmission {
                if !signal_present && self.squelch_hang_counter < 7 {
                    self.recording_stopped = true;
                }
                if flatness < 0.45 {
                    self.recording_stopped = false;
                }
            }

            if self.squelch_hang_counter == 0 {
                println!("  [Squelch] HANG TIME EXPIRED - closing squelch");
                let timestamp = chrono::Utc::now()
                    .format("%Y-%m-%d %H:%M:%S UTC")
                    .to_string();
                let freq_mhz = *ctx.freq as f64 / 1e6;
                let ch_info = ctx
                    .display_channel
                    .map(|ch| {
                        let label = ctx.channel_label.unwrap_or("UNKNOWN");
                        format!("Ch {} ({}) ", ch, label)
                    })
                    .unwrap_or_default();
                let log_msg = format!(
                    "{} | SQUELCH CLOSED | {}| {:.3} MHz | Signal: {:.1}dB | Noise Floor: {:.1}dB\n",
                    timestamp, ch_info, freq_mhz, signal_db, *ctx.noise_floor
                );
                println!(
                    "[{}] ▼ Squelch CLOSED (signal: {:.1}dB dropped to noise floor: {:.1}dB)",
                    timestamp, signal_db, *ctx.noise_floor
                );

                // Broadcast squelch close event
                if let Some(broadcaster) = ctx.broadcaster.as_ref() {
                    broadcaster.broadcast(AudioMessage::SquelchEvent {
                        channel: ctx.display_channel,
                        freq: *ctx.freq,
                        open: false,
                        signal_db,
                        classification: self.last_signal_class.label().to_string(),
                    });
                }

                if ctx.recording_enabled && !self.recording_buffer.is_empty() {
                    *ctx.transmission_count += 1;
                    let channel_label = ctx.channel_label;
                    let result = recorder::write_recording(
                        &self.recording_buffer,
                        *ctx.freq,
                        ctx.display_channel,
                        channel_label,
                        *ctx.transmission_count,
                    );

                    match result {
                        Ok(path) => {
                            let freq_mhz = *ctx.freq as f64 / 1e6;
                            let timestamp = chrono::Utc::now()
                                .format("%Y-%m-%d %H:%M:%S UTC")
                                .to_string();
                            println!(
                                "[{}]   Saved: {} ({:.3} MHz)",
                                timestamp,
                                path.display(),
                                freq_mhz
                            );

                            let ch_info = ctx
                                .display_channel
                                .map(|ch| {
                                    let label = ctx.channel_label.unwrap_or("UNKNOWN");
                                    format!("Ch {} ({}) ", ch, label)
                                })
                                .unwrap_or_default();
                            let record_msg = format!(
                                "{} | RECORDING SAVED | {}| {:.3} MHz | {}\n",
                                timestamp,
                                ch_info,
                                freq_mhz,
                                path.display()
                            );
                            let _ = ctx.log_file.write_all(record_msg.as_bytes());
                            let _ = ctx.log_file.flush();

                            // Submit for transcription
                            if let Some(bc) = ctx.broadcaster.as_ref() {
                                crate::transcribe::submit_transcription(
                                    &path,
                                    bc.clone(),
                                    ctx.display_channel,
                                    *ctx.freq,
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!("Recording error: {}", e);
                        }
                    }
                    self.recording_buffer.clear();
                } else if !ctx.recording_enabled {
                    self.recording_buffer.clear();
                }

                ctx.log_file.write_all(log_msg.as_bytes()).ok();
                ctx.log_file.flush().ok();
                self.squelch_open = false;
                self.frames_since_squelch_open = 0;
                self.voice_detected_this_transmission = false;
                self.recording_stopped = false;
                return Ok(());
            }
        } else if signal_present {
            // Entropy-based squelch open: structured audio detected (low flatness)
            let timestamp = chrono::Utc::now()
                .format("%Y-%m-%d %H:%M:%S UTC")
                .to_string();
            let freq_mhz = *ctx.freq as f64 / 1e6;
            let ch_info = ctx
                .display_channel
                .map(|ch| {
                    let label = ctx.channel_label.unwrap_or("UNKNOWN");
                    format!("Ch {} ({}) ", ch, label)
                })
                .unwrap_or_default();
            let log_msg = format!(
                "{} | SQUELCH OPEN | {}| {:.3} MHz | Flatness: {:.3} | Signal: {:.1}dB | Noise: {:.1}dB\n",
                timestamp,
                ch_info,
                freq_mhz,
                flatness,
                signal_db,
                *ctx.noise_floor
            );
            println!(
                "[{}] ▲ Squelch OPEN (flatness: {:.3} < 0.55, signal: {:.1}dB, noise: {:.1}dB)",
                timestamp, flatness, signal_db, *ctx.noise_floor
            );
            ctx.log_file.write_all(log_msg.as_bytes()).ok();
            ctx.log_file.flush().ok();
            self.squelch_open = true;
            self.squelch_hang_counter = SQUELCH_HANG_FRAMES;
            self.recording_buffer.clear();

            // Drain prebuffer into recording + broadcast (captures pre-squelch voice onset)
            if !self.prebuffer.is_empty() {
                let mut pre: Vec<f32> = self.prebuffer.drain(..).collect();
                // Fade-in the prebuffer to avoid click at start
                let fade_len = (pre.len()).min(FADE_IN_SAMPLES);
                for i in 0..fade_len {
                    let g = i as f32 / fade_len as f32;
                    pre[i] *= g * g;
                }
                if ctx.recording_enabled {
                    self.recording_buffer.extend_from_slice(&pre);
                }
                if let Some(broadcaster) = ctx.broadcaster.as_ref() {
                    broadcaster.broadcast(AudioMessage::Audio {
                        channel: ctx.display_channel,
                        freq: *ctx.freq,
                        samples: pre,
                        signal_db,
                    });
                }
            }

            self.frames_since_squelch_open = 0;
            self.voice_detected_this_transmission = false;
            self.recording_stopped = false;

            // Broadcast squelch open event
            if let Some(broadcaster) = ctx.broadcaster.as_ref() {
                broadcaster.broadcast(AudioMessage::SquelchEvent {
                    channel: ctx.display_channel,
                    freq: *ctx.freq,
                    open: true,
                    signal_db,
                    classification: self.last_signal_class.label().to_string(),
                });
            }
        }

        // Audio was demodulated earlier (before squelch decision) for entropy analysis.
        // Use flatness for audio gating instead of classification.
        let past_voice_gate = self.frames_since_squelch_open <= 1
            || self.voice_detected_this_transmission
            || self.frames_since_squelch_open > VOICE_GATE_MAX_FRAMES;

        let in_hang_tail = self.voice_detected_this_transmission
            && self.squelch_hang_counter < SQUELCH_HANG_FRAMES
            && self.squelch_hang_counter > 0
            && !signal_present
            && self.squelch_hang_counter < 7;

        let should_output_audio =
            self.squelch_open && !is_noise && past_voice_gate && !in_hang_tail;

        if !self.squelch_open {
            for &sample in &audio {
                if self.prebuffer.len() >= self.prebuffer_capacity {
                    self.prebuffer.pop_front();
                }
                self.prebuffer.push_back(sample);
            }
        }

        if should_output_audio {
            // Apply fade-in ramp on first frames after squelch opens
            if self.frames_since_squelch_open <= 2 {
                let base = if self.frames_since_squelch_open <= 1 {
                    0
                } else {
                    audio.len()
                };
                for (i, sample) in audio.iter_mut().enumerate() {
                    let total_i = base + i;
                    if total_i < FADE_IN_SAMPLES {
                        let gain = total_i as f32 / FADE_IN_SAMPLES as f32;
                        *sample *= gain * gain; // Squared ramp for softer onset
                    }
                }
            }

            // Apply fade-out ramp when hang counter is low (approaching close)
            if self.squelch_hang_counter <= 3 && self.squelch_hang_counter > 0 {
                let fade_factor = self.squelch_hang_counter as f32 / 3.0;
                let audio_len = audio.len() as f32;
                for (i, sample) in audio.iter_mut().enumerate() {
                    let ramp = fade_factor * (1.0 - i as f32 / audio_len);
                    *sample *= ramp.max(0.0);
                }
            }

            if ctx.viz_config.enabled && self.frame_count % 20 == 0 {
                let viz_output = render_waveform(&audio, "Audio Waveform", ctx.viz_config);
                print_viz(&viz_output, ctx.viz_config);

                let audio_iq: Vec<Complex32> =
                    audio.iter().map(|&s| Complex32::new(s, 0.0)).collect();
                let (audio_freqs, mut audio_mags) = self.signal_detector.power_spectrum(&audio_iq);
                self.audio_notch
                    .apply_to_spectrum(&audio_freqs, &mut audio_mags);
                let half_len = audio_freqs.len() / 2;
                let pos_freqs = audio_freqs[0..half_len].to_vec();
                let pos_mags = audio_mags[0..half_len].to_vec();
                self.audio_spectrogram_history
                    .push((pos_freqs.clone(), pos_mags.clone()));
                if self.audio_spectrogram_history.len() > 60 {
                    self.audio_spectrogram_history.remove(0);
                }
                let audio_spec_output = render_spectrum(
                    &pos_freqs,
                    &pos_mags,
                    "Audio Spectrum (Voice/Ticks)",
                    ctx.viz_config,
                );
                print_viz(&audio_spec_output, ctx.viz_config);
                if self.audio_spectrogram_history.len() >= 10 {
                    let spectrogram_output = render_spectrogram(
                        &self.audio_spectrogram_history,
                        "Audio Spectrogram (Time ->)",
                        ctx.viz_config,
                    );
                    print_viz(&spectrogram_output, ctx.viz_config);
                }
            }

            if let Some(stream) = ctx.stream_conn.as_mut() {
                if let Err(e) = send_stream_samples(stream, &audio, ctx.stream_source) {
                    eprintln!("Stream write failed: {}", e);
                    *ctx.stream_conn = None;
                }
            }

            // Only add to recording buffer if not in hang tail
            if ctx.recording_enabled && !self.recording_stopped {
                self.recording_buffer.extend_from_slice(&audio);
            }

            if self.frame_count % 5 == 0 {
                let audio_rms =
                    (audio.iter().map(|x| x * x).sum::<f32>() / audio.len() as f32).sqrt();
                let audio_peak = audio.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
                println!(
                    "  Audio RMS: {:.3} | Peak: {:.3} | Samples: {}",
                    audio_rms,
                    audio_peak,
                    audio.len()
                );
            }

            // Broadcast audio to web clients
            if let Some(broadcaster) = ctx.broadcaster.as_ref() {
                broadcaster.broadcast(AudioMessage::Audio {
                    channel: ctx.display_channel,
                    freq: *ctx.freq,
                    samples: audio.clone(),
                    signal_db,
                });
            }

            if ctx.listen_enabled {
                let _ = ctx.tx.try_send(audio.clone());
            }
        }

        if signal_db > *ctx.noise_floor + 1.0
            && self.last_signal_class != SignalClassification::Static
        {
            let mut offset = self.freq_estimator.estimate(&iq_samples);
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

        // Always-on monitor tap: stream the demodulated frame (static included)
        // whenever squelch is closed — open-squelch frames are already
        // broadcast by the transmission path above, so no duplicates.
        if ctx.always_stream && !self.squelch_open {
            if let Some(broadcaster) = ctx.broadcaster.as_ref() {
                broadcaster.broadcast(AudioMessage::Audio {
                    channel: ctx.display_channel,
                    freq: *ctx.freq,
                    samples: audio.clone(),
                    signal_db,
                });
            }
        }

        // Broadcast signal level every frame for always-active viz
        if let Some(broadcaster) = ctx.broadcaster.as_ref() {
            if self.frame_count % 2 == 0 {
                broadcaster.broadcast(AudioMessage::SignalLevel {
                    channel: ctx.display_channel,
                    freq: *ctx.freq,
                    signal_db,
                    noise_floor: *ctx.noise_floor,
                    squelch_open: self.squelch_open,
                    audio_flatness: self.audio_flatness_ema,
                });
            }
        }

        Ok(())
    }
}

/// Channel-select FIR: cutoff at NFM voice bandwidth so the neighbouring
/// 25 kHz channel is deep in the stopband by the time we demodulate.
const CHAN_FIR_TAPS: usize = 81;
const CHAN_FIR_CUTOFF_HZ: f32 = 11_000.0;

struct NFMDemod {
    prev_sample: Complex32,
    freq_corr_step: f32,
    freq_corr_phase: f32,
    chan_fir: Vec<f32>,
    fir_hist: Vec<Complex32>, // unconsumed tail — keeps filter + decimation phase continuous across frames
}

impl NFMDemod {
    fn new() -> Self {
        Self {
            prev_sample: Complex32::new(0.0, 0.0),
            freq_corr_step: 0.0,
            freq_corr_phase: 0.0,
            chan_fir: crate::wideband::design_lowpass_fir(
                CHAN_FIR_TAPS,
                CHAN_FIR_CUTOFF_HZ / SDR_RATE as f32,
            ),
            fir_hist: Vec::new(),
        }
    }

    fn set_freq_offset(&mut self, offset_hz: f32) {
        let sample_rate = (SDR_RATE as f32) / (DECIMATION_1 as f32);
        self.freq_corr_step = if offset_hz.abs() < 0.05 {
            0.0
        } else {
            -2.0 * std::f32::consts::PI * offset_hz / sample_rate
        };
    }

    fn apply_freq_correction(&mut self, samples: &mut [Complex32]) {
        if self.freq_corr_step.abs() < 1e-7 {
            return;
        }
        for sample in samples {
            let rot = Complex32::from_polar(1.0, self.freq_corr_phase);
            *sample *= rot;
            self.freq_corr_phase += self.freq_corr_step;
            if self.freq_corr_phase > std::f32::consts::PI {
                self.freq_corr_phase -= 2.0 * std::f32::consts::PI;
            } else if self.freq_corr_phase < -std::f32::consts::PI {
                self.freq_corr_phase += 2.0 * std::f32::consts::PI;
            }
        }
    }

    /// Returns (audio, in-channel signal dB). The wideband channelizer hands
    /// us ±120 kHz of spectrum; the old 3-tap smoother had no selectivity, so
    /// a transmission on the next 25 kHz channel rode straight into the
    /// discriminator (and aliased through the ÷5 decimation) — talking on
    /// ch 71 painted ch 72's waterfall and opened its entropy squelch. The
    /// FIR here is the actual channel filter, polyphase-decimated (computed
    /// only at output positions), and power is measured after it so squelch
    /// metering can't see the neighbours either.
    fn process(&mut self, iq_samples: &[u8]) -> (Vec<f32>, f32) {
        let mut complex: Vec<Complex32> = Vec::with_capacity(iq_samples.len() / 2);
        for chunk in iq_samples.chunks_exact(2) {
            let i = (chunk[0] as f32 - 127.5) / 127.5;
            let q = (chunk[1] as f32 - 127.5) / 127.5;
            complex.push(Complex32::new(i, q));
        }

        let taps = self.chan_fir.len();
        let mut buf = std::mem::take(&mut self.fir_hist);
        buf.extend_from_slice(&complex);
        let mut decimated: Vec<Complex32> = Vec::with_capacity(buf.len() / DECIMATION_1 + 1);
        let mut pos = 0usize;
        while pos + taps <= buf.len() {
            let mut acc = Complex32::new(0.0, 0.0);
            for (j, &c) in self.chan_fir.iter().enumerate() {
                acc += buf[pos + j] * c;
            }
            decimated.push(acc);
            pos += DECIMATION_1;
        }
        self.fir_hist = buf.split_off(pos.min(buf.len()));

        let narrow_db = if decimated.is_empty() {
            -120.0
        } else {
            10.0 * (decimated.iter().map(|s| s.norm_sqr()).sum::<f32>()
                / decimated.len() as f32
                + 1e-12)
                .log10()
        };

        self.apply_freq_correction(&mut decimated);

        let mut audio = Vec::with_capacity(decimated.len());
        let mut prev = self.prev_sample;
        for &sample in &decimated {
            let normalized = sample / (sample.norm() + 1e-10);
            let product = normalized * prev.conj();
            audio.push(product.arg() / (2.0 * std::f32::consts::PI));
            prev = normalized;
        }
        self.prev_sample = prev;

        let mut dc_offset = 0.0f32;
        let dc_alpha = 0.001;
        let hp_filtered: Vec<f32> = audio
            .iter()
            .map(|&sample| {
                dc_offset = dc_alpha * sample + (1.0 - dc_alpha) * dc_offset;
                sample - dc_offset
            })
            .collect();

        let filtered = Self::simple_lowpass(&hp_filtered, 48_000.0, 3000.0);

        let final_audio = filtered;

        let max = final_audio.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
        let audio = if max > 0.01 {
            final_audio.iter().map(|&x| (x / max) * 0.7).collect()
        } else {
            final_audio
        };
        (audio, narrow_db)
    }

    fn simple_lowpass(input: &[f32], sample_rate: f32, cutoff: f32) -> Vec<f32> {
        let rc = 1.0 / (2.0 * std::f32::consts::PI * cutoff);
        let dt = 1.0 / sample_rate;
        let alpha = dt / (rc + dt);

        let mut output = Vec::with_capacity(input.len());
        let mut state = 0.0;
        for &sample in input {
            state = alpha * sample + (1.0 - alpha) * state;
            output.push(state);
        }
        output
    }
}

pub fn calc_power_db(buffer: &[u8]) -> f32 {
    let mut power_sum = 0.0f32;
    for chunk in buffer.chunks_exact(2) {
        let i = (chunk[0] as f32 - 127.5) / 127.5;
        let q = (chunk[1] as f32 - 127.5) / 127.5;
        power_sum += i * i + q * q;
    }
    10.0 * (power_sum / (buffer.len() as f32 / 2.0) + 1e-12).log10()
}
