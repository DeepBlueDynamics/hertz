//! How the real ollaya picks tools and arguments from the agent specs, for a
//! set of sample calls. Needs ollaya at OLLAYA_URL (default 127.0.0.1:11435):
//! `cargo test -p hertz-daemon --test agent_live -- --ignored --nocapture`.
//! It prints a table and fails only when ollaya is unreachable; it measures,
//! the models are trained elsewhere.

mod common;

use std::collections::BTreeMap;

use hertz_daemon::agent::{NOTHING, NOTHING_WHEN};
use hertz_daemon::ops;
use hertz_relay::ollaya::OllayaClient;
use serde_json::json;

const CALLS: &[(&str, &str)] = &[
    ("Bravo Kilo, weather every hour please.", "weather_set"),
    (
        "Bravo Kilo, when is the next weather sample?",
        "weather_status",
    ),
    ("Bravo Kilo, start recording.", "set_recording"),
    (
        "Bravo Kilo, too much noise, tighten the squelch.",
        "set_squelch",
    ),
    ("Bravo Kilo, this is Kord, testing.", NOTHING),
    ("Bravo Kilo, mute the speaker.", "set_listen"),
    ("Bravo Kilo, what's the forecast?", "weather_latest"),
    ("Bravo Kilo, anything on channel sixteen?", "transcripts"),
    ("Bravo Kilo, status report.", "status"),
    (
        "Bravo Kilo, turn the weather on every thirty minutes.",
        "weather_set",
    ),
    ("Bravo Kilo, stop the weather.", "weather_set"),
    ("Bravo Kilo, what's the weather?", "weather_latest"),
    (
        "Bravo Kilo, take a weather sample now.",
        "weather_sample_now",
    ),
    ("Bravo Kilo, is weather on?", "weather_status"),
    ("Bravo Kilo, what did I miss?", "transcripts"),
    ("Bravo Kilo, what are you scanning?", "status"),
    ("Bravo Kilo, turn the speaker off.", "set_listen"),
    ("Bravo Kilo, stop recording.", "set_recording"),
    (
        "Bravo Kilo, open the squelch up, I can't hear weak stations.",
        "set_squelch",
    ),
    ("Bravo Kilo, radio check.", NOTHING),
    ("Bravo Kilo, how copy?", NOTHING),
];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn live_ollaya_picks() {
    let (daemon, _listen, _tmp) = common::start().await;
    let state = daemon.state.clone();
    let specs: Vec<(&str, ops::AgentSpec)> = ops::registry()
        .filter_map(|t| t.agent.map(|s| (t.name, s(&state))))
        .collect();
    let mut criteria: BTreeMap<String, String> = specs
        .iter()
        .map(|(_, s)| (s.label.clone(), s.when.clone()))
        .collect();
    criteria.insert(NOTHING.into(), NOTHING_WHEN.into());
    let ollaya = OllayaClient::from_env();
    let specs2 = specs.clone();
    let rows = tokio::task::spawn_blocking(move || {
        let mut rows = Vec::new();
        for (text, want) in CALLS {
            let st = json!(format!("Radio call to Bravo Kilo on channel 72: {text}"));
            let q = BTreeMap::from([("tool".to_string(), criteria.clone())]);
            let a = ollaya.choose(&st, &q).expect("ollaya reachable");
            let pick = &a["tool"];
            let mut args = String::new();
            if let Some((_, spec)) = specs2.iter().find(|(_, s)| s.label == pick.choice) {
                if !spec.questions.is_empty() {
                    let qs: BTreeMap<String, BTreeMap<String, String>> = spec
                        .questions
                        .iter()
                        .map(|q| (q.arg.clone(), q.criteria()))
                        .collect();
                    let ans = ollaya.choose(&st, &qs).expect("ollaya reachable");
                    args = ans
                        .iter()
                        .map(|(k, c)| format!("{k}={} ({:.2})", c.choice, c.confidence))
                        .collect::<Vec<_>>()
                        .join(", ");
                }
            }
            let got = specs2
                .iter()
                .find(|(_, s)| s.label == pick.choice)
                .map(|(n, _)| n.to_string())
                .unwrap_or_else(|| pick.choice.clone());
            rows.push((
                text.to_string(),
                want.to_string(),
                got,
                pick.confidence,
                args,
            ));
        }
        rows
    })
    .await
    .unwrap();
    let mut right = 0;
    for (text, want, got, p, args) in &rows {
        let ok = want == got;
        right += ok as usize;
        println!(
            "{} {got:<20} p={p:.2} want {want:<20} {args}\n    {text}",
            if ok { "OK " } else { "XX " }
        );
    }
    println!("{right}/{} right", rows.len());
    daemon.shutdown();
    daemon.join().await;
}
