//! Radio call → Hyperia pane. ollaya picks which pane a transcript addresses from
//! the live pane list, with an explicit "none of these" option. Only the model
//! decides: if ollaya picks "none", or isn't confident, there is no route.

use std::collections::BTreeMap;

use anyhow::Result;

use crate::hyperia::{HyperiaClient, Layout, Pane};
use crate::ollaya::OllayaClient;

/// Option label for "the call addresses none of these panes".
pub const NO_PANE: &str = "(none of these panes)";

/// Outcome of routing one transcript.
#[derive(Clone, Debug)]
pub enum Route {
    /// Routed to this pane; `how` is ollaya's probability ("p=0.98").
    Pane { pane: Pane, how: String },
    /// ollaya picked "none", or its top pick was below the threshold.
    NoMatch { best: String, probability: f64 },
}

/// Ask ollaya which terminal pane in the live layout `transcript` addresses. A pick
/// below `min_probability` counts as indeterminate.
pub fn route(
    hyperia: &HyperiaClient,
    ollaya: &OllayaClient,
    transcript: &str,
    min_probability: f64,
) -> Result<Route> {
    let panes = terminal_panes(&hyperia.layout()?);
    route_among(&panes, ollaya, transcript, min_probability)
}

/// The terminal panes of a layout (web panes can't receive mail).
pub fn terminal_panes(layout: &Layout) -> Vec<Pane> {
    layout
        .panes()
        .map(|(_, _, p)| p)
        .filter(|p| p.kind == "terminal")
        .cloned()
        .collect()
}

/// Ask ollaya which of `panes` `transcript` addresses.
pub fn route_among(
    panes: &[Pane],
    ollaya: &OllayaClient,
    transcript: &str,
    min_probability: f64,
) -> Result<Route> {
    let options = pane_labels(panes);
    let names: Vec<String> = options.keys().cloned().collect();
    let pick = pick_name(&names, ollaya, transcript, min_probability)?;
    Ok(
        match pick.name.as_ref().and_then(|n| options.get(n).cloned()) {
            Some(pane) => Route::Pane {
                pane,
                how: pick.how(),
            },
            None => Route::NoMatch {
                best: pick.best,
                probability: pick.probability,
            },
        },
    )
}

/// Option label → pane. Duplicate names get the paneId prefix so every label is
/// distinct (and ollaya can't pick one of two identically named panes by name).
pub fn pane_labels(panes: &[Pane]) -> BTreeMap<String, Pane> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for p in panes {
        *seen.entry(p.name.clone()).or_default() += 1;
    }
    panes
        .iter()
        .map(|p| {
            let label = if seen[&p.name] > 1 {
                format!("{} ({})", p.name, &p.pane_id[..p.pane_id.len().min(8)])
            } else {
                p.name.clone()
            };
            (label, p.clone())
        })
        .collect()
}

/// Which of `names` (pane names, radio call signs) the speaker called.
#[derive(Clone, Debug)]
pub struct Pick {
    /// The called name, when ollaya's pick clears `min_probability`.
    pub name: Option<String>,
    /// ollaya's top option (a name or [`NO_PANE`]) and its probability.
    pub best: String,
    pub probability: f64,
    /// The second-chance score for `best` when the first pick fell short.
    pub confirm: Option<f64>,
}

impl Pick {
    /// For logs: "p=0.98", or "p=0.31, confirm 0.57" when the second chance ran.
    pub fn how(&self) -> String {
        match self.confirm {
            Some(c) => format!("p={:.2}, confirm {c:.2}", self.probability),
            None => format!("p={:.2}", self.probability),
        }
    }
}

