//! Relay routing against the real ollaya (OLLAYA_URL, default 127.0.0.1:11435) on
//! labelled radio calls: `cargo test -p hertz-relay --test route_live -- --ignored
//! --nocapture`. Prints a table; fails only when ollaya is unreachable.

use hertz_relay::ollaya::OllayaClient;
use hertz_relay::route::pick_name;

const NAMES: &[&str] = &[
    "Concrete Limpet 🦀",
    "Electric Barracuda 🐟",
    "Low Cobra 🐍",
    "Occasional Slug 👺",
    "Top Rabbit 🧟",
    "Zippy Lungfish 🦦 ",
    "Stable Xerinae 👽",
    "Fond Egret 🦞",
    "Annual Echidna 🧇",
    "Magnificent Vole 🦩",
    "Minor Canid",
    "Quebec Lima",
    "Oscar Sierra",
];

/// (expected name, or "" for no route; transcript)
const CALLS: &[(&str, &str)] = &[
    (
        "Zippy Lungfish 🦦 ",
        "Zippy Lungfish, this is Base Case, come in over.",
    ),
    (
        "Zippy Lungfish 🦦 ",
        "Zippy Lungfish, this is Base, come in over.",
    ),
    (
        "Stable Xerinae 👽",
        "Stables arena, Stables arena, this is base, come in.",
    ),
    ("Fond Egret 🦞", "Fund egret, fund egret, come in."),
    ("Annual Echidna 🧇", "Kidna, Annual Echidna, come back."),
    (
        "Magnificent Vole 🦩",
        "Magnificent vole. Magnificent vole. This is Base Base.",
    ),
    ("Top Rabbit 🧟", "Top rabbit over"),
    ("Oscar Sierra", "Oscar Sierra, this is base, how copy?"),
    ("", "Thank you, have a stable day."),
    ("", "Radio check, radio check, this is base."),
    ("Occasional Slug 👺", "Occasional slug, come in please."),
    ("Low Cobra 🐍", "Hello Cobra, come in."),
    ("Minor Canid", "Minor Canid, Minor Canid, this is base."),
    ("", "Stand by one, I'll call you back."),
];

#[test]
#[ignore]
fn live_routing() {
    let ollaya = OllayaClient::from_env();
    let names: Vec<String> = NAMES.iter().map(|s| s.to_string()).collect();
    let mut right = 0;
    let started = std::time::Instant::now();
    for (want, text) in CALLS {
        let pick = pick_name(&names, &ollaya, text, 0.5).expect("ollaya reachable");
        let got = pick.name.clone().unwrap_or_default();
        let ok = got == *want;
        right += ok as usize;
        println!(
            "{} {:<22} best {:<22} {:<22} | {text}",
            if ok { "OK" } else { "XX" },
            if got.is_empty() {
                "(no route)"
            } else {
                got.as_str()
            },
            pick.best,
            pick.how()
        );
    }
    println!(
        "{right}/{} right in {:.1} s",
        CALLS.len(),
        started.elapsed().as_secs_f32()
    );
}
