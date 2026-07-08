use std::collections::HashMap;
use std::f32::consts::PI;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossbeam_channel::{bounded, Sender};
use num_complex::Complex32;
use rustfft::{num_complex::Complex, FftPlanner};

use crate::agentic;
use crate::audio;
use crate::broadcast::{AudioBroadcaster, AudioMessage, ChannelInfo};
use crate::channels::{all_channels, channel_label, channel_to_freq};
use crate::control::{self, ControlState};
use crate::pipeline::{create_entropy_pool, Pipeline, PipelineConfig, PipelineContext};
use crate::viz::VizConfig;
use crate::web;
use crate::{AUDIO_SAMPLE_RATE, AUTO_RESUME_SCAN_SEC, PREBUFFER_SECONDS};

const WIDEBAND_RATE: u32 = 2_400_000;
const WIDEBAND_CENTER: u32 = 156_737_500; // Midpoint of marine VHF band
const WIDEBAND_DECIMATION: usize = 10; // 2.4MHz -> 240kHz per channel
const WIDEBAND_BUFFER_SAMPLES: usize = 480_000; // 200ms at 2.4MHz
const FFT_DETECT_SIZE: usize = 8192;
const LPF_TAPS: usize = 51;
const LPF_CUTOFF_NORM: f32 = 0.05; // 120kHz / 2.4MHz

struct ChannelSlot {
    pipeline: Pipeline,
    channel: u8,
    freq: u32,
    noise_floor: f32,
    squelch_thresh: f32,
    transmission_count: u32,
    silence_start: Option<Instant>,
    mixer_phase: f32,
}

/// Design a windowed-sinc low-pass FIR filter.
/// `cutoff_norm` is cutoff frequency normalized to sample rate (0..0.5).
pub(crate) fn design_lowpass_fir(num_taps: usize, cutoff_norm: f32) -> Vec<f32> {
    let center = (num_taps - 1) as f32 / 2.0;
    let mut coeffs = Vec::with_capacity(num_taps);

    for i in 0..num_taps {
        let n = i as f32 - center;
        let sinc = if n.abs() < 1e-6 {
            2.0 * cutoff_norm
        } else {
            (2.0 * PI * cutoff_norm * n).sin() / (PI * n)
        };
        // Hann window
        let window = 0.5 * (1.0 - (2.0 * PI * i as f32 / (num_taps - 1) as f32).cos());
        coeffs.push(sinc * window);
    }

    let sum: f32 = coeffs.iter().sum();
    for c in &mut coeffs {
        *c /= sum;
    }

    coeffs
}

/// Extract a single channel from wideband IQ data.
/// Frequency-shifts to baseband, applies FIR LPF, decimates, converts to u8.
fn extract_channel(
    wideband: &[Complex32],
    offset_hz: f32,
    filter_coeffs: &[f32],
    mixer_phase: &mut f32,
) -> Vec<u8> {
    let phase_step = -2.0 * PI * offset_hz / WIDEBAND_RATE as f32;
    let half_len = filter_coeffs.len() / 2;

    // Frequency-shift the entire buffer
    let mut shifted = Vec::with_capacity(wideband.len());
    let mut phase = *mixer_phase;
    for &sample in wideband {
        let mixer = Complex32::from_polar(1.0, phase);
        shifted.push(sample * mixer);
        phase += phase_step;
        if phase > PI {
            phase -= 2.0 * PI;
        } else if phase < -PI {
            phase += 2.0 * PI;
        }
    }
    *mixer_phase = phase;

    // FIR filter + decimate (polyphase: only compute at output positions)
    let mut result_bytes = Vec::with_capacity((wideband.len() / WIDEBAND_DECIMATION) * 2);
    let mut pos = half_len;
    while pos + half_len < shifted.len() {
        let mut sum = Complex32::new(0.0, 0.0);
        for (j, &coeff) in filter_coeffs.iter().enumerate() {
            sum += shifted[pos + j - half_len] * coeff;
        }
        let i_byte = (sum.re * 127.5 + 127.5).clamp(0.0, 255.0) as u8;
        let q_byte = (sum.im * 127.5 + 127.5).clamp(0.0, 255.0) as u8;
        result_bytes.push(i_byte);
        result_bytes.push(q_byte);
        pos += WIDEBAND_DECIMATION;
    }

    result_bytes
}

