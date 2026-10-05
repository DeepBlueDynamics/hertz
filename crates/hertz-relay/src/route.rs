//! Radio call → Hyperia pane. ollaya picks which pane a transcript addresses from
//! the live pane list, with an explicit "none of these" option. No fuzzy matching:
//! if ollaya picks "none", or isn't confident, there is no route.

use std::collections::BTreeMap;

use anyhow::Result;

use crate::hyperia::{HyperiaClient, Pane};
use crate::ollaya::OllayaClient;

/// Option label for "the call addresses none of these panes".
pub const NO_PANE: &str = "(none of these panes)";

/// Outcome of routing one transcript.
#[derive(Clone, Debug)]
pub enum Route {
    /// ollaya picked this pane.
    Pane { pane: Pane, probability: f64 },
    /// ollaya picked "none", or its top pick was below the threshold.
    NoMatch { best: String, probability: f64 },
}

/// Ask ollaya which terminal pane `transcript` addresses. A pick below
/// `min_probability` counts as indeterminate.
pub fn route(
    hyperia: &HyperiaClient,
    ollaya: &OllayaClient,
    transcript: &str,
    min_probability: f64,
) -> Result<Route> {
    let layout = hyperia.layout()?;
    // Option label → pane. Duplicate names get the paneId prefix so every option is
    // distinct (and ollaya can't pick one of two identically named panes by name).
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let terminals: Vec<&Pane> = layout
        .panes()
        .map(|(_, _, p)| p)
        .filter(|p| p.kind == "terminal")
        .inspect(|p| *seen.entry(p.name.clone()).or_default() += 1)
        .collect();
    let options: BTreeMap<String, &Pane> = terminals
        .iter()
        .map(|p| {
            let label = if seen[&p.name] > 1 {
                format!("{} ({})", p.name, &p.pane_id[..p.pane_id.len().min(8)])
            } else {
                p.name.clone()
            };
            (label, *p)
        })
        .collect();

    let mut criteria: BTreeMap<String, String> = options
        .keys()
        .map(|label| {
            (
                label.clone(),
                format!("The radio call is addressed to {label:?}."),
            )
        })
        .collect();
    criteria.insert(
        NO_PANE.to_string(),
        "The radio call is not addressed to any of the listed names.".to_string(),
    );
    let mut questions = BTreeMap::new();
    questions.insert("pane".to_string(), criteria);

    let state = serde_json::Value::String(format!("Radio call transcript: {transcript:?}"));
    let answer = ollaya
        .choose(&state, &questions)?
        .remove("pane")
        .expect("choose returns every asked question");
    let probability = answer
        .probabilities
        .get(&answer.choice)
        .copied()
        .unwrap_or(answer.confidence);

    Ok(match options.get(&answer.choice) {
        Some(pane) if probability >= min_probability => Route::Pane {
            pane: (*pane).clone(),
            probability,
        },
        _ => Route::NoMatch {
            best: answer.choice,
            probability,
        },
    })
}
