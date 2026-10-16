//! `/mcp`: a small MCP server (streamable HTTP, JSON responses, no SSE) on hertzd's
//! own port, so an agent can drive hertz while it runs:
//! `claude mcp add --transport http hertz http://127.0.0.1:9080/mcp`.
//!
//! Tools come from the registry in [`crate::ops`]. Auth is the REST rule (loopback is
//! exempt; otherwise the bearer token when one is configured), and a browser
//! `Origin` other than localhost is refused, so a web page can't drive it.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};

use crate::ops;
use crate::runtime::DaemonState;

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
            let mut out = Vec::new();
            for m in &batch {
                if let Some(r) = handle(&state, m).await {
                    out.push(r);
                }
            }
            if out.is_empty() {
                StatusCode::ACCEPTED.into_response()
            } else {
                Json(Value::Array(out)).into_response()
            }
        }
        m => match handle(&state, &m).await {
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
async fn handle(state: &DaemonState, msg: &Value) -> Option<Value> {
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
            "instructions": "hertz radio daemon: read what the radio heard and control the \
                scanner, receiver and NOAA weather sampling. While a weather sample is \
                taken the scan channels are not heard.",
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({
            "tools": ops::registry().map(|t| ops::describe(state, t)).collect::<Vec<_>>()
        })),
        "tools/call" => call(state, &params).await,
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    })
}

async fn call(state: &DaemonState, params: &Value) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    if ops::find(name).is_none() {
        return Err((-32602, format!("unknown tool: {name}")));
    }
    let out = ops::call(state, name, args)
        .await
        .map_err(|e| e.to_string());
    Ok(match out {
        Ok(mut v) => {
            // Extra content blocks (an audio chunk) ride in `_content`.
            let extra = v.as_object_mut().and_then(|o| o.remove(ops::CONTENT_KEY));
            let mut content = vec![json!({ "type": "text", "text": v.to_string() })];
            if let Some(Value::Array(blocks)) = extra {
                content.extend(blocks);
            }
            json!({ "content": content, "structuredContent": v })
        }
        Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
    })
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
