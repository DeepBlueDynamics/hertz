//! `hertzd` — the Hertz daemon binary. Loads config + bandplans, boots the runtime,
//! and serves on the configured port until ctrl-c. Pass `--mock` to run with a
//! synthetic SDR (no hardware) for smoke-testing the network surface.

use std::sync::Arc;

use hertz_daemon::{Daemon, MockFactory, RealFactory};
use hertz_types::DaemonConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,hertz_daemon=debug")),
        )
        .init();

    let argv: Vec<String> = std::env::args().collect();
    // Optional `--mock`: boot a MockFactory so the binary runs and serves with no
    // hardware attached (smoke-test the network surface / `doctor`).
    let mock_mode = argv.iter().any(|a| a == "--mock");
    // Optional `--listen`: play squelch-gated audio on the local speakers and log
    // squelch activity to the console (vhf_monitor-style, no TUI needed).
    let listen_mode = argv.iter().any(|a| a == "--listen");

    let config_path: Option<String> = argv
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .cloned()
        .or_else(|| std::env::var("HERTZ_CONFIG").ok());
    let config = DaemonConfig::load(config_path.as_deref())?;
    let bandplans_dir = locate_bandplans();
    let channels = hertz_channels::load_channels(&bandplans_dir, None::<&std::path::PathBuf>)?;

    banner(&config, &bandplans_dir, channels.channels.len(), mock_mode);

    let daemon = if mock_mode {
        tracing::warn!("--mock mode: synthesizing static IQ, no real dongle");
        Daemon::start(config, Arc::new(mock_factory()), channels).await?
    } else {
        Daemon::start(config, Arc::new(RealFactory), channels).await?
    };

    if listen_mode {
        hertz_daemon::speaker::spawn(&daemon.state.bus);
    }

    // ctrl-c → graceful shutdown.
    tokio::signal::ctrl_c().await?;
    tracing::info!("ctrl-c received, shutting down");
    daemon.shutdown();
    daemon.join().await;
    tracing::info!("bye");
    Ok(())
}

/// A harmless mock: static mid-range IQ (silence) so the daemon boots and serves.
fn mock_factory() -> MockFactory {
    MockFactory::from_closure(|_| (128u8, 128u8))
}

#[allow(clippy::too_many_lines)]
fn banner(config: &DaemonConfig, bandplans_dir: &std::path::Path, n_channels: usize, mock: bool) {
    println!(
        "\n  hertzd  v{}  — Hertz radio daemon",
        env!("CARGO_PKG_VERSION")
    );
    println!("  ═══════════════════════════════════════════");
    println!("  listen:    {}", config.daemon.listen);
    println!("  data_dir:  {}", config.daemon.data_dir);
    println!(
        "  bandplans: {} ({} channels)",
        bandplans_dir.display(),
        n_channels
    );
    println!(
        "  mode:      {}",
        if mock { "MOCK (no hardware)" } else { "live" }
    );
    println!("  dongles:   {}", config.dongles.len());
    for d in &config.dongles {
        println!("    · {:?} {:?} serial={}", d.role, d.bandplan, d.serial);
    }
    println!();
}

fn locate_bandplans() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("HERTZ_BANDPLANS") {
        return std::path::PathBuf::from(p);
    }
    for candidate in ["./bandplans", "../bandplans", "/etc/hertz/bandplans"] {
        if std::path::Path::new(candidate).is_dir() {
            return std::path::PathBuf::from(candidate);
        }
    }
    std::path::PathBuf::from("./bandplans")
}
