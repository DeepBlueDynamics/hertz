mod agentic;
mod audio;
mod broadcast;
mod channels;
mod control;
mod dsp;
mod monitor;
mod pipeline;
mod transcribe;
mod tui;
mod viz;
mod voicepaint;
mod web;
mod wideband;

use channels::{channel_to_freq, freq_to_channel};
use clap::{Parser, Subcommand};
use control::CONTROL_PORT;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use viz::VizConfig;

const WS_PORT: u16 = 9081;

pub(crate) const SDR_RATE: u32 = 240_000;
pub(crate) const BUFFER_SIZE: usize = 48_000;
pub(crate) const DECIMATION_1: usize = 5;
pub(crate) const AUTO_RESUME_SCAN_SEC: u64 = 5;
pub(crate) const AUDIO_SAMPLE_RATE: usize = (SDR_RATE as usize) / DECIMATION_1;
pub(crate) const PREBUFFER_SECONDS: f32 = 1.5;
pub(crate) const AFC_SMOOTHING: f32 = 0.85;
pub(crate) const AFC_THRESHOLD_HZ: f32 = 0.5;
pub(crate) const AFC_MAX_STEP_HZ: f32 = 5000.0;

#[derive(Parser)]
#[command(name = "VHF Monitor")]
#[command(about = "Marine VHF Radio Monitor", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Monitor a single VHF channel or frequency
    Monitor {
        /// Channel number (e.g., 16, 72) - optional if frequency is specified
        #[arg(short, long)]
        channel: Option<u8>,
        /// Frequency in Hz (e.g., 462000000 for 462 MHz) - optional if channel is specified
        #[arg(short, long)]
        frequency: Option<u32>,
        /// RTL-SDR device index (0 = first device)
        #[arg(long, default_value_t = 0)]
        device: u32,
        /// Squelch threshold in dB above noise floor (default: 6.0)
        #[arg(short, long, default_value_t = 6.0)]
        squelch: f32,
        /// Log file for squelch events (default: vhf_monitor.log)
        #[arg(short, long, default_value = "vhf_monitor.log")]
        log: String,
        /// Enable WAV recording of transmissions
        #[arg(short = 'r', long)]
        record: bool,
        /// Enable live audio output to speakers (default: true, use --no-listen to disable)
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        listen: bool,
        /// Enable ASCII visualization (scrolling spectrum/waveforms)
        #[arg(short = 'v', long)]
        viz: bool,
        /// Enable interactive TUI (Terminal User Interface) - real-time spectrum with controls
        #[arg(short = 't', long)]
        tui: bool,
        /// Stream normalized audio frames to host:port (newline-delimited JSON)
        #[arg(long)]
        stream: Option<String>,
    },
    /// Monitor all VHF channels simultaneously using wideband capture
    Scan {
        /// Comma-separated list of channels to exclude (e.g., "70")
        #[arg(short = 'x', long)]
        exclude: Option<String>,
        /// Squelch threshold in dB above noise floor (default: 12.0)
        #[arg(short, long, default_value_t = 12.0)]
        squelch: f32,
        /// RTL-SDR device index (0 = first device)
        #[arg(long, default_value_t = 0)]
        device: u32,
        /// Enable recording of transmissions
        #[arg(short, long, default_value_t = false)]
        record: bool,
        /// Enable live audio output (default: true)
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        listen: bool,
        /// Enable ASCII visualization
        #[arg(short = 'v', long)]
        viz: bool,
        /// Stream normalized audio frames to host:port (newline-delimited JSON)
        #[arg(long)]
        stream: Option<String>,
    },
}

/// Check if required ports are available before starting
fn check_ports() -> Result<(), String> {
    let ports = [(CONTROL_PORT, "HTTP"), (WS_PORT, "WebSocket")];
    let mut conflicts = Vec::new();

    for (port, name) in ports {
        match TcpListener::bind(("localhost", port)) {
            Ok(_) => {} // Port is free, listener dropped immediately
            Err(e) => {
                if e.kind() == std::io::ErrorKind::AddrInUse {
                    conflicts.push((port, name));
                }
            }
        }
    }

    if !conflicts.is_empty() {
        eprintln!("\n  ⚠️  PORT CONFLICT DETECTED");
        eprintln!("  ─────────────────────────");
        for (port, name) in &conflicts {
            eprintln!("  Port {} ({}) is already in use!", port, name);
        }
        eprintln!("\n  To find what's using these ports:");
        for (port, _) in &conflicts {
            eprintln!("    netstat -ano | findstr :{}", port);
        }
        eprintln!("\n  Then kill the process with:");
        eprintln!("    taskkill /F /PID <pid>");
        eprintln!();
        return Err("Ports unavailable".to_string());
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Set terminal tab title with emoji
    print!("\x1b]0;\u{1F419} Meridian Radio\x07");
    println!("\n  \u{1F419}  M E R I D I A N   R A D I O  \u{1F419}");
    println!("  ═══════════════════════════════════");
    println!("  solid-state marine VHF monitor\n");

    // Check ports before doing anything else
    check_ports()?;

    let cli = Cli::parse();

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        print!("\x1b]0;\u{1F419} Meridian Radio [stopped]\x07");
        println!("\n  \u{1F419} Shutting down...");
        r.store(false, Ordering::Relaxed);
    })?;

    match cli.command {
        Commands::Monitor {
            channel,
            frequency,
            device,
            squelch,
            log,
            record,
            listen,
            viz,
            tui,
            stream,
        } => {
            let freq = if let Some(f) = frequency {
                f
            } else if let Some(ch) = channel {
                channel_to_freq(ch).ok_or("Invalid VHF channel")?
            } else {
                return Err("Must specify either --channel or --frequency".into());
            };

            let display_channel = channel.or_else(|| freq_to_channel(freq));

            let viz_config = VizConfig {
                enabled: viz && !tui,
                output_dir: "viz".to_string(),
                width: 60,
                height: 8,
            };

            if tui {
                println!("TUI mode not yet integrated - use --viz for ASCII visualization");
            }

            monitor::monitor_channel_by_freq(
                device,
                freq,
                display_channel,
                squelch,
                &log,
                record,
                listen,
                viz_config,
                stream,
                running,
            )?;
        }
        Commands::Scan {
            exclude,
            squelch,
            device,
            record,
            listen,
            viz,
            stream,
        } => {
            let excluded_channels: Vec<u8> = if let Some(exclude_str) = exclude {
                exclude_str
                    .split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect()
            } else {
                Vec::new()
            };

            let viz_config = VizConfig {
                enabled: viz,
                output_dir: "viz".to_string(),
                width: 60,
                height: 8,
            };

            wideband::wideband_monitor(
                device,
                &excluded_channels,
                squelch,
                "logs/wideband_monitor.log",
                record,
                listen,
                viz_config,
                stream,
                running,
            )?;
        }
    }

    Ok(())
}
