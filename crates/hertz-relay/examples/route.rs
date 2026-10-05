//! Dry-run routing: which pane would a transcript go to? Sends nothing.
//! `cargo run -p hertz-relay --example route -- "Top Rabbit, come in, over."`
use hertz_relay::hyperia::HyperiaClient;
use hertz_relay::ollaya::OllayaClient;
use hertz_relay::route::{route, Route};

fn main() -> anyhow::Result<()> {
    let token = std::fs::read_to_string(
        std::path::Path::new(&std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME"))?)
            .join(".hertz/hyperia-token"),
    )
    .ok()
    .map(|t| t.trim().to_string());
    let hyperia = HyperiaClient::new("http://localhost:9800/mcp", token);
    let ollaya = OllayaClient::from_env();
    for transcript in std::env::args().skip(1) {
        match route(&hyperia, &ollaya, &transcript, 0.5)? {
            Route::Pane { pane, probability } => {
                println!(
                    "{transcript:?}\n  -> {} [{}] p={probability:.3}",
                    pane.name, pane.pane_id
                )
            }
            Route::NoMatch { best, probability } => {
                println!("{transcript:?}\n  -> no match (best {best:?} p={probability:.3})")
            }
        }
    }
    Ok(())
}
