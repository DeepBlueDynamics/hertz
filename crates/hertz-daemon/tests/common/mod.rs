//! A mock daemon for tool tests: one monitor dongle on marine 72 at silence,
//! weather settings kept but sampling off, on 127.0.0.1 with an ephemeral port.
#![allow(dead_code)]

use std::sync::Arc;

use hertz_daemon::{Daemon, MockFactory};
use hertz_types::{DaemonConfig, DaemonSettings, DongleConfig, DongleRole, WeatherConfig};
use serde_json::{json, Value};

pub fn free_port() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    addr.to_string()
}

pub fn config(listen: &str, data_dir: &std::path::Path) -> DaemonConfig {
    DaemonConfig {
        daemon: DaemonSettings {
            listen: listen.to_string(),
            data_dir: data_dir.to_string_lossy().to_string(),
            auth_token: None,
        },
        dongles: vec![DongleConfig {
            serial: "MOCK01".into(),
            role: DongleRole::Monitor,
            bandplan: Some("marine-vhf-us".into()),
            tap_channel: None,
            groups: None,
            dwell_ms: Some(150),
            priority: None,
            squelch_db: 10.0,
            record: false,
            frequency_hz: Some(156_625_000),
            scan: Some(vec!["72".into()]),
            // What hertzd sets without --weather: settings kept, sampling off.
            weather: Some(WeatherConfig {
                channels: vec!["WX1".into()],
                every_minutes: 15,
                sample_seconds: 60,
                first_after_seconds: 0,
                enabled: false,
            }),
            gain_db: None,
        }],
        transcription: None,
        tx: None,
        relay: None,
        lume: None,
    }
}

/// Start the mock daemon; keep the tempdir alive as long as the daemon.
pub async fn start() -> (Daemon, String, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let bandplans = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bandplans");
    let channels = hertz_channels::load_channels(&bandplans, None::<&std::path::PathBuf>).unwrap();
    let listen = free_port();
    // Silence: the squelch stays shut, so the scan loop is idle.
    let factory = MockFactory::from_closure(|_| (128u8, 128u8));
    let daemon = Daemon::start(config(&listen, tmp.path()), Arc::new(factory), channels)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    (daemon, listen, tmp)
}

/// POST a JSON-RPC message to /mcp; (status, body or Null).
pub async fn rpc(listen: &str, msg: Value) -> (u16, Value) {
    let url = format!("http://{listen}/mcp");
    tokio::task::spawn_blocking(move || match ureq::post(&url).send_json(msg) {
        Ok(r) => {
            let status = r.status();
            let body = r.into_string().unwrap_or_default();
            (status, serde_json::from_str(&body).unwrap_or(Value::Null))
        }
        Err(ureq::Error::Status(code, r)) => (code, json!(r.into_string().unwrap_or_default())),
        Err(e) => panic!("POST /mcp: {e}"),
    })
    .await
    .unwrap()
}

/// `tools/call` over /mcp; the JSON-RPC `result`.
pub async fn tool(listen: &str, name: &str, args: Value) -> Value {
    let (status, v) = rpc(
        listen,
        json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":name,"arguments":args}}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    v["result"].clone()
}
