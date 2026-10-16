//! Operations: every hertzd function an agent or a person can call, in one
//! registry that `/mcp`, REST (`/api/tools/:name`) and the radio agent all use.
//!
//! A [`Tool`] is a thin wrapper over a function that does the work: a name, a
//! description, a JSON Schema, and `call`. A tool may also carry an
//! [`AgentSpec`]: the multiple-choice questions ollaya answers to pick the tool
//! and fill its arguments, and a stock spoken reply. It's published in
//! `tools/list` as `_meta["hertz/decide"]`.
//!
//! Adding a tool: write it in the module for its area (`ops/<area>.rs`), add it
//! to that module's `TOOLS`, and the registry, MCP, REST and the conformance test
//! pick it up.

pub mod history;
pub mod receiver;
pub mod recordings;
pub mod status;
pub mod weather;

use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Value};

use crate::runtime::DaemonState;

/// A result field holding extra MCP content blocks, such as
/// `{"type": "audio", "data": <base64>, "mimeType": "audio/wav"}`. MCP sends them
/// as content beside the JSON; REST returns the field as it is.
pub const CONTENT_KEY: &str = "_content";

/// Recording audio goes out in pieces of at most this many bytes.
pub const CHUNK_BYTES: u64 = 2 * 1024 * 1024;

/// What a tool returns: JSON for the caller, or an [`OpError`].
pub type OpResult = Result<Value, OpError>;
pub type OpFuture = Pin<Box<dyn Future<Output = OpResult> + Send>>;

/// One callable operation.
pub struct Tool {
    /// Plain snake_case, no prefix (clients namespace by server).
    pub name: &'static str,
    /// One or two sentences for an agent: what it does and what it returns.
    pub description: &'static str,
    /// Changes nothing (MCP `readOnlyHint`).
    pub read_only: bool,
    /// The JSON Schema of the arguments; may list live values (channel ids).
    pub input_schema: fn(&DaemonState) -> Value,
    pub call: fn(DaemonState, Value) -> OpFuture,
    /// How ollaya picks this tool and its arguments; `None` keeps it away from
    /// the radio agent.
    pub agent: Option<fn(&DaemonState) -> AgentSpec>,
}

/// Wrap a synchronous op as a tool `call`.
pub fn ready(r: OpResult) -> OpFuture {
    Box::pin(std::future::ready(r))
}

/// An op's failure, phrased for the caller. A bad argument says what is
/// accepted, so a small model can fix it from the message alone.
#[derive(Debug, Clone, PartialEq)]
pub enum OpError {
    /// Bad or missing arguments (HTTP 400).
    BadArgs(String),
    /// The named thing doesn't exist (HTTP 404).
    NotFound(String),
    /// It can't be done in this setup or right now (HTTP 409).
    Unavailable(String),
}

impl OpError {
    pub fn message(&self) -> &str {
        match self {
            OpError::BadArgs(m) | OpError::NotFound(m) | OpError::Unavailable(m) => m,
        }
    }

    pub fn http_status(&self) -> u16 {
        match self {
            OpError::BadArgs(_) => 400,
            OpError::NotFound(_) => 404,
            OpError::Unavailable(_) => 409,
        }
    }
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

/// Parse a tool's arguments into its typed request; the error names the fields.
pub fn args<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, OpError> {
    let v = if v.is_null() { json!({}) } else { v };
    serde_json::from_value(v).map_err(|e| OpError::BadArgs(format!("bad arguments: {e}")))
}

/// How the radio agent decides on a tool with ollaya, which only answers
/// multiple-choice questions.
#[derive(Clone, Debug, serde::Serialize)]
pub struct AgentSpec {
    /// When this tool is the right one: its criterion in the `tool` question.
    pub when: String,
    /// One choice question per argument, asked together after the tool is picked.
    pub questions: Vec<Question>,
    /// The stock spoken reply. `{arg}` is the picked option's `say` for that
    /// argument; `{result.field}` is a top-level field of the tool's result.
    pub reply: String,
}

/// A choice question that fills one argument.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Question {
    pub arg: String,
    pub options: Vec<Choice>,
}

/// One option: its label (what ollaya returns), when it is right, the argument
/// value it becomes, and how the reply says it.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Choice {
    pub label: String,
    pub when: String,
    /// `Null` leaves the argument out.
    pub value: Value,
    pub say: String,
}

impl Choice {
    pub fn new(label: impl Into<String>, when: impl Into<String>, value: Value) -> Self {
        let label = label.into();
        Self {
            say: label.clone(),
            label,
            when: when.into(),
            value,
        }
    }

