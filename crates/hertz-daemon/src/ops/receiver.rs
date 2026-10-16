//! Tools: set_squelch, set_recording, set_listen. (Phase 1, Confidential Turkey.)

use serde_json::{json, Value};

use super::{args, ready, AgentSpec, Choice, OpError, OpResult, Question, Tool};
use crate::runtime::DaemonState;

pub const TOOLS: &[Tool] = &[
    Tool {
        name: "set_squelch",
        description: "Set the squelch margin in dB over the noise floor \
            (0-30). Lower hears weaker signals; higher is quieter. Applies \
            live to the scan.",
        read_only: false,
        input_schema: set_squelch_schema,
        call: |s, a| ready(set_squelch(&s, a)),
        agent: Some(set_squelch_spec),
    },
    Tool {
        name: "set_recording",
        description: "Turn recording on or off for a dongle.",
        read_only: false,
        input_schema: record_schema,
        call: |s, a| ready(set_recording(&s, a)),
        agent: Some(|_| toggle_spec("record", "recording")),
    },
    Tool {
        name: "set_listen",
        description: "Turn the speaker on or off for a dongle.",
        read_only: false,
        input_schema: listen_schema,
        call: |s, a| ready(set_listen(&s, a)),
        agent: Some(|_| toggle_spec("listen", "the speaker")),
    },
];

/// The `dongle` argument, optional when exactly one dongle is attached.
fn dongle_property() -> Value {
    json!({
        "type": "string",
        "description": "A dongle id; optional when there is only one."
    })
}

fn set_squelch_schema(_: &DaemonState) -> Value {
    json!({
        "type": "object",
        "properties": {
            "dongle": dongle_property(),
            "squelch_db": { "type": "number", "minimum": 0, "maximum": 30, "description": "Margin in dB over the noise floor." }
        },
        "required": ["squelch_db"]
    })
}

fn record_schema(_: &DaemonState) -> Value {
    json!({
        "type": "object",
        "properties": {
            "dongle": dongle_property(),
            "record": { "type": "boolean" }
        },
        "required": ["record"]
    })
}

fn listen_schema(_: &DaemonState) -> Value {
    json!({
        "type": "object",
        "properties": {
            "dongle": dongle_property(),
            "listen": { "type": "boolean" }
        },
        "required": ["listen"]
    })
}

fn toggle_spec(arg: &str, what: &str) -> AgentSpec {
    let arg = arg.to_string();
    let what = what.to_string();
    AgentSpec {
        when: format!("The caller wants {what} turned on or off"),
        questions: vec![Question::new(
            arg.clone(),
            vec![
                Choice::new(
                    "on",
                    format!("The caller wants {what} on or started"),
                    json!(true),
                ),
                Choice::new(
                    "off",
                    format!("The caller wants {what} off or stopped"),
                    json!(false),
                ),
            ],
        )],
        reply: format!("Confirmed, {what} {{{arg}}}."),
    }
}

fn set_squelch_spec(_: &DaemonState) -> AgentSpec {
    AgentSpec {
        when: "The caller wants the squelch changed, or the receiver to hear \
            weaker or fewer signals"
            .into(),
        questions: vec![Question::new(
            "squelch_db",
            vec![
                Choice::new(
                    "5",
                    "Much more sensitive, hear very weak signals",
                    json!(5.0),
                )
                .say("five dB"),
                Choice::new("10", "A little more sensitive", json!(10.0)).say("ten dB"),
                Choice::new("15", "Normal", json!(15.0)).say("fifteen dB"),
                Choice::new("20", "A little quieter", json!(20.0)).say("twenty dB"),
                Choice::new("30", "Quiet, only strong signals", json!(30.0)).say("thirty dB"),
            ],
        )],
        reply: "Confirmed, squelch {squelch_db}.".into(),
    }
}

/// Resolve the optional `dongle` argument to one id.
fn resolve_dongle(state: &DaemonState, dongle: Option<&str>) -> Result<String, OpError> {
    let ids: Vec<String> = state.dongle_summaries().into_iter().map(|d| d.id).collect();
    match (dongle, ids.as_slice()) {
        (Some(d), _) => {
            if ids.iter().any(|i| i == d) {
                Ok(d.to_string())
            } else {
                Err(OpError::BadArgs(format!(
                    "{d:?} is not a dongle; dongles are {}",
                    ids.join(", ")
                )))
            }
        }
        (None, [one]) => Ok(one.clone()),
        (None, []) => Err(OpError::Unavailable("no dongle is attached".into())),
        (None, _) => Err(OpError::BadArgs(format!(
            "dongle is required; dongles are {}",
            ids.join(", ")
        ))),
    }
}

fn summary(state: &DaemonState, id: &str) -> Value {
    state
        .control_for(id)
        .map(|c| serde_json::to_value(c.snapshot().to_summary()).unwrap_or_default())
        .unwrap_or(json!(null))
}

/// `set_squelch`: live squelch margin (clamped 0-30 by the control).
pub fn set_squelch(state: &DaemonState, a: Value) -> OpResult {
    #[derive(serde::Deserialize)]
    struct Q {
        dongle: Option<String>,
        squelch_db: f32,
    }
    let q: Q = args(a)?;
    let id = resolve_dongle(state, q.dongle.as_deref())?;
    let control = state
        .control_for(&id)
        .ok_or_else(|| OpError::NotFound(format!("{id:?} is not a dongle")))?;
    control.set_squelch(q.squelch_db);
    Ok(summary(state, &id))
}

/// `set_recording`: recording on/off.
pub fn set_recording(state: &DaemonState, a: Value) -> OpResult {
    #[derive(serde::Deserialize)]
    struct Q {
        dongle: Option<String>,
        record: bool,
    }
    let q: Q = args(a)?;
    let id = resolve_dongle(state, q.dongle.as_deref())?;
    let control = state
        .control_for(&id)
        .ok_or_else(|| OpError::NotFound(format!("{id:?} is not a dongle")))?;
    control.set_recording(q.record);
    Ok(summary(state, &id))
}

/// `set_listen`: speaker on/off.
pub fn set_listen(state: &DaemonState, a: Value) -> OpResult {
    #[derive(serde::Deserialize)]
    struct Q {
        dongle: Option<String>,
        listen: bool,
    }
    let q: Q = args(a)?;
    let id = resolve_dongle(state, q.dongle.as_deref())?;
    let control = state
        .control_for(&id)
        .ok_or_else(|| OpError::NotFound(format!("{id:?} is not a dongle")))?;
    control.set_listening(q.listen);
    Ok(summary(state, &id))
}
