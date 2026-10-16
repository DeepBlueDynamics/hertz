//! The radio agent against a mock ollaya and a mock Hyperia (127.0.0.1,
//! ephemeral ports) on the mock daemon: the picks become a tool call, the stock
//! reply is spoken, the call sign rotates and is posted, an unsure pick asks to
//! say again, and a call is handled once.

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use hertz_daemon::agent::{Agent, AgentCall};
use hertz_relay::hyperia::HyperiaClient;
use hertz_types::{Event, RelayConfig};
use serde_json::{json, Value};

/// Question name → (choice, confidence) the mock ollaya answers.
type Answers = Arc<Mutex<BTreeMap<String, (String, f64)>>>;
/// Hyperia tool calls seen: (tool, arguments).
type Calls = Arc<Mutex<Vec<(String, Value)>>>;

async fn decide(State(answers): State<Answers>, Json(req): Json<Value>) -> Json<Value> {
    let answers = answers.lock().unwrap();
    let mut out = serde_json::Map::new();
    for name in req["questions"].as_object().unwrap().keys() {
        let (choice, p) = answers.get(name).cloned().unwrap_or(("?".into(), 0.0));
        out.insert(
            name.clone(),
            json!({"type": "choice", "choice": choice, "confidence": p, "probabilities": {}}),
        );
    }
    Json(json!({ "answers": out }))
}

async fn hyperia(State(calls): State<Calls>, Json(req): Json<Value>) -> Json<Value> {
    let name = req["params"]["name"].as_str().unwrap_or("").to_string();
    calls
        .lock()
        .unwrap()
        .push((name.clone(), req["params"]["arguments"].clone()));
    let text = if name == "radio_callsigns" {
        r#"{"callsigns":["Quebec Lima"]}"#
    } else {
        r#"{"ok":true}"#
    };
    Json(
        json!({"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": text}]}}),
    )
}

async fn serve(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

fn spoken(calls: &Calls) -> Vec<String> {
    calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(n, _)| n == "hyperia_spoken_summary")
        .map(|(_, a)| a["text"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_carries_out_a_call_and_rotates() {
    let (daemon, _listen, _tmp) = common::start().await;
    let answers: Answers = Default::default();
    let calls: Calls = Default::default();
    let ollaya_url = serve(
        Router::new()
            .route("/api/decide", post(decide))
            .with_state(answers.clone()),
    )
    .await;
    let hyperia_url = serve(
        Router::new()
            .route("/", post(hyperia))
            .with_state(calls.clone()),
    )
    .await;

    let cfg: RelayConfig = serde_json::from_value(json!({
        "ollaya_url": ollaya_url,
        "agent": true,
        "agent_chat": "Kord Chat",
    }))
    .unwrap();
    let mut events = daemon.state.bus.subscribe_events();
    let state = daemon.state.clone();
    let rt = tokio::runtime::Handle::current();
    let hy = hyperia_url.clone();
    let agent = tokio::task::spawn_blocking(move || {
        let signs = HyperiaClient::new(hy.clone(), None);
        Agent::new(
            state,
            rt,
            &cfg,
            HyperiaClient::new(hy, None),
            Box::new(move || signs.radio_callsigns().unwrap_or_default()),
        )
    })
    .await
    .unwrap();
    let first = agent.callsign();
    assert_ne!(first, "Quebec Lima", "never Hyperia's sign");
    let posted = calls.lock().unwrap().iter().any(|(n, a)| {
        n == "msg_send" && a["pane"] == "Kord Chat" && a["body"].as_str().unwrap().contains(&first)
    });
    assert!(posted, "the call sign goes to the chat");

    let call = |text: &str, rec: &str| AgentCall {
        dongle_id: "d0".into(),
        channel: Some("72".into()),
        freq_hz: 156_625_000,
        text: text.into(),
        recording: Some(rec.into()),
    };
    let run = |c: AgentCall| {
        let agent = Arc::clone(&agent);
        tokio::task::spawn_blocking(move || agent.handle(&c))
    };

    // Weather on, every thirty.
    {
        let mut a = answers.lock().unwrap();
        a.insert("tool".into(), ("switch weather".into(), 0.93));
        a.insert("enabled".into(), ("on".into(), 0.97));
        a.insert("every_minutes".into(), ("30".into(), 0.88));
    }
    let out = run(call("turn the weather on every thirty", "a.wav"))
        .await
        .unwrap();
    assert_eq!(out, "ok");
    let w = daemon.state.weather.get();
    assert!(w.enabled && w.every_minutes == 30, "{w:?}");
    assert_eq!(
        spoken(&calls).last().unwrap(),
        "Confirmed, weather on, every thirty minutes."
    );
    let second = agent.callsign();
    assert_ne!(second, first, "the call sign rotates after a command");

    // The record for Lume: every question with its answer.
    let rec = loop {
        match events.recv().await.unwrap() {
            Event::Agent {
                tool,
                args,
                decisions,
                ..
            } => break (tool, args, decisions),
            _ => continue,
        }
    };
    assert_eq!(rec.0, "weather_set");
    assert_eq!(
        serde_json::from_str::<Value>(&rec.1).unwrap(),
        json!({"enabled": true, "every_minutes": 30})
    );
    let d: Value = serde_json::from_str(&rec.2).unwrap();
    let qs: Vec<&str> = d
        .as_array()
        .unwrap()
        .iter()
        .map(|q| q["question"].as_str().unwrap())
        .collect();
    assert_eq!(qs, ["tool", "enabled", "every_minutes"]);

    // The same transmission again (server follow-up): not carried out twice.
    assert_eq!(
        run(call("turn the weather on every thirty", "a.wav"))
            .await
            .unwrap(),
        "already handled"
    );

    // Unsure about the tool: say again, no rotation.
    answers
        .lock()
        .unwrap()
        .insert("tool".into(), ("switch weather".into(), 0.3));
    let out = run(call("mumble", "b.wav")).await.unwrap();
    assert!(out.starts_with("tool unsure"), "{out}");
    assert_eq!(spoken(&calls).last().unwrap(), "Say again.");
    assert_eq!(agent.callsign(), second);

    // Nothing to do: acknowledged, no rotation.
    answers
        .lock()
        .unwrap()
        .insert("tool".into(), ("nothing".into(), 0.9));
    assert_eq!(
        run(call("radio check", "c.wav")).await.unwrap(),
        "nothing to do"
    );
    assert_eq!(spoken(&calls).last().unwrap(), "Roger.");
    assert_eq!(agent.callsign(), second);

    daemon.shutdown();
    daemon.join().await;
}