    pub fn say(mut self, say: impl Into<String>) -> Self {
        self.say = say.into();
        self
    }
}

impl Question {
    pub fn new(arg: impl Into<String>, options: Vec<Choice>) -> Self {
        Self {
            arg: arg.into(),
            options,
        }
    }

    /// The ollaya `criteria` map: label → when.
    pub fn criteria(&self) -> std::collections::BTreeMap<String, String> {
        self.options
            .iter()
            .map(|o| (o.label.clone(), o.when.clone()))
            .collect()
    }

    pub fn option(&self, label: &str) -> Option<&Choice> {
        self.options.iter().find(|o| o.label == label)
    }
}

impl AgentSpec {
    /// The arguments for the picked labels (argument name → label). Unknown
    /// labels are an error; `Null` values are left out.
    pub fn build_args(
        &self,
        picks: &std::collections::BTreeMap<String, String>,
    ) -> Result<Value, String> {
        let mut out = serde_json::Map::new();
        for q in &self.questions {
            let label = picks
                .get(&q.arg)
                .ok_or_else(|| format!("no answer for {:?}", q.arg))?;
            let opt = q
                .option(label)
                .ok_or_else(|| format!("{label:?} is not an option for {:?}", q.arg))?;
            if !opt.value.is_null() {
                out.insert(q.arg.clone(), opt.value.clone());
            }
        }
        Ok(Value::Object(out))
    }

    /// The reply with `{arg}` and `{result.field}` filled in. A field that is
    /// missing is left as it is, so a test can spot it.
    pub fn fill_reply(
        &self,
        picks: &std::collections::BTreeMap<String, String>,
        result: &Value,
    ) -> String {
        let mut out = self.reply.clone();
        for q in &self.questions {
            if let Some(o) = picks.get(&q.arg).and_then(|l| q.option(l)) {
                out = out.replace(&format!("{{{}}}", q.arg), &o.say);
            }
        }
        if let Some(obj) = result.as_object() {
            for (k, v) in obj {
                let text = match v {
                    Value::String(s) => s.clone(),
                    Value::Null => continue,
                    other => other.to_string(),
                };
                out = out.replace(&format!("{{result.{k}}}"), &text);
            }
        }
        out
    }
}

/// Every tool, in a stable order.
pub fn registry() -> impl Iterator<Item = &'static Tool> {
    status::TOOLS
        .iter()
        .chain(history::TOOLS)
        .chain(recordings::TOOLS)
        .chain(receiver::TOOLS)
        .chain(weather::TOOLS)
}

pub fn find(name: &str) -> Option<&'static Tool> {
    registry().find(|t| t.name == name)
}

/// Run a tool by name.
pub async fn call(state: &DaemonState, name: &str, args: Value) -> OpResult {
    let tool = find(name).ok_or_else(|| {
        let names: Vec<&str> = registry().map(|t| t.name).collect();
        OpError::NotFound(format!(
            "unknown tool {name:?}; tools are {}",
            names.join(", ")
        ))
    })?;
    (tool.call)(state.clone(), args).await
}

/// The MCP `tools/list` entry for a tool.
pub fn describe(state: &DaemonState, tool: &Tool) -> Value {
    let mut v = json!({
        "name": tool.name,
        "description": tool.description,
        "inputSchema": (tool.input_schema)(state),
        "annotations": { "readOnlyHint": tool.read_only },
    });
    if let Some(spec) = tool.agent {
        v["_meta"] = json!({ "hertz/decide": spec(state) });
    }
    v
}

/// An empty-object schema, for tools with no arguments.
pub fn no_args(_: &DaemonState) -> Value {
    json!({ "type": "object", "properties": {} })
}

/// Text short enough to speak in a stock reply: whole words up to `max`
/// characters, with "…" when cut; `empty` when there's nothing.
pub fn speakable(text: &str, max: usize, empty: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return empty.to_string();
    }
    if text.chars().count() <= max {
        return text;
    }
    let mut out = String::new();
    for w in text.split(' ') {
        if out.chars().count() + w.chars().count() + 1 > max {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::speakable;

    #[test]
    fn speakable_cuts_on_words() {
        assert_eq!(speakable("  ", 10, "Nothing."), "Nothing.");
        assert_eq!(speakable("short one", 20, ""), "short one");
        assert_eq!(speakable("one two three four", 10, ""), "one two…");
    }
}