/// Ask ollaya which of `names` the transcript calls, with an explicit "none of
/// these" option. The model decides, but only among names the transcript says or
/// sounds like.
pub fn pick_name(
    names: &[String],
    ollaya: &OllayaClient,
    transcript: &str,
    min_probability: f64,
) -> Result<Pick> {
    if names.is_empty() {
        return Ok(Pick {
            name: None,
            best: "(no names)".to_string(),
            probability: 0.0,
            confirm: None,
        });
    }
    // ollaya reads each option's description; the label (the exact name) only maps
    // its answer back. Descriptions use the name without emoji: a trailing 🧇
    // dropped "Annual Echidna, Annual Echidna, come in" from 1.00 to 0.38 for that
    // pane. Wording measured on 12 labelled calls: "speaker is calling X by name"
    // 10/12 with no false routes, vs 7/11 for the previous "addressed to" wording.
    let mut criteria: BTreeMap<String, String> = names
        .iter()
        .map(|n| {
            (
                n.clone(),
                format!("The speaker is calling {} by name.", spoken_name(n)),
            )
        })
        .collect();
    criteria.insert(
        NO_PANE.to_string(),
        "The speaker does not call any of these names.".to_string(),
    );
    let mut questions = BTreeMap::new();
    questions.insert("pane".to_string(), criteria);

    // A misheard name ("Stables arena") is invisible to ollaya, which reads text, not
    // sound. When the call-up has a strict sound-alike of a whole name, say so.
    let spoken: Vec<String> = names.iter().map(|n| spoken_name(n)).collect();
    let mut state = format!("Radio call transcript: {transcript:?}");
    let hint = crate::sounds::sounds_like(transcript, &spoken);
    // The option label of the sounds-like name, for a second chance below.
    let hinted: Option<String> = hint
        .as_ref()
        .and_then(|h| names.iter().find(|n| spoken_name(n) == h.name).cloned());
    if let Some(hit) = &hint {
        state.push_str(&format!(
            "\nSpeech recognition often misspells names. In this transcript, {:?} sounds like the name {:?}.",
            hit.phrase, hit.name
        ));
    }
    let state = serde_json::Value::String(state);
    let answer = ollaya
        .choose(&state, &questions)?
        .remove("pane")
        .expect("choose returns every asked question");
    let probability = answer
        .probabilities
        .get(&answer.choice)
        .copied()
        .unwrap_or(answer.confidence);
    // On a garbled call-up ollaya can be confidently wrong, and it leans toward the
    // first option listed ("Thank you, have a stable day." scored 0.81 for whichever
    // pane sorted first). So a pick only counts when the transcript says the name,
    // or the call-up sounds like it.
    let said = |n: &str| says(transcript, n) || hinted.as_deref() == Some(n);
    let picked = answer.choice != NO_PANE && said(&answer.choice);
    // The transcript says exactly one name word for word and ollaya ranks it first:
    // that's the call, whatever the probability. With Hyperia's call signs in the
    // list too, "Zippy Lungfish, this is Base Case, come in over." scored 0.44 and
    // the yes/no second chance 0.48, so a clear call went nowhere.
    let outright: Vec<&String> = names.iter().filter(|n| says(transcript, n)).collect();
    let only_one_said = outright.len() == 1 && *outright[0] == answer.choice;
    if picked && (probability >= min_probability || only_one_said) {
        return Ok(Pick {
            name: Some(answer.choice.clone()),
            best: answer.choice,
            probability,
            confirm: None,
        });
    }
    // Second chance: with a dozen names in play the right one often wins at only
    // 0.3 ("Kidna, Annual Echidna, come back."). Ask about that name alone: the top
    // pick, or otherwise a name the transcript says (ollaya picked "Minor Canid" at
    // 0.57 for "Magnificent vole. Magnificent vole. This is Base Base."), or the name
    // the call-up sounds like ("Fund egret").
    let spelled = names.iter().find(|n| says(transcript, n)).cloned();
    let candidate = match (picked, spelled.or(hinted)) {
        (true, _) => answer.choice.clone(),
        (false, Some(n)) => n,
        (false, None) => {
            return Ok(Pick {
                name: None,
                best: answer.choice,
                probability,
                confirm: None,
            })
        }
    };
    let confirm = confirm_name(ollaya, &state, &candidate)?;
    Ok(Pick {
        name: (confirm >= CONFIRM_MIN).then(|| candidate.clone()),
        best: candidate,
        probability,
        confirm: Some(confirm),
    })
}

/// A top pick below `min_probability` is routed if a two-option question about that
/// name alone clears this. On 14 labelled radio calls, calls that said the name scored
/// 0.34–0.92 and calls that didn't 0.06–0.30.
pub const CONFIRM_MIN: f64 = 0.4;

/// Probability that the transcript in `state` is calling `name`, asked as a
/// two-option choice with no other names competing.
fn confirm_name(ollaya: &OllayaClient, state: &serde_json::Value, name: &str) -> Result<f64> {
    let spoken = spoken_name(name);
    let mut criteria = BTreeMap::new();
    criteria.insert(
        "yes".to_string(),
        format!("The speaker is calling {spoken} by name."),
    );
    criteria.insert(
        "no".to_string(),
        format!("The speaker is not calling {spoken}."),
    );
    let mut questions = BTreeMap::new();
    questions.insert("confirm".to_string(), criteria);
    let answer = ollaya
        .choose(state, &questions)?
        .remove("confirm")
        .expect("choose returns every asked question");
    Ok(answer.probabilities.get("yes").copied().unwrap_or(0.0))
}

/// Whether `transcript` says the option `label` outright: its spoken words in order,
/// ignoring case and punctuation. A "(paneId)" suffix on a duplicate name isn't said.
fn says(transcript: &str, label: &str) -> bool {
    let words = |s: &str| -> Vec<String> {
        s.split(|c: char| !c.is_alphanumeric() && c != '\'')
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .collect()
    };
    let name = spoken_name(label);
    let name = words(name.split(" (").next().unwrap_or(&name));
    !name.is_empty() && words(transcript).windows(name.len()).any(|w| w == name)
}

/// A pane name as it would be said aloud: letters, digits, apostrophes, hyphens and
/// spaces only ("Annual Echidna 🧇" -> "Annual Echidna"). Used only in the text
/// ollaya reads; options are still keyed by the exact pane name.
pub fn spoken_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '\'' | '-' | '(' | ')'))
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spoken_name_drops_emoji() {
        assert_eq!(spoken_name("Annual Echidna 🧇"), "Annual Echidna");
        assert_eq!(
            spoken_name("Top Rabbit 🧟 (a29df980)"),
            "Top Rabbit (a29df980)"
        );
        assert_eq!(spoken_name("O'Brien-Smith"), "O'Brien-Smith");
    }

    #[test]
    fn says_needs_the_whole_name() {
        assert!(says("Annual Echidna, come in.", "Annual Echidna 🧇"));
        assert!(says("echo x-ray, echo X-ray.", "Echo X-ray"));
        assert!(says("Top rabbit over", "Top Rabbit 🧟 (a29df980)"));
        assert!(!says("If scent roll, magnifice and ball", "Echo X-ray"));
        assert!(!says("Echo, come in", "Echo X-ray"));
    }
}
