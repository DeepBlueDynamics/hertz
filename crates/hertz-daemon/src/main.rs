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
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("info,hertz_daemon=debug,rtl_sdr_rs=warn")
            }),
        )
        .init();

    let argv: Vec<String> = std::env::args().collect();
    if argv.iter().any(|a| a == "--version" || a == "-V") {
        println!("hertzd {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
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
    let mut config = DaemonConfig::load(config_path.as_deref())?;
    // Optional `--weather[=WX3,...]`: NOAA weather monitoring on every scanning dongle,
    // 3 minutes every 15, the first right away. Channels from the flag, else the
    // config's [dongle.weather], else WX1. Receive is offline while it samples.
    if let Some(flag) = argv
        .iter()
        .find(|a| *a == "--weather" || a.starts_with("--weather="))
    {
        let given: Vec<String> = flag
            .strip_prefix("--weather=")
            .map(|list| {
                list.split(',')
                    .map(|c| c.trim().to_uppercase())
                    .filter(|c| !c.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        apply_weather_flag(&mut config, &given);
    }
    let bandplans_dir = locate_bandplans();
    let channels = hertz_channels::load_channels(&bandplans_dir, None::<&std::path::PathBuf>)?;

    banner(&config, &bandplans_dir, channels.channels.len(), mock_mode);
    let lume = config.lume.clone();

    let daemon = if mock_mode {
        tracing::warn!("--mock mode: synthesizing static IQ, no real dongle");
        Daemon::start(config, Arc::new(mock_factory()), channels).await?
    } else {
        Daemon::start(config, Arc::new(RealFactory), channels).await?
    };

    if listen_mode {
        hertz_daemon::speaker::spawn(&daemon.state.bus);
    }
    hertz_daemon::lume::spawn(&daemon.state.bus, lume, mock_mode);

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

/// `--weather`: 3 minutes every 15 on each scanning monitor dongle, starting now.
fn apply_weather_flag(config: &mut DaemonConfig, given: &[String]) {
    for dc in &mut config.dongles {
        let scanning = dc.role == hertz_types::DongleRole::Monitor
            && dc.scan.as_ref().is_some_and(|s| !s.is_empty());
        if !scanning {
            continue;
        }
        let channels = if !given.is_empty() {
            given.to_vec()
        } else {
            dc.weather
                .as_ref()
                .map(|w| w.channels.clone())
                .unwrap_or_else(|| vec!["WX1".to_string()])
        };
        dc.weather = Some(hertz_types::WeatherConfig {
            channels,
            every_minutes: 15,
            sample_seconds: 180,
            first_after_seconds: 0,
        });
    }
}

#[cfg(test)]
mod weather_flag_tests {
    use super::*;

    #[test]
    fn flag_sets_three_minutes_every_fifteen_starting_now() {
        let toml = r#"
            [daemon]
            listen = "127.0.0.1:0"
            [[dongle]]
            serial = "1"
            role = "monitor"
            squelch_db = 10.0
            record = false
            scan = ["72"]
            [dongle.weather]
            channels = ["WX3"]
            every_minutes = 30
            sample_seconds = 8
            [[dongle]]
            serial = "2"
            role = "monitor"
            squelch_db = 10.0
            record = false
        "#;
        let mut config: DaemonConfig = toml::from_str(toml).unwrap();
        apply_weather_flag(&mut config, &[]);
        let w = config.dongles[0].weather.as_ref().unwrap();
        assert_eq!(
            (
                w.channels.clone(),
                w.every_minutes,
                w.sample_seconds,
                w.first_after_seconds
            ),
            (vec!["WX3".to_string()], 15, 180, 0)
        );
        assert!(
            config.dongles[1].weather.is_none(),
            "a fixed-frequency dongle doesn't scan, so no weather"
        );
        apply_weather_flag(&mut config, &["WX1".into(), "WX2".into()]);
        assert_eq!(
            config.dongles[0].weather.as_ref().unwrap().channels,
            vec!["WX1", "WX2"]
        );
    }
}
