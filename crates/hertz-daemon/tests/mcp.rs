//! `/mcp` on a mock daemon (127.0.0.1, ephemeral port): the MCP handshake, the
//! tool list, and `weather_set` turning weather sampling on and off live, seen as
//! the scan loop's `ScanState` events.

mod common;

use std::time::Duration;

use common::{rpc, tool};
use hertz_types::Event;
use serde_json::{json, Value};

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
    let (daemon, listen, _tmp) = common::start().await;
    let mut events = daemon.state.bus.subscribe_events();

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
    assert!(
        names.contains(&"weather_status") && names.contains(&"weather_set"),
        "{names:?}"
    );

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
