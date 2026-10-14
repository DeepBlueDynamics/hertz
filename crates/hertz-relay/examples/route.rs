//! Dry-run routing: which call sign or pane would a transcript go to? Sends nothing.
//! Asks about the same names hertzd does: live call signs, then pane names.
//! `cargo run -p hertz-relay --example route -- "Top Rabbit, come in, over."`
use hertz_relay::hyperia::HyperiaClient;
use hertz_relay::ollaya::OllayaClient;
use hertz_relay::route::{pane_labels, pick_name, terminal_panes};

fn main() -> anyhow::Result<()> {
    let token = std::fs::read_to_string(
        std::path::Path::new(&std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME"))?)
            .join(".hertz/hyperia-token"),
    )
    .ok()
    .map(|t| t.trim().to_string());
    let hyperia = HyperiaClient::new("http://localhost:9800/mcp", token);
    let ollaya = OllayaClient::from_env();
    let signs = hyperia.radio_callsigns().unwrap_or_default();
    let panes = pane_labels(&terminal_panes(&hyperia.layout()?));
    let mut names = signs.clone();
    names.extend(panes.keys().filter(|k| !signs.contains(k)).cloned());
    println!("names: {}", names.join(", "));
    for transcript in std::env::args().skip(1) {
        let pick = pick_name(&names, &ollaya, &transcript, 0.5)?;
        match &pick.name {
            Some(n) if signs.contains(n) => {
                println!("{transcript:?}\n  -> call sign {n} ({})", pick.how())
            }
            Some(n) => println!(
                "{transcript:?}\n  -> {n} [{}] ({})",
                panes[n].pane_id,
                pick.how()
            ),
            None => println!(
                "{transcript:?}\n  -> no match (best {:?}, {})",
                pick.best,
                pick.how()
            ),
        }
    }
    Ok(())
}
