//! `hertz-panes`: pull every Hyperia pane and tab, hand the layout to ollaya, and
//! ask it which tab and which pane are active. Prints ollaya's answers next to
//! what Hyperia itself reports.

use std::collections::BTreeMap;

use hertz_relay::hyperia::HyperiaClient;
use hertz_relay::ollaya::OllayaClient;

fn main() -> anyhow::Result<()> {
    let layout = HyperiaClient::from_env().layout()?;

    println!("Hyperia layout:");
    for w in &layout.windows {
        println!(
            "  window {}{}",
            w.id,
            if w.focused { " (focused)" } else { "" }
        );
        for t in &w.tabs {
            println!(
                "    tab {:?}{}",
                t.name,
                if t.active { " (active)" } else { "" }
            );
            for p in &t.panes {
                println!(
                    "      pane {:?} [{} {}]{}{}",
                    p.name,
                    p.kind,
                    if p.process.is_empty() {
                        "-"
                    } else {
                        &p.process
                    },
                    if p.active { " active" } else { "" },
                    if p.focused { " focused" } else { "" },
                );
            }
        }
    }

    // The state ollaya sees: names, what each pane runs, and Hyperia's flags, as
    // plain sentences (the classifier reads these slightly better than nested JSON).
    let state = serde_json::Value::String(describe(&layout));

    let mut questions = BTreeMap::new();
    questions.insert(
        "active_tab".to_string(),
        layout
            .windows
            .iter()
            .flat_map(|w| &w.tabs)
            .map(|t| {
                (
                    t.name.clone(),
                    format!("The tab named {:?} is the active tab.", t.name),
                )
            })
            .collect::<BTreeMap<_, _>>(),
    );
    questions.insert(
        "active_pane".to_string(),
        layout
            .panes()
            .map(|(_, t, p)| {
                (
                    p.name.clone(),
                    format!(
                        "The pane named {:?} (in tab {:?}, running {}) is the active pane.",
                        p.name,
                        t.name,
                        if p.process.is_empty() {
                            "nothing"
                        } else {
                            &p.process
                        }
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>(),
    );

    // ollaya needs at least two options per question; a lone tab/pane is trivially it.
    let trivial: BTreeMap<String, String> = questions
        .iter()
        .filter(|(_, opts)| opts.len() == 1)
        .map(|(q, opts)| (q.clone(), opts.keys().next().cloned().unwrap_or_default()))
        .collect();
    questions.retain(|_, opts| opts.len() >= 2);
    let answers = if questions.is_empty() {
        BTreeMap::new()
    } else {
        OllayaClient::from_env().choose(&state, &questions)?
    };

    let truth_tab = layout
        .windows
        .iter()
        .find(|w| w.focused)
        .or(layout.windows.first())
        .and_then(|w| w.tabs.iter().find(|t| t.active))
        .map(|t| t.name.as_str());
    let truth_pane = layout
        .panes()
        .find(|(_, t, p)| p.focused || (t.active && p.active))
        .map(|(_, _, p)| p.name.as_str());

    println!("\nollaya says:");
    for (q, truth) in [("active_tab", truth_tab), ("active_pane", truth_pane)] {
        if let Some(only) = trivial.get(q) {
            println!("  {q}: {only:?} — only one option, not asked");
            continue;
        }
        let a = &answers[q];
        let verdict = match truth {
            Some(t) if t == a.choice => "matches Hyperia",
            Some(_) => "DIFFERS from Hyperia",
            None => "no Hyperia ground truth",
        };
        println!(
            "  {q}: {:?} (confidence {:.3}) — {verdict}{}",
            a.choice,
            a.confidence,
            truth.map(|t| format!(" ({t:?})")).unwrap_or_default()
        );
        let mut probs: Vec<_> = a.probabilities.iter().collect();
        probs.sort_by(|x, y| y.1.total_cmp(x.1));
        for (opt, p) in probs {
            println!("      {p:.4}  {opt}");
        }
    }
    Ok(())
}

/// One sentence per window, tab and pane, e.g.
/// "Pane Top Rabbit: running claude, active, focused."
fn describe(layout: &hertz_relay::hyperia::Layout) -> String {
    let flag = |on: bool, yes: &str| {
        if on {
            format!(", {yes}")
        } else {
            String::new()
        }
    };
    let mut out = Vec::new();
    for w in &layout.windows {
        out.push(format!(
            "Hyperia window {}{}.",
            w.id,
            flag(w.focused, "focused")
        ));
        for t in &w.tabs {
            out.push(format!("Tab {}{}.", t.name, flag(t.active, "active")));
            for p in &t.panes {
                let what = if p.process.is_empty() {
                    "idle shell".to_string()
                } else {
                    format!("running {}", p.process)
                };
                let state = if p.active || p.focused {
                    format!("{}{}", flag(p.active, "active"), flag(p.focused, "focused"))
                } else {
                    ", not active".to_string()
                };
                out.push(format!("Pane {}: {what}{state}.", p.name));
            }
        }
    }
    out.join(" ")
}
