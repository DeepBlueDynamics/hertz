//! `/mcp`: a small MCP server (streamable HTTP, JSON responses, no SSE) on hertzd's
//! own port, so an agent can drive hertz while it runs:
//! `claude mcp add --transport http hertz http://127.0.0.1:9080/mcp`.
//!
//! Tools: `weather_status` and `weather_set` (turn NOAA weather sampling on and
//! off, change its channels and interval). Auth is the REST rule (loopback is
//! exempt; otherwise the bearer token when one is configured), and a browser
//! `Origin` other than localhost is refused, so a web page can't drive it.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};

use crate::runtime::DaemonState;
use crate::weather::WeatherChange;

const PROTOCOL_VERSION: &str = "2025-06-18";

pub async fn post_mcp(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<Value>,
) -> Response {
    if let Err(r) = super::check_auth(&state, &headers, peer) {
        return r;
    }
    if !origin_ok(&headers) {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    match body {
        Value::Array(batch) => {
            let out: Vec<Value> = batch.iter().filter_map(|m| handle(&state, m)).collect();
            if out.is_empty() {
                StatusCode::ACCEPTED.into_response()
            } else {
                Json(Value::Array(out)).into_response()
            }
        }
        m => match handle(&state, &m) {
            Some(r) => Json(r).into_response(),
            None => StatusCode::ACCEPTED.into_response(),
        },
    }
}

/// No server-to-client stream.
pub async fn get_mcp() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        "no SSE stream; POST JSON-RPC",
    )
        .into_response()
}

/// A browser sends `Origin`; only a localhost page may call.
fn origin_ok(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let rest = origin.split("://").nth(1).unwrap_or("");
    let host = if rest.starts_with('[') {
        rest.split_inclusive(']').next().unwrap_or("")
    } else {
        rest.split(['/', ':']).next().unwrap_or("")
    };
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// One JSON-RPC message; `None` for a notification (no reply).
fn handle(state: &DaemonState, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let id = id?;
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "hertzd", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "hertz radio daemon. weather_status shows NOAA weather sampling; \
                weather_set turns it on or off. While a sample is taken (3 minutes) the \
                scan channels are not heard.",
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => call(state, &params),
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    })
}

fn tools() -> Value {
    json!([
        {
            "name": "weather_status",
            "description": "NOAA weather sampling: on or off, channels, minutes between \
                samples, whether a sample is being taken now and when the next is due.",
            "inputSchema": { "type": "object", "properties": {} },
        },
        {
            "name": "weather_set",
            "description": "Turn NOAA weather sampling on or off while hertzd runs. On, it \
                samples each channel for 3 minutes right away and then every \
                every_minutes; receive on the scan channels is offline while it samples. \
                Off stops a sample in progress.",
            "inputSchema": {
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
            },
        },
    ])
}

fn call(state: &DaemonState, params: &Value) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let out = match name {
        "weather_status" => Ok(weather_status(state)),
        "weather_set" => weather_set(state, args),
        _ => return Err((-32602, format!("unknown tool: {name}"))),
    };
    Ok(match out {
        Ok(v) => json!({
            "content": [{ "type": "text", "text": v.to_string() }],
            "structuredContent": v,
        }),
        Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
    })
}

pub fn weather_status(state: &DaemonState) -> Value {
    let mut v = serde_json::to_value(state.weather.get()).unwrap_or_default();
    v["scanning_dongles"] = json!(state.weather.dongles());
    v
}

/// Validate and apply a `weather_set`; the error is for the caller.
pub fn weather_set(state: &DaemonState, args: Value) -> Result<Value, String> {
    let change: WeatherChange =
        serde_json::from_value(args).map_err(|e| format!("bad arguments: {e}"))?;
    if state.weather.dongles() == 0 {
        return Err(
            "no dongle can sample weather (it needs role = \"monitor\" with a scan list)".into(),
        );
    }
    if let Some(chs) = &change.channels {
        for c in chs {
            let known = state
                .channels
                .get_by_id(&c.trim().to_uppercase())
                .is_some_and(|ch| ch.id.starts_with("WX"));
            if !known {
                return Err(format!("{c:?} is not a NOAA weather channel (WX1-WX7)"));
            }
        }
    }
    if change.every_minutes == Some(0) {
        return Err("every_minutes must be at least 1".into());
    }
    state.weather.set(change);
    Ok(weather_status(state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_rule() {
        let h = |o: &str| {
            let mut m = HeaderMap::new();
            m.insert(axum::http::header::ORIGIN, o.parse().unwrap());
            m
        };
        assert!(origin_ok(&HeaderMap::new()));
        assert!(origin_ok(&h("http://localhost:3000")));
        assert!(origin_ok(&h("http://127.0.0.1")));
        assert!(origin_ok(&h("http://[::1]:8080")));
        assert!(!origin_ok(&h("https://evil.example")));
        assert!(!origin_ok(&h("http://localhost.evil.example")));
    }
}
