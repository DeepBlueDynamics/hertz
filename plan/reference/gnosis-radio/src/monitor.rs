use std::convert::TryFrom;
use std::fs::OpenOptions;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::{bounded, Sender};
use rtlsdr::DirectSampling;

use crate::agentic;
use crate::audio;
use crate::broadcast::AudioBroadcaster;
use crate::channels::{channel_label, freq_to_channel};
use crate::control::{self, ControlState};
use crate::pipeline::{calc_power_db, create_entropy_pool, Pipeline, PipelineConfig, PipelineContext};
use crate::viz::VizConfig;
use crate::web;
use crate::{AUDIO_SAMPLE_RATE, BUFFER_SIZE, PREBUFFER_SECONDS, SDR_RATE};

pub fn monitor_channel_by_freq(
    device_index: u32,
    freq: u32,
    display_channel: Option<u8>,
    squelch_margin: f32,
    log_path: &str,
    enable_recording: bool,
    enable_listen: bool,
    viz_config: VizConfig,
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

    let control_state = ControlState {
        channel: Arc::new(AtomicU32::new(
            display_channel.map(|c| c as u32).unwrap_or(0),
        )),
        recording: Arc::new(AtomicBool::new(enable_recording)),
        listen: Arc::new(AtomicBool::new(enable_listen)),
        frequency_hz: Arc::new(AtomicU32::new(freq)),
        squelch_margin: Arc::new(Mutex::new(squelch_margin)),
    };

    let agentic_state = agentic::create_agentic_state();
    let entropy_pool = create_entropy_pool();
    agentic::start_agentic_subscriber(agentic_state.clone(), broadcaster.clone(), running.clone());
    control::start_control_server(control_state.clone(), broadcaster.clone(), running.clone(), agentic_state, entropy_pool.clone());
    web::start_websocket_server(broadcaster.clone(), running.clone());

    let mut dev = rtlsdr::open(device_index).map_err(|e| format!("Failed to open SDR: {:?}", e))?;
    dev.set_direct_sampling(DirectSampling::Disabled).ok();
    dev.set_sample_rate(SDR_RATE)
        .map_err(|e| format!("{:?}", e))?;
    if let Err(e) = dev.set_center_freq(freq) {
        eprintln!("  Warning: set_center_freq: {:?} (PLL may not lock — continuing anyway)", e);
    }
    dev.set_tuner_gain_mode(true)
        .map_err(|e| format!("{:?}", e))?;
    dev.set_tuner_gain(496).map_err(|e| format!("{:?}", e))?;
    dev.set_tuner_bandwidth(150_000).ok();
    if let Err(e) = dev.reset_buffer() {
        eprintln!("  Warning: reset_buffer: {:?} (continuing anyway)", e);
    }

    if let Some(ch) = display_channel {
        println!("Monitoring VHF Channel {}", ch);
    } else {
        println!("Monitoring Frequency");
    }
    println!("  Frequency: {:.3} MHz", freq as f64 / 1e6);
    println!("  Auto Squelch: {}dB above noise", squelch_margin);
    println!("  Logging to: {}", log_path);
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

    let mut log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| format!("Failed to open log file: {}", e))?;

    let (tx, rx): (Sender<Vec<f32>>, crossbeam_channel::Receiver<Vec<f32>>) = bounded(20);
    let audio_handle = audio::spawn_audio_thread(rx, running.clone());

    // Flush initial junk from buffer (ignore errors — PLL may not be locked yet)
    let _ = dev.read_sync(BUFFER_SIZE * 2);

    println!("\nCALIBRATION: Reading noise floor...");
    let mut noise_samples = Vec::new();
    for _ in 0..10 {
        if let Ok(buffer) = dev.read_sync(BUFFER_SIZE * 2) {
            noise_samples.push(calc_power_db(&buffer));
        }
    }
    let mut noise_floor = if noise_samples.is_empty() {
        -60.0
    } else {
        noise_samples.iter().copied().sum::<f32>() / noise_samples.len() as f32
    };
    println!("Initial noise floor: {:.1}dB", noise_floor);

    let mut squelch_thresh = noise_floor + squelch_margin;
    let prebuffer_capacity = (AUDIO_SAMPLE_RATE as f32 * PREBUFFER_SECONDS) as usize;
    let mut pipeline = Pipeline::new(PipelineConfig {
        prebuffer_capacity,
        viz_enabled: viz_config.enabled,
    });

    let mut transmission_count = 0u32;
    let mut current_freq = freq;
    let mut current_recording_state = enable_recording;
    let mut current_listen_state = enable_listen;
    let mut current_squelch_margin = squelch_margin;

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

    while running.load(Ordering::Relaxed) {
        let new_freq = control_state.frequency_hz.load(Ordering::Relaxed);
        if new_freq != current_freq {
            println!(
                "\nChanging frequency from {:.3} MHz to {:.3} MHz",
                current_freq as f64 / 1e6,
                new_freq as f64 / 1e6
            );
            if let Err(e) = dev.set_center_freq(new_freq) {
                eprintln!("Failed to change frequency: {:?}", e);
            } else {
                current_freq = new_freq;
                pipeline.reset();
                noise_floor = -60.0;
                squelch_thresh = noise_floor + current_squelch_margin;
                println!("Frequency changed successfully\n");
            }
        }

        let new_recording_state = control_state.recording.load(Ordering::Relaxed);
        if new_recording_state != current_recording_state {
            current_recording_state = new_recording_state;
            println!(
                "\nRecording {}",
                if new_recording_state {
                    "ENABLED"
                } else {
                    "DISABLED"
                }
            );
        }

        let new_listen_state = control_state.listen.load(Ordering::Relaxed);
        if new_listen_state != current_listen_state {
            current_listen_state = new_listen_state;
            println!(
                "\nLive Audio {}",
                if new_listen_state {
                    "ENABLED"
                } else {
                    "DISABLED"
                }
            );
        }

        // Check for squelch margin updates from web UI
        if let Ok(margin) = control_state.squelch_margin.lock() {
            if (*margin - current_squelch_margin).abs() > 0.01 {
                current_squelch_margin = *margin;
                squelch_thresh = noise_floor + current_squelch_margin;
                println!("\nSquelch margin changed to {:.1} dB", current_squelch_margin);
            }
        }

        let buffer = match dev.read_sync(BUFFER_SIZE * 2) {
            Ok(buffer) => buffer,
            Err(e) => {
                eprintln!("Read error: {:?}", e);
                break;
            }
        };

        let active_channel = freq_to_channel(current_freq).or(display_channel);
        let ch_label = active_channel.map(channel_label);

        let mut ctx = PipelineContext {
            freq: &mut current_freq,
            display_channel: active_channel,
            channel_label: ch_label,
            squelch_margin: current_squelch_margin,
            noise_floor: &mut noise_floor,
            squelch_thresh: &mut squelch_thresh,
            log_file: &mut log_file,
            stream_conn: &mut stream_conn,
            stream_source: "gnosis_radio",
            recording_enabled: current_recording_state,
            listen_enabled: current_listen_state,
            viz_config: &viz_config,
            tx: &tx,
            transmission_count: &mut transmission_count,
            broadcaster: Some(broadcaster.clone()),
            entropy_pool: Some(entropy_pool.clone()),
            always_stream: true,
        };

        if let Err(e) = pipeline.process_buffer(&buffer, &mut ctx) {
            eprintln!("Processing error: {}", e);
        }
    }

    running.store(false, Ordering::Relaxed);
    let _ = audio_handle.join();

    Ok(())
}
