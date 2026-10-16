//! Tools: status, doctor, time, channels. (Phase 1, Confidential Turkey.)

use std::collections::HashSet;

use serde_json::{json, Value};

use super::{args, no_args, ready, AgentSpec, OpError, OpResult, Tool};
use crate::runtime::DaemonState;

pub const TOOLS: &[Tool] = &[
    Tool {
        name: "status",
        description: "What the receiver is doing: version, uptime, each dongle \
            (online, role, frequency, squelch, recording, listening), channels \
            loaded and the weather summary.",
        read_only: true,
        input_schema: no_args,
        call: |s, _| ready(Ok(status(&s))),
        agent: Some(|_| AgentSpec {
            label: "radio status".into(),
            when: "Tell what the radio is scanning or doing".into(),
            questions: vec![],
            reply: "{result.state}.".into(),
        }),
    },
    Tool {
        name: "doctor",
        description: "Runtime health: platform, channels loaded, per-dongle \
            online state and any warnings.",
        read_only: true,
        input_schema: no_args,
        call: |s, _| ready(Ok(doctor(&s))),
        agent: None,
    },
    Tool {
        name: "channels",
        description: "Channels by `group` or `ids`, each with frequency, mode and \
            whether it is in the scan list. With neither, every channel.",
        read_only: true,
        input_schema: channels_schema,
        call: |s, a| ready(channels(&s, a)),
        agent: None,
    },
    Tool {
        name: "time",
        description: "The daemon clock: unix seconds and an ISO-8601 UTC time.",
        read_only: true,
        input_schema: no_args,
        call: |s, _| ready(Ok(time(&s))),
        agent: None,
    },
];

/// `status`: the REST status plus the weather summary and a short spoken `state`.
pub fn status(state: &DaemonState) -> Value {
    let dongles = state.dongle_summaries();
    let state_line = dongles
        .iter()
        .map(|d| {
            format!(
                "{} {:.3} MHz squelch {:.1} dB",
                d.id,
                d.freq_hz as f64 / 1e6,
                d.squelch_db
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let mut v = serde_json::to_value(hertz_types::wire::StatusResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_sec: state.started_at.elapsed().as_secs_f32(),
        dongles,
        channels_loaded: state.channels.channels.len(),
        auth_required: state.auth_token.is_some(),
    })
    .unwrap_or_default();
    v["weather"] = crate::ops::weather::weather_status(state);
    v["state"] = json!(if state_line.is_empty() {
        "no dongle online".to_string()
    } else {
        state_line
    });
    v
}

/// `doctor`: mirror of the REST `/api/doctor`.
pub fn doctor(state: &DaemonState) -> Value {
    let dongles = state
        .dongle_summaries()
        .into_iter()
        .map(|s| hertz_types::wire::DongleDoctorEntry {
            id: s.id,
            serial: s.serial,
            online: s.online,
            dropped_samples: 0,
            read_errors: 0,
            epoch: 0,
        })
        .collect();
    serde_json::to_value(hertz_types::wire::DoctorReport {
        platform: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        channels_loaded: state.channels.channels.len(),
        dongles,
        messages: vec![],
    })
    .unwrap_or_default()
}

fn channels_schema(_: &DaemonState) -> Value {
    json!({
        "type": "object",
        "properties": {
            "group": {
                "type": "string",
                "description": "A band-plan group id."
            },
            "ids": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Channel ids."
            }
        }
    })
}

#[derive(serde::Deserialize, Default)]
struct ChannelsQuery {
    group: Option<String>,
    ids: Option<Vec<String>>,
}

/// Every entry in any dongle's scan list (channel ids or group names).
fn scan_entries(state: &DaemonState) -> HashSet<String> {
    let mut set = HashSet::new();
    for dc in &state.config.dongles {
        for s in dc.scan.iter().flatten().chain(dc.priority.iter().flatten()) {
            set.insert(s.clone());
        }
    }
    set
}

fn in_scan(scan: &HashSet<String>, c: &hertz_types::Channel) -> bool {
    scan.contains(&c.id) || scan.contains(&c.group)
}

fn channel_row(c: &hertz_types::Channel, scan: &HashSet<String>) -> Value {
    json!({
        "id": c.id,
        "name": c.name,
        "freq_hz": c.freq_hz,
        "mode": c.mode,
        "group": c.group,
        "in_scan": in_scan(scan, c),
    })
}

/// `channels`: the band-plan channels filtered by `group` and/or `ids`.
pub fn channels(state: &DaemonState, a: Value) -> OpResult {
    let q: ChannelsQuery = args(a)?;
    let scan = scan_entries(state);

    let ids: Vec<String> = match (&q.group, &q.ids) {
        (Some(g), _) => {
            let group = state.channels.get_by_group(g);
            if group.is_empty() {
                let groups: Vec<&str> = state.channels.groups.keys().map(|s| s.as_str()).collect();
                return Err(OpError::BadArgs(format!(
                    "{g:?} is not a band-plan group; groups are {}",
                    groups.join(", ")
                )));
            }
            group.into_iter().map(|c| c.id.clone()).collect()
        }
        (None, Some(ids)) => ids.clone(),
        (None, None) => state.channels.channels.keys().cloned().collect(),
    };

    let mut out = Vec::new();
    for id in ids {
        match state.channels.get_by_id(&id) {
            Some(c) => out.push(channel_row(c, &scan)),
            None => {
                return Err(OpError::BadArgs(format!(
                    "{id:?} is not a channel id; call channels with no arguments to list them"
                )));
            }
        }
    }
    Ok(json!(out))
}

/// `time`: unix seconds and ISO-8601 UTC.
pub fn time(_: &DaemonState) -> Value {
    let now = chrono::Utc::now();
    json!({
        "unix": now.timestamp() as f64,
        "iso_utc": now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    })
}
