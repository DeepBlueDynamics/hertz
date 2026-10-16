//! `/mcp` on a mock daemon (127.0.0.1, ephemeral port): the MCP handshake, the
//! tool list, and `weather_set` turning weather sampling on and off live, seen as
//! the scan loop's `ScanState` events.

use std::sync::Arc;
use std::time::Duration;

use hertz_daemon::{Daemon, MockFactory};
use hertz_types::{DaemonConfig, DaemonSettings, DongleConfig, DongleRole, Event, WeatherConfig};
use serde_json::{json, Value};

fn free_port() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    addr.to_string()
}

/// POST a JSON-RPC message; (status, body or Null).
async fn rpc(listen: &str, msg: Value) -> (u16, Value) {
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

async fn tool(listen: &str, name: &str, args: Value) -> Value {
    let (status, v) = rpc(
        listen,
        json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":name,"arguments":args}}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    v["result"].clone()
}

/// Wait for a ScanState with this state.
async fn scan_state(rx: &mut tokio::sync::broadcast::Receiver<Event>, want: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Err(_) => panic!("no ScanState {want:?} within 15 s"),
            Ok(Ok(Event::ScanState { state, .. })) if state == want => return,
            Ok(_) => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_weather_set_turns_sampling_on_and_off() {
    let tmp = tempfile::tempdir().unwrap();
    let bandplans = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bandplans");
    let channels = hertz_channels::load_channels(&bandplans, None::<&std::path::PathBuf>).unwrap();
    let listen = free_port();
    let config = DaemonConfig {
        daemon: DaemonSettings {
            listen: listen.clone(),
            data_dir: tmp.path().to_string_lossy().to_string(),
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
    };
    // Silence: the squelch stays shut, so the scan loop is idle.
    let factory = MockFactory::from_closure(|_| (128u8, 128u8));
    let daemon = Daemon::start(config, Arc::new(factory), channels)
        .await
        .unwrap();
    let mut events = daemon.state.bus.subscribe_events();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (status, init) = rpc(
        &listen,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(init["result"]["serverInfo"]["name"], "hertzd");
    let (status, _) = rpc(
        &listen,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    assert_eq!(status, 202, "a notification gets no reply");

    let (_, list) = rpc(
        &listen,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await;
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["weather_status", "weather_set"]);

    let (_, unknown) = rpc(&listen, json!({"jsonrpc":"2.0","id":3,"method":"nope"})).await;
    assert_eq!(unknown["error"]["code"], -32601);

    let s = tool(&listen, "weather_status", json!({})).await;
    let s = &s["structuredContent"];
    assert_eq!(
        (s["enabled"].clone(), s["scanning_dongles"].clone()),
        (json!(false), json!(1))
    );
    assert_eq!(s["channels"], json!(["WX1"]));

    let bad = tool(
        &listen,
        "weather_set",
        json!({"enabled": true, "channels": ["72"]}),
    )
    .await;
    assert_eq!(bad["isError"], true, "72 is not a weather channel: {bad}");

    // On: the first sample starts right away.
    let on = tool(
        &listen,
        "weather_set",
        json!({"enabled": true, "channels": ["wx3"], "every_minutes": 20}),
    )
    .await;
    let s = &on["structuredContent"];
    assert_eq!(
        (
            s["enabled"].clone(),
            s["channels"].clone(),
            s["every_minutes"].clone()
        ),
        (json!(true), json!(["WX3"]), json!(20))
    );
    scan_state(&mut events, "weather").await;
    let s = tool(&listen, "weather_status", json!({})).await;
    assert_eq!(s["structuredContent"]["sampling"], true);

    // Off: the 60 s sample in progress stops and the scan resumes.
    let off = tool(&listen, "weather_set", json!({"enabled": false})).await;
    assert_eq!(off["structuredContent"]["enabled"], false);
    scan_state(&mut events, "scanning").await;
    let s = tool(&listen, "weather_status", json!({})).await;
    let s = &s["structuredContent"];
    assert_eq!(
        (s["sampling"].clone(), s["next_sample_unix_ms"].clone()),
        (json!(false), Value::Null)
    );

    // The REST mirror reads the same settings.
    let url = format!("http://{listen}/api/weather");
    let rest: Value =
        tokio::task::spawn_blocking(move || ureq::get(&url).call().unwrap().into_json().unwrap())
            .await
            .unwrap();
    assert_eq!(
        (rest["enabled"].clone(), rest["every_minutes"].clone()),
        (json!(false), json!(20))
    );

    daemon.shutdown();
    daemon.join().await;
}