/// Detect which channels have energy above the squelch threshold using FFT.
/// Returns vec of (channel_number, power_db).
fn detect_active_channels(
    wideband: &[Complex32],
    channel_offsets: &[(u8, f32)],
    noise_floor_db: f32,
    squelch_margin: f32,
) -> Vec<(u8, f32)> {
    let fft_size = FFT_DETECT_SIZE.min(wideband.len());

    let mut buffer: Vec<Complex<f32>> = wideband[..fft_size]
        .iter()
        .map(|s| Complex { re: s.re, im: s.im })
        .collect();

    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(fft_size);
    fft.process(&mut buffer);

    let bin_width = WIDEBAND_RATE as f32 / fft_size as f32;
    // Number of FFT bins per 25 kHz channel
    let channel_half_bins = (12_500.0 / bin_width) as usize;

    let thresh = noise_floor_db + squelch_margin;
    let mut active = Vec::new();

    for &(ch, offset) in channel_offsets {
        let bin_idx = if offset >= 0.0 {
            (offset / bin_width) as usize
        } else {
            fft_size - ((-offset) / bin_width) as usize
        };

        let start = bin_idx.saturating_sub(channel_half_bins);
        let end = (bin_idx + channel_half_bins).min(fft_size - 1);

        let mut power = 0.0f32;
        let mut count = 0;
        for b in start..=end {
            power += buffer[b].norm_sqr();
            count += 1;
        }

        let power_db = 10.0 * (power / count as f32 + 1e-12).log10();
        if power_db > thresh {
            active.push((ch, power_db));
        }
    }

    active
}

/// Estimate wideband noise floor from FFT magnitudes (median).
fn estimate_noise_floor(wideband: &[Complex32]) -> f32 {
    let fft_size = FFT_DETECT_SIZE.min(wideband.len());

    let mut buffer: Vec<Complex<f32>> = wideband[..fft_size]
        .iter()
        .map(|s| Complex { re: s.re, im: s.im })
        .collect();

    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(fft_size);
    fft.process(&mut buffer);

    let mut mags: Vec<f32> = buffer
        .iter()
        .map(|c| 10.0 * (c.norm_sqr() + 1e-12).log10())
        .collect();
    mags.sort_by(|a, b| a.partial_cmp(b).unwrap());
    mags[mags.len() / 2]
}

