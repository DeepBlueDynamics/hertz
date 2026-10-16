//! Weather tools: NOAA weather sampling on the scan dongle. The worked example
//! for the registry: one plain tool (`weather_status`) and one with an agent
//! spec (`weather_set`).

use serde_json::{json, Value};

use super::{args, no_args, ready, AgentSpec, Choice, OpError, OpResult, Question, Tool};
use crate::runtime::DaemonState;
use crate::weather::WeatherChange;

pub const TOOLS: &[Tool] = &[
    Tool {
        name: "weather_status",
        description: "NOAA weather sampling: on or off, channels, minutes between \
            samples, whether a sample is being taken now and when the next is due.",
        read_only: true,
        input_schema: no_args,
        call: |s, _| ready(Ok(weather_status(&s))),
        agent: Some(|_| AgentSpec {
            label: "weather status".into(),
            when: "Say whether weather sampling is on".into(),
            questions: vec![],
            reply: "Weather is {result.state}.".into(),
        }),
    },
    Tool {
        name: "weather_set",
        description: "Turn NOAA weather sampling on or off while hertzd runs. On, it \
            samples each channel for 3 minutes right away and then every \
            every_minutes; receive on the scan channels is offline while it samples. \
            Off stops a sample in progress.",
        read_only: false,
        input_schema: weather_set_schema,
        call: |s, a| ready(weather_set(&s, a)),
        agent: Some(weather_set_spec),
    },
    Tool {
        name: "weather_sample_now",
        description: "Take one NOAA weather sample now, keeping the channels and \
            interval. Receive on the scan channels is offline while it samples.",
        read_only: false,
        input_schema: no_args,
        call: |s, _| ready(weather_sample_now(&s)),
        agent: Some(|_| AgentSpec {
            label: "weather sample now".into(),
            when: "Take a weather sample immediately".into(),
            questions: vec![],
            reply: "Taking a weather sample now.".into(),
        }),
    },
    Tool {
        name: "weather_latest",
        description: "The latest weather sample per channel: transcript, signal \
            level, time and the WAV path.",
        read_only: true,
        input_schema: no_args,
        call: |s, _| ready(Ok(weather_latest(&s))),
        agent: Some(|_| AgentSpec {
            label: "weather report".into(),
            when: "Tell what the weather is or what the weather forecast says".into(),
            questions: vec![],
            reply: "{result.say}".into(),
        }),
    },
];

fn weather_set_schema(_: &DaemonState) -> Value {
    json!({
        "type": "object",
        "properties": {
            "enabled": { "type": "boolean" },
            "channels": {
                "type": "array",
                "items": { "type": "string" },
                "description": "NOAA channel ids, WX1-WX7 (WX3 = 162.475 MHz). \
                    Left out: unchanged.",
            },
            "every_minutes": {
                "type": "integer",
                "minimum": 1,
                "description": "Minutes between samples. Left out: unchanged.",
            },
        },
        "required": ["enabled"],
    })
}

fn weather_set_spec(_: &DaemonState) -> AgentSpec {
    let every = |m: u64, words: &str, when: &str| {
        Choice::new(m.to_string(), when, json!(m)).say(format!("every {words} minutes"))
    };
    AgentSpec {
        label: "switch weather".into(),
        when: "Turn the NOAA weather sampling on or off, or change how often it samples".into(),
        questions: vec![
            Question::new(
                "enabled",
                vec![
                    Choice::new(
                        "on",
                        "The caller wants weather turned on or started",
                        json!(true),
                    ),
                    Choice::new(
                        "off",
                        "The caller wants weather turned off or stopped",
                        json!(false),
                    ),
                ],
            ),
            Question::new(
                "every_minutes",
                vec![
                    Choice::new("unchanged", "No interval is mentioned", Value::Null)
                        .say("at the same interval"),
                    every(15, "fifteen", "Every fifteen minutes or a quarter hour"),
                    every(30, "thirty", "Every thirty minutes or half an hour"),
                    Choice::new("60", "Every hour or sixty minutes", json!(60)).say("every hour"),
                ],
            ),
        ],
        reply: "Confirmed, weather {enabled}, {every_minutes}.".into(),
    }
}

pub fn weather_status(state: &DaemonState) -> Value {
    let mut v = serde_json::to_value(state.weather.get()).unwrap_or_default();
    v["scanning_dongles"] = json!(state.weather.dongles());
    // For the spoken reply.
    let on = v["enabled"].as_bool() == Some(true);
    v["state"] = json!(match (on, v["sampling"].as_bool() == Some(true)) {
        (true, true) => "on, sampling now",
        (true, false) => "on",
        _ => "off",
    });
    v
}

/// Validate and apply a `weather_set`.
pub fn weather_set(state: &DaemonState, a: Value) -> OpResult {
    let change: WeatherChange = args(a)?;
    if state.weather.dongles() == 0 {
        return Err(OpError::Unavailable(
            "no dongle can sample weather (it needs role = \"monitor\" with a scan list)".into(),
        ));
    }
    if let Some(chs) = &change.channels {
        for c in chs {
            let known = state
                .channels
                .get_by_id(&c.trim().to_uppercase())
                .is_some_and(|ch| ch.id.starts_with("WX"));
            if !known {
                return Err(OpError::BadArgs(format!(
                    "{c:?} is not a NOAA weather channel; use WX1-WX7"
                )));
            }
        }
    }
    if change.every_minutes == Some(0) {
        return Err(OpError::BadArgs("every_minutes must be at least 1".into()));
    }
    state.weather.set(change);
    Ok(weather_status(state))
}

/// `weather_sample_now`: force an immediate sample, keeping the channels and
/// interval. Re-applying the current settings bumps the epoch, which the scan
/// loop reads as "sample now".
pub fn weather_sample_now(state: &DaemonState) -> OpResult {
    if state.weather.dongles() == 0 {
        return Err(OpError::Unavailable(
            "no dongle can sample weather (it needs role = \"monitor\" with a scan list)".into(),
        ));
    }
    let settings = state.weather.get();
    if !settings.enabled {
        return Err(OpError::Unavailable(
            "weather is off; turn it on with weather_set first".into(),
        ));
    }
    state.weather.set(WeatherChange {
        enabled: true,
        channels: Some(settings.channels),
        every_minutes: Some(settings.every_minutes),
    });
    Ok(weather_status(state))
}

/// `weather_latest`: the latest sample per channel, newest first.
pub fn weather_latest(_: &DaemonState) -> Value {
    let samples = crate::weather::latest_samples();
    let latest = samples
        .first()
        .map(|s| s.transcript.clone())
        .unwrap_or_default();
    let list: Vec<Value> = samples
        .iter()
        .map(|s| {
            json!({
                "channel": s.channel,
                "freq_hz": s.freq_hz,
                "transcript": s.transcript,
                "signal_db": s.signal_db,
                "time_unix_ms": s.unix_ms,
                "wav": s.wav,
            })
        })
        .collect();
    let say = match samples.first() {
        Some(s) => format!(
            "Weather from {}: {}",
            s.channel,
            super::speakable(&s.transcript, 180, "no words heard")
        ),
        None => "No weather sample yet.".to_string(),
    };
    json!({ "samples": list, "count": list.len(), "latest": latest, "say": say })
}
