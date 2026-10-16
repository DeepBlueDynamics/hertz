//! Tools: transcripts, activity, wait_for_call. (Phase 1, Confidential Turkey.)

use std::collections::HashSet;
use std::time::Duration;

use serde_json::{json, Value};

use super::{args, ready, AgentSpec, OpResult, Tool};
use crate::runtime::DaemonState;
use hertz_types::Event;

pub const TOOLS: &[Tool] = &[
    Tool {
        name: "transcripts",
        description: "Recent transcripts, newest first, filtered by `channel`, \
            `since_minutes`, a `contains` text match and `limit`. Each is the \
            text heard on a channel with its time, frequency and channel.",
        read_only: true,
        input_schema: transcripts_schema,
        call: |s, a| ready(transcripts(&s, a)),
        agent: Some(|_| AgentSpec {
            label: "recent calls".into(),
            when: "Tell what calls were missed or what was heard on a channel".into(),
            questions: vec![],
            reply: "{result.say}".into(),
        }),
    },
    Tool {
        name: "activity",
        description: "Recent squelch opens and closes with signal level, \
            classification and duration, newest first.",
        read_only: true,
        input_schema: activity_schema,
        call: |s, a| ready(activity(&s, a)),
        agent: None,
    },
    Tool {
        name: "wait_for_call",
        description: "Block up to `timeout_s` (at most 120) for the next \
            transcript, optionally on given `channels`; return it, or null on \
            timeout. Lets an agent listen without polling.",
        read_only: true,
        input_schema: wait_for_call_schema,
        call: |s, a| Box::pin(wait_for_call(s, a)),
        agent: None,
    },
];

fn transcripts_schema(_: &DaemonState) -> Value {
    json!({
        "type": "object",
        "properties": {
            "channel": { "type": "string", "description": "A channel id (see `channels`)." },
            "since_minutes": { "type": "number", "description": "Only transcripts this recent." },
            "limit": { "type": "integer", "minimum": 1, "description": "How many, newest first." },
            "contains": { "type": "string", "description": "Only transcripts whose text contains this (case-insensitive)." }
        }
    })
}

fn activity_schema(_: &DaemonState) -> Value {
    json!({
        "type": "object",
        "properties": {
            "since_minutes": { "type": "number", "description": "Only activity this recent." },
            "limit": { "type": "integer", "minimum": 1, "description": "How many, newest first." }
        }
    })
}

fn wait_for_call_schema(_: &DaemonState) -> Value {
    json!({
        "type": "object",
        "properties": {
            "timeout_s": { "type": "integer", "minimum": 1, "maximum": 120, "description": "Seconds to wait (default 60)." },
            "channels": { "type": "array", "items": { "type": "string" }, "description": "Only these channel ids." }
        }
    })
}

#[derive(serde::Deserialize, Default)]
struct TranscriptsQuery {
    channel: Option<String>,
    since_minutes: Option<f64>,
    limit: Option<usize>,
    contains: Option<String>,
}

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `transcripts`: newest-first, filtered.
pub fn transcripts(state: &DaemonState, a: Value) -> OpResult {
    let q: TranscriptsQuery = args(a)?;
    let now = unix_now();
    let since = q.since_minutes.map(|m| now - m * 60.0);
    let needle = q.contains.as_deref().map(|s| s.to_lowercase());

    let mut list: Vec<hertz_types::wire::TranscriptEntry> = state
        .history
        .transcripts()
        .into_iter()
        .filter(|t| since.map_or(true, |s| t.ts_sec as f64 >= s))
        .filter(|t| {
            q.channel
                .as_ref()
                .map_or(true, |c| t.channel.as_deref() == Some(c.as_str()))
        })
        .filter(|t| {
            needle
                .as_ref()
                .map_or(true, |n| t.text.to_lowercase().contains(n.as_str()))
        })
        .collect();
    list.sort_by(|a, b| {
        b.ts_sec
            .partial_cmp(&a.ts_sec)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if let Some(n) = q.limit {
        list.truncate(n);
    }

    let say = match list.first() {
        Some(t) => format!(
            "Last call on channel {}: {}",
            t.channel.as_deref().unwrap_or("unknown"),
            super::speakable(&t.text, 160, "nothing heard")
        ),
        None => "Nothing heard yet.".to_string(),
    };
    let last = list.first().map(|t| t.text.clone()).unwrap_or_default();
    Ok(json!({
        "transcripts": list,
        "count": list.len(),
        "last": last,
        "say": say,
    }))
}

#[derive(serde::Deserialize, Default)]
struct ActivityQuery {
    since_minutes: Option<f64>,
    limit: Option<usize>,
}

/// `activity`: newest-first, filtered.
pub fn activity(state: &DaemonState, a: Value) -> OpResult {
    let q: ActivityQuery = args(a)?;
    let now = unix_now();
    let since = q.since_minutes.map(|m| now - m * 60.0);

    let mut list: Vec<hertz_types::wire::ActivityEntry> = state
        .history
        .activity()
        .into_iter()
        .filter(|e| since.map_or(true, |s| e.ts_sec as f64 >= s))
        .collect();
    list.sort_by(|a, b| {
        b.ts_sec
            .partial_cmp(&a.ts_sec)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if let Some(n) = q.limit {
        list.truncate(n);
    }
    Ok(json!({ "activity": list, "count": list.len() }))
}

#[derive(serde::Deserialize, Default)]
struct WaitForCallQuery {
    timeout_s: Option<u64>,
    channels: Option<Vec<String>>,
}

/// `wait_for_call`: block for the next transcript on the bus (or null on timeout).
pub async fn wait_for_call(state: DaemonState, a: Value) -> OpResult {
    let q: WaitForCallQuery = args(a)?;
    let timeout_s = q.timeout_s.unwrap_or(60).clamp(1, 120);
    let channels: HashSet<String> = q.channels.unwrap_or_default().into_iter().collect();

    let mut rx = state.bus.subscribe_events();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_s);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            // Timed out.
            Err(_) => return Ok(Value::Null),
            // Channel closed.
            Ok(Err(_)) => return Ok(Value::Null),
            Ok(Ok(Event::Transcription {
                dongle_id,
                channel,
                freq_hz,
                text,
                ..
            })) => {
                if !channels.is_empty()
                    && channel.as_deref().map_or(true, |c| !channels.contains(c))
                {
                    continue;
                }
                return Ok(json!({
                    "ts_sec": unix_now(),
                    "dongle_id": dongle_id,
                    "channel": channel,
                    "freq_hz": freq_hz,
                    "text": text,
                }));
            }
            // Some other event: keep waiting.
            Ok(Ok(_)) => continue,
        }
    }
}
