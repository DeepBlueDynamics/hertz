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
            when: "The caller asks whether weather sampling is on, or when the next \
                weather sample is"
                .into(),
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
        when: "The caller wants NOAA weather sampling turned on or off, or its \
            interval changed"
            .into(),
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
