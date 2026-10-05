//! Hyperia layout client: one stateless MCP `tools/call` of `terminal_status`.
//!
//! Endpoint and identity come from the environment Hyperia gives every pane:
//! `HYPERIA_MCP_URL` (default `http://localhost:9800/mcp`) and `HYPERIA_AGENT_TOKEN`.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

const DEFAULT_URL: &str = "http://localhost:9800/mcp";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Layout {
    pub windows: Vec<Window>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Window {
    pub id: u32,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub tabs: Vec<Tab>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Tab {
    pub name: String,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub panes: Vec<Pane>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Pane {
    pub name: String,
    pub pane_id: String,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub kind: String,
    /// Foreground process (e.g. "claude", "hertzd", "pwsh").
    #[serde(default)]
    pub process: String,
}

impl Layout {
    /// Every pane with its window id and tab name.
    pub fn panes(&self) -> impl Iterator<Item = (u32, &Tab, &Pane)> {
        self.windows.iter().flat_map(|w| {
            w.tabs
                .iter()
                .flat_map(move |t| t.panes.iter().map(move |p| (w.id, t, p)))
        })
    }
}

pub struct HyperiaClient {
    url: String,
    token: Option<String>,
}

impl HyperiaClient {
    /// Explicit endpoint and bearer (a daemon uses its own agent identity).
    pub fn new(url: impl Into<String>, token: Option<String>) -> Self {
        Self {
            url: url.into(),
            token: token.filter(|t| !t.is_empty()),
        }
    }

    pub fn from_env() -> Self {
        Self {
            url: std::env::var("HYPERIA_MCP_URL").unwrap_or_else(|_| DEFAULT_URL.to_string()),
            token: std::env::var("HYPERIA_AGENT_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
        }
    }

    /// Fetch the current window/tab/pane layout.
    pub fn layout(&self) -> Result<Layout> {
        let text = self.call_tool("terminal_status", serde_json::json!({}))?;
        serde_json::from_str(&text).context("parse terminal_status")
    }

    /// Send durable mail to a pane (`msg_send`). The first send to a pane asks the
    /// human for consent; Hyperia holds the message and releases it on approval.
    /// Returns Hyperia's reply (operation state) as text.
    pub fn msg_send(&self, pane_id: &str, subject: &str, body: &str) -> Result<String> {
        self.call_tool(
            "msg_send",
            serde_json::json!({ "pane": pane_id, "subject": subject, "body": body }),
        )
    }

    /// Invoke one Hyperia tool; returns its first text content block.
    fn call_tool(&self, name: &str, arguments: serde_json::Value) -> Result<String> {
        let mut req = ureq::post(&self.url)
            .set("content-type", "application/json")
            .set("accept", "application/json, text/event-stream");
        if let Some(t) = &self.token {
            req = req.set("authorization", &format!("Bearer {t}"));
        }
        let body = req
            .send_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": name, "arguments": arguments },
            }))
            .with_context(|| format!("hyperia {} unreachable", self.url))?
            .into_string()?;
        // Streamable HTTP answers as SSE (`data: {...}`) or plain JSON.
        let json = body
            .lines()
            .find_map(|l| l.strip_prefix("data:"))
            .unwrap_or(&body)
            .trim();
        let rpc: serde_json::Value = serde_json::from_str(json).context("parse MCP reply")?;
        if let Some(err) = rpc.get("error") {
            return Err(anyhow!("hyperia {name}: {err}"));
        }
        rpc["result"]["content"][0]["text"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("hyperia {name}: no text content in {rpc}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_terminal_status_shape() {
        let json = r#"{"version":"0.21.6","hints":{},"windows":[{"id":1,"focused":true,"tabs":[
            {"name":"Doughnut","active":true,"panes":[
              {"name":"Top Rabbit","paneId":"a29d","active":true,"focused":true,"kind":"terminal","process":"claude","bspW":100.0},
              {"name":"Correct Alligator","paneId":"29c4","active":false,"kind":"terminal","process":"hertzd"}]}]}]}"#;
        let l: Layout = serde_json::from_str(json).unwrap();
        let panes: Vec<_> = l
            .panes()
            .map(|(_, t, p)| (t.name.as_str(), p.name.as_str(), p.active))
            .collect();
        assert_eq!(
            panes,
            vec![
                ("Doughnut", "Top Rabbit", true),
                ("Doughnut", "Correct Alligator", false)
            ]
        );
    }
}