pub fn wideband_monitor(
    device_index: u32,
    exclude: &[u8],
    squelch_margin: f32,
    log_path: &str,
    enable_recording: bool,
    enable_listen: bool,
    _viz_config: VizConfig,
    stream_target: Option<String>,
    running: Arc<AtomicBool>,
) -> Result<(), String> {
    std::fs::create_dir_all("recordings")
        .map_err(|e| format!("Failed to create recordings directory: {}", e))?;
    std::fs::create_dir_all("logs")
        .map_err(|e| format!("Failed to create logs directory: {}", e))?;

    let device_index = i32::try_from(device_index)
        .map_err(|_| format!("Invalid device index: {}", device_index))?;

    // Create broadcaster for web streaming
    let broadcaster = Arc::new(AudioBroadcaster::new());

    // Build list of monitored channels and their offsets from center
    let monitored: Vec<u8> = all_channels()
        .into_iter()
        .filter(|ch| !exclude.contains(ch))
        .collect();

    let channel_offsets: Vec<(u8, f32)> = monitored
        .iter()
        .filter_map(|&ch| {
            channel_to_freq(ch).map(|freq| (ch, freq as f32 - WIDEBAND_CENTER as f32))
        })
        .collect();

    println!("Wideband VHF Monitor");
    println!("  Center: {:.3} MHz", WIDEBAND_CENTER as f64 / 1e6);
    println!("  Bandwidth: {:.1} MHz", WIDEBAND_RATE as f64 / 1e6);
    println!(
        "  Monitoring {} channels simultaneously",
        channel_offsets.len()
    );
    if !exclude.is_empty() {
        println!("  Excluding channels: {:?}", exclude);
    }
    println!("  Squelch margin: {} dB", squelch_margin);
    println!(
        "  Recording: {}",
        if enable_recording {
            "ENABLED"
        } else {
            "DISABLED"
        }
    );
    println!(
        "  Live Audio: {}\n",
        if enable_listen { "ENABLED" } else { "DISABLED" }
    );

    // Control server + WebSocket server
    let control_state = ControlState {
        channel: Arc::new(AtomicU32::new(0)),
        recording: Arc::new(AtomicBool::new(enable_recording)),
        listen: Arc::new(AtomicBool::new(enable_listen)),
        frequency_hz: Arc::new(AtomicU32::new(WIDEBAND_CENTER)),
        squelch_margin: Arc::new(Mutex::new(squelch_margin)),
    };
    let agentic_state = agentic::create_agentic_state();
    let entropy_pool = create_entropy_pool();
    agentic::start_agentic_subscriber(agentic_state.clone(), broadcaster.clone(), running.clone());
    control::start_control_server(control_state.clone(), broadcaster.clone(), running.clone(), agentic_state, entropy_pool.clone());
    web::start_websocket_server(broadcaster.clone(), running.clone());

    // SDR setup
    let mut dev = rtlsdr::open(device_index).map_err(|e| format!("Failed to open SDR: {:?}", e))?;
    dev.set_sample_rate(WIDEBAND_RATE)
        .map_err(|e| format!("{:?}", e))?;
    dev.set_center_freq(WIDEBAND_CENTER)
        .map_err(|e| format!("{:?}", e))?;
    dev.set_tuner_gain_mode(true)
        .map_err(|e| format!("{:?}", e))?;
    dev.set_tuner_gain(496).map_err(|e| format!("{:?}", e))?;
    dev.set_tuner_bandwidth(WIDEBAND_RATE).ok();
    dev.reset_buffer().map_err(|e| format!("{:?}", e))?;

    // Audio thread
    let (tx, rx): (Sender<Vec<f32>>, crossbeam_channel::Receiver<Vec<f32>>) = bounded(20);
    let audio_handle = audio::spawn_audio_thread(rx, running.clone());

    // Log file
    let mut log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| format!("Failed to open log file: {}", e))?;

    // Stream connection
    let mut stream_conn = if let Some(addr) = stream_target {
        match TcpStream::connect(&addr) {
            Ok(stream) => {
                stream.set_nodelay(true).ok();
                println!("Streaming audio to {}", addr);
                Some(stream)
            }
            Err(e) => {
                eprintln!("Failed to connect to stream {}: {}", addr, e);
                None
            }
        }
    } else {
        None
    };

    // Pre-compute FIR filter coefficients
    let filter_coeffs = design_lowpass_fir(LPF_TAPS, LPF_CUTOFF_NORM);

    // Calibrate noise floor
    println!("Calibrating noise floor...");
    let _ = dev
        .read_sync(WIDEBAND_BUFFER_SAMPLES * 2)
        .map_err(|e| format!("{:?}", e))?;
    let mut noise_samples = Vec::new();
    for _ in 0..5 {
        if let Ok(buffer) = dev.read_sync(WIDEBAND_BUFFER_SAMPLES * 2) {
            let iq = bytes_to_complex(&buffer);
            noise_samples.push(estimate_noise_floor(&iq));
        }
    }
    let mut noise_floor_db = if noise_samples.is_empty() {
        -60.0
    } else {
        noise_samples.iter().sum::<f32>() / noise_samples.len() as f32
    };
    println!("Initial noise floor: {:.1} dB\n", noise_floor_db);

    let prebuffer_capacity = (AUDIO_SAMPLE_RATE as f32 * PREBUFFER_SECONDS) as usize;
    let mut active_channels: HashMap<u8, ChannelSlot> = HashMap::new();
    let viz_off = VizConfig {
        enabled: false,
        output_dir: String::new(),
        width: 0,
        height: 0,
    };

    let mut frame_count: u64 = 0;
    let mut current_squelch_margin = squelch_margin;

    while running.load(Ordering::Relaxed) {
        let raw_buffer = match dev.read_sync(WIDEBAND_BUFFER_SAMPLES * 2) {
            Ok(buf) => buf,
            Err(e) => {
                eprintln!("SDR read error: {:?}", e);
                break;
            }
        };

        let wideband = bytes_to_complex(&raw_buffer);
        frame_count += 1;

        // Check for squelch margin updates from web UI
        if let Ok(margin) = control_state.squelch_margin.lock() {
            if (*margin - current_squelch_margin).abs() > 0.01 {
                current_squelch_margin = *margin;
                println!("\nSquelch margin changed to {:.1} dB", current_squelch_margin);
            }
        }

        // Check recording state from control server
        let current_recording = control_state.recording.load(Ordering::Relaxed);
        let current_listen = control_state.listen.load(Ordering::Relaxed);

        // Always-on monitor tap: the channel whose demodulated audio streams
        // continuously (static included) so UIs can graph the raw dongle
        // output at all times. User selection via /channel wins; CH 16 default.
        let user_sel = control_state.channel.load(Ordering::Relaxed) as u8;
        let tap_ch: u8 = if user_sel != 0 && channel_to_freq(user_sel).is_some() {
            user_sel
        } else {
            16
        };

        // Update noise floor periodically (when few channels active)
        if frame_count % 50 == 0 && active_channels.is_empty() {
            let nf = estimate_noise_floor(&wideband);
            noise_floor_db = 0.95 * noise_floor_db + 0.05 * nf;
        }

        // Detect active channels via FFT
        let detected =
            detect_active_channels(&wideband, &channel_offsets, noise_floor_db, current_squelch_margin);

        // Broadcast channel activity periodically
        if frame_count % 25 == 0 {
            let mut active_info: Vec<ChannelInfo> = Vec::new();
            for &(ch, power_db) in &detected {
                active_info.push(ChannelInfo {
                    channel: ch,
                    label: channel_label(ch).to_string(),
                    freq: channel_to_freq(ch).unwrap_or(0),
                    signal_db: power_db,
                    classification: "DETECTED".to_string(),
                });
            }
            for (ch, slot) in &active_channels {
                if !detected.iter().any(|(c, _)| c == ch) {
                    active_info.push(ChannelInfo {
                        channel: *ch,
                        label: channel_label(*ch).to_string(),
                        freq: slot.freq,
                        signal_db: slot.noise_floor,
                        classification: if slot.pipeline.is_squelch_open() {
                            "ACTIVE".to_string()
                        } else {
                            "IDLE".to_string()
                        },
                    });
                }
            }
            // Always broadcast — even an empty list. UIs need the heartbeat and
            // the live noise floor; broadcasting only on activity made idle
            // scan mode look dead (zero WS messages).
            broadcaster.broadcast(AudioMessage::ChannelActivity {
                active: active_info,
                noise_floor: noise_floor_db,
            });

            // Lock = a channel with its SQUELCH OPEN (a real transmission), the
            // strongest if several. Raw FFT detections must NOT move the lock —
            // that flipped UIs between noise birdies while scanning. Scanning
            // with no lock is reported as channel: None ("SCANNING" state).
            let lock = active_channels
                .iter()
                .filter(|(_, s)| s.pipeline.is_squelch_open())
                .map(|(c, s)| {
                    let p = detected
                        .iter()
                        .find(|(dc, _)| dc == c)
                        .map(|&(_, p)| p)
                        .unwrap_or(s.noise_floor);
                    (*c, p)
                })
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            if let Some((ch, p)) = lock {
                broadcaster.broadcast(AudioMessage::SignalLevel {
                    channel: Some(ch),
                    freq: channel_to_freq(ch).unwrap_or(0),
                    signal_db: p,
                    noise_floor: noise_floor_db,
                    squelch_open: true,
                    audio_flatness: 0.5,
                });
            } else {
                broadcaster.broadcast(AudioMessage::SignalLevel {
                    channel: None,
                    freq: WIDEBAND_CENTER,
                    signal_db: noise_floor_db,
                    noise_floor: noise_floor_db,
                    squelch_open: false,
                    audio_flatness: 0.85,
                });
            }
        }

        // Create pipelines for newly detected channels
        for &(ch, power_db) in &detected {
            if !active_channels.contains_key(&ch) {
                let freq = match channel_to_freq(ch) {
                    Some(f) => f,
                    None => continue,
                };
                let timestamp = chrono::Utc::now()
                    .format("%Y-%m-%d %H:%M:%S UTC")
                    .to_string();
                println!(
                    "[{}] >> Ch{} ({}) {:.3} MHz - Signal detected ({:.1} dB)",
                    timestamp,
                    ch,
                    channel_label(ch),
                    freq as f64 / 1e6,
                    power_db
                );

                // Pipeline signal_db is now IN-CHANNEL power (post channel
                // filter): noise reads ~-30..-35 dB, voice ~-10..0 dB. Seed
                // near the noise end; the entropy tracker refines it fast.
                let narrowband_noise = -32.0f32;
                active_channels.insert(
                    ch,
                    ChannelSlot {
                        pipeline: Pipeline::new(PipelineConfig {
                            prebuffer_capacity,
                            viz_enabled: false,
                        }),
                        channel: ch,
                        freq,
                        noise_floor: narrowband_noise,
                        squelch_thresh: narrowband_noise + current_squelch_margin,
                        transmission_count: 0,
                        silence_start: None,
                        mixer_phase: 0.0,
                    },
                );
            }
        }

        // Keep a pipeline alive for the tap channel so its audio (static
        // included) streams continuously, independent of detection.
        if !active_channels.contains_key(&tap_ch) {
            if let Some(freq) = channel_to_freq(tap_ch) {
                let narrowband_noise = -32.0f32;
                active_channels.insert(
                    tap_ch,
                    ChannelSlot {
                        pipeline: Pipeline::new(PipelineConfig {
                            prebuffer_capacity,
                            viz_enabled: false,
                        }),
                        channel: tap_ch,
                        freq,
                        noise_floor: narrowband_noise,
                        squelch_thresh: narrowband_noise + current_squelch_margin,
                        transmission_count: 0,
                        silence_start: None,
                        mixer_phase: 0.0,
                    },
                );
            }
        }

        // Process each active channel
        let active_chs: Vec<u8> = active_channels.keys().copied().collect();
        for ch in &active_chs {
            let offset_hz = match channel_offsets.iter().find(|(c, _)| c == ch) {
                Some(&(_, off)) => off,
                None => continue,
            };

            let slot = active_channels.get_mut(ch).unwrap();

            // Extract narrowband IQ for this channel
            let narrowband =
                extract_channel(&wideband, offset_hz, &filter_coeffs, &mut slot.mixer_phase);

            if narrowband.is_empty() {
                continue;
            }

            let ch_label = channel_label(slot.channel);
            let mut ctx = PipelineContext {
                freq: &mut slot.freq,
                display_channel: Some(slot.channel),
                channel_label: Some(ch_label),
                squelch_margin: current_squelch_margin,
                noise_floor: &mut slot.noise_floor,
                squelch_thresh: &mut slot.squelch_thresh,
                log_file: &mut log_file,
                stream_conn: &mut stream_conn,
                stream_source: "gnosis_radio_wideband",
                recording_enabled: current_recording,
                listen_enabled: current_listen,
                viz_config: &viz_off,
                tx: &tx,
                transmission_count: &mut slot.transmission_count,
                broadcaster: Some(broadcaster.clone()),
                entropy_pool: Some(entropy_pool.clone()),
                always_stream: *ch == tap_ch,
            };

            if let Err(e) = slot.pipeline.process_buffer(&narrowband, &mut ctx) {
                eprintln!("Pipeline error on Ch{}: {}", ch, e);
            }

            // Track silence for auto-removal
            if slot.pipeline.is_squelch_open() {
                slot.silence_start = None;
            } else {
                let silence = slot.silence_start.get_or_insert_with(Instant::now);
                if silence.elapsed().as_secs() >= AUTO_RESUME_SCAN_SEC {
                    // Will be removed below
                }
            }
        }

        // Remove channels that have been silent too long (the tap channel is
        // never removed — its continuous stream is the point).
        let bc = &broadcaster;
        active_channels.retain(|ch, slot| {
            if *ch == tap_ch {
                return true;
            }
            if let Some(silence_start) = slot.silence_start {
                if silence_start.elapsed().as_secs() >= AUTO_RESUME_SCAN_SEC {
                    let timestamp = chrono::Utc::now()
                        .format("%Y-%m-%d %H:%M:%S UTC")
                        .to_string();
                    println!(
                        "[{}] << Ch{} ({}) - Silence timeout, releasing",
                        timestamp,
                        ch,
                        channel_label(*ch)
                    );
                    // Broadcast squelch close so web UI updates
                    bc.broadcast(AudioMessage::SquelchEvent {
                        channel: Some(*ch),
                        freq: slot.freq,
                        open: false,
                        signal_db: slot.noise_floor,
                        classification: "TIMEOUT".to_string(),
                    });
                    return false;
                }
            }
            true
        });

        // Status line (when no channels active)
        if active_channels.is_empty() && frame_count % 10 == 0 {
            print!(
                "\rMonitoring {} channels | Noise floor: {:.1} dB    ",
                channel_offsets.len(),
                noise_floor_db
            );
            std::io::stdout().flush().ok();
        }
    }

    running.store(false, Ordering::Relaxed);
    println!("\nWideband monitor stopped");
    let _ = audio_handle.join();
    Ok(())
}

fn bytes_to_complex(buffer: &[u8]) -> Vec<Complex32> {
    buffer
        .chunks_exact(2)
        .map(|chunk| {
            let i = (chunk[0] as f32 - 127.5) / 127.5;
            let q = (chunk[1] as f32 - 127.5) / 127.5;
            Complex32::new(i, q)
        })
        .collect()
}
