//! Second-opinion check on recordings, without sending anything:
//! `cargo run -p hertz-relay --example verify -- data/recordings/x.wav "on-device text"`
//! (`WHISPER_URL` overrides the service, default http://127.0.0.1:8765)
use std::path::Path;

use hertz_relay::ollaya::OllayaClient;
use hertz_relay::verify::{adds_information, TranscriptionService, DEFAULT_MODEL, DEFAULT_URL};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let url = std::env::var("WHISPER_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let service = TranscriptionService::new(url.clone(), DEFAULT_MODEL);
    if !service.is_up() {
        println!("transcription service not running at {url}; nothing to check");
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
