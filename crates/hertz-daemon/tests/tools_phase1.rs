//! Phase-1 tool tests on a mock daemon (127.0.0.1, ephemeral port): status,
//! doctor, channels, time, transcripts, activity, recordings, recording,
//! wait_for_call, set_squelch, set_recording, set_listen, weather_latest and
//! weather_sample_now.

mod common;

use serde_json::{json, Value};

/// The MCP `tools/call` reply wraps the tool's result in `structuredContent`.
fn sc(result: &Value) -> Value {
    result["structuredContent"].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase1_read_tools() {
    let (daemon, listen, _tmp) = common::start().await;

    let status = sc(&common::tool(&listen, "status", json!({})).await);
    assert!(status["version"].is_string(), "{status}");
    assert_eq!(status["dongles"].as_array().unwrap().len(), 1);
    assert!(status["state"].is_string());
    assert_eq!(status["weather"]["enabled"], false);

    let time = sc(&common::tool(&listen, "time", json!({})).await);
    assert!(time["unix"].as_f64().unwrap() > 0.0);
    assert!(time["iso_utc"].as_str().unwrap().ends_with('Z'));

    let doctor = sc(&common::tool(&listen, "doctor", json!({})).await);
    assert!(doctor["platform"].is_string());
    assert_eq!(doctor["dongles"].as_array().unwrap().len(), 1);

    // Channel 72 is in the mock scan list.
    let chans = sc(&common::tool(&listen, "channels", json!({"ids": ["72"]})).await);
    let chans = chans.as_array().unwrap();
    assert_eq!(chans.len(), 1);
    assert_eq!(chans[0]["id"], "72");
    assert_eq!(chans[0]["in_scan"], true);

    // By group, and an unknown group is a clear error.
    let chans = sc(&common::tool(&listen, "channels", json!({"group": "marine-vhf-us"})).await);
    assert!(!chans.as_array().unwrap().is_empty());
    let bad = common::tool(&listen, "channels", json!({"group": "nope"})).await;
    assert_eq!(bad["isError"], true, "{bad}");

    // Empty history on a silent mock.
    let t = sc(&common::tool(&listen, "transcripts", json!({})).await);
    assert_eq!(t["count"], 0);
    assert_eq!(t["last"], "");
    let a = sc(&common::tool(&listen, "activity", json!({})).await);
    assert_eq!(a["count"], 0);

    // No recordings on the mock.
    let r = sc(&common::tool(&listen, "recordings", json!({})).await);
    assert_eq!(r["count"], 0);
    let rec = common::tool(&listen, "recording", json!({"file": "nope.wav"})).await;
    assert_eq!(rec["isError"], true, "{rec}");

    // wait_for_call times out to null.
    let w = sc(&common::tool(&listen, "wait_for_call", json!({"timeout_s": 1})).await);
    assert!(w.is_null(), "{w}");

    // weather_latest empty; weather_sample_now refused (weather off on the mock).
    let wl = sc(&common::tool(&listen, "weather_latest", json!({})).await);
    assert_eq!(wl["count"], 0);
    let ws = common::tool(&listen, "weather_sample_now", json!({})).await;
    assert_eq!(ws["isError"], true, "{ws}");

    daemon.shutdown();
    daemon.join().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase1_receiver_controls() {
    let (daemon, listen, _tmp) = common::start().await;

    // One dongle, so the dongle argument is optional.
    let s = sc(&common::tool(&listen, "set_squelch", json!({"squelch_db": 15.0})).await);
    assert_eq!(s["squelch_db"], 15.0, "{s}");

    let s = sc(&common::tool(&listen, "set_recording", json!({"record": true})).await);
    assert_eq!(s["recording"], true);
    let s = sc(&common::tool(&listen, "set_listen", json!({"listen": true})).await);
    assert_eq!(s["listening"], true);

    // An unknown dongle names the valid ids.
    let bad = common::tool(
        &listen,
        "set_squelch",
        json!({"dongle": "NOPE", "squelch_db": 5.0}),
    )
    .await;
    assert_eq!(bad["isError"], true, "{bad}");
    let msg = bad["content"][0]["text"].as_str().unwrap();
    assert!(msg.contains("MOCK01"), "{msg}");

    daemon.shutdown();
    daemon.join().await;
}
