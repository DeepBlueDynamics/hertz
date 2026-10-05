//! Second-opinion check on recordings, without sending anything:
//! `cargo run -p hertz-relay --example verify -- data/recordings/x.wav "on-device text"`
use std::path::Path;

use hertz_relay::ollaya::OllayaClient;
use hertz_relay::verify::{adds_information, TranscriptionService, DEFAULT_MODEL, DEFAULT_URL};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let service = TranscriptionService::new(DEFAULT_URL, DEFAULT_MODEL);
    if !service.is_up() {
        println!("transcription service not running at {DEFAULT_URL}; nothing to check");
        return Ok(());
    }
    let ollaya = OllayaClient::from_env();
    for pair in args.chunks(2) {
        let [wav, first] = pair else { break };
        let second = service.transcribe(Path::new(wav))?;
        let (adds, p) = adds_information(&ollaya, first, &second, 0.5)?;
        println!(
            "{wav}\n  on-device: {first:?}\n  server:    {second:?}\n  -> {} (p={p:.2})",
            if adds { "ADDS INFORMATION" } else { "same" }
        );
    }
    Ok(())
}
