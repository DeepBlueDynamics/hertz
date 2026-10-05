//! ollaya client: `POST /api/decide` with multiple-choice questions over a state.
//!
//! ollaya serves decision (classification) models, not text generation. A request
//! is a `state` (string or JSON) plus named questions; a `choice` question carries
//! `criteria` mapping each option to a description of when it applies.

use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

pub const DEFAULT_URL: &str = "http://127.0.0.1:11435";
pub const DEFAULT_MODEL: &str = "laya:en";

/// One answered choice question.
#[derive(Clone, Debug, Deserialize)]
pub struct Choice {
    pub choice: String,
    pub confidence: f64,
    #[serde(default)]
    pub probabilities: BTreeMap<String, f64>,
}

pub struct OllayaClient {
    url: String,
    model: String,
}

impl OllayaClient {
    pub fn new(url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            model: model.into(),
        }
    }

    /// `OLLAYA_URL` / `OLLAYA_MODEL`, else the local defaults.
    pub fn from_env() -> Self {
        Self::new(
            std::env::var("OLLAYA_URL").unwrap_or_else(|_| DEFAULT_URL.to_string()),
            std::env::var("OLLAYA_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
        )
    }

    /// Ask several choice questions about one state in a single call.
    /// `questions`: name → (option → description of when that option is right).
    pub fn choose(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, BTreeMap<String, String>>,
    ) -> Result<BTreeMap<String, Choice>> {
        let qs: serde_json::Map<String, serde_json::Value> = questions
            .iter()
            .map(|(name, criteria)| {
                (
                    name.clone(),
                    serde_json::json!({ "type": "choice", "criteria": criteria }),
                )
            })
            .collect();
        let reply: serde_json::Value = ureq::post(&format!("{}/api/decide", self.url))
            .send_json(serde_json::json!({
                "model": self.model,
                "state": state,
                "questions": qs,
            }))
            .map_err(|e| match e {
                ureq::Error::Status(code, r) => {
                    anyhow!("ollaya {code}: {}", r.into_string().unwrap_or_default())
                }
                other => anyhow!("ollaya {} unreachable: {other}", self.url),
            })?
            .into_json()
            .context("parse ollaya reply")?;
        if reply["state_truncated"].as_bool() == Some(true) {
            eprintln!("warning: ollaya truncated the state; answers may miss late panes");
        }
        questions
            .keys()
            .map(|name| {
                let a = reply["answers"][name].clone();
                serde_json::from_value::<Choice>(a)
                    .map(|c| (name.clone(), c))
                    .with_context(|| format!("ollaya gave no answer for {name:?}: {reply}"))
            })
            .collect()
    }
}
