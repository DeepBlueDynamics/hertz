//! Radio call → Hyperia pane. On each transcript, pull the live pane list, let
//! ollaya pick the addressed pane (or "none"), and `msg_send` the transcript there
//! as hertz's own agent identity. Unmatched calls are logged here, nothing is sent.
//!
//! After a send, if the transcription service is up, the recording is
//! re-transcribed there; when ollaya says that version heard something the
//! on-device one missed, a follow-up goes to the same pane.

use std::path::Path;
use std::sync::Arc;

use hertz_relay::hyperia::HyperiaClient;
use hertz_relay::ollaya::OllayaClient;
use hertz_relay::route::{route, Route};
use hertz_relay::verify::{adds_information, TranscriptionService};
use hertz_types::{Event, RelayConfig};
use tracing::{debug, info, warn};

use crate::bus::EventBus;

struct Relay {
    hyperia: HyperiaClient,
    ollaya: OllayaClient,
    verify: Option<TranscriptionService>,
    min_probability: f64,
}

/// One transcript to relay.
struct Call {
    channel: String,
    mhz: f64,
    text: String,
    recording: Option<String>,
}

impl Call {
    fn subject(&self) -> String {
        format!("[radio] Ch {} {:.3} MHz", self.channel, self.mhz)
    }
}

/// Start the relay. Must be called inside the tokio runtime. A missing token file
/// disables the relay with a warning.
pub fn spawn(bus: &EventBus, cfg: &RelayConfig) {
    let token_path = expand_home(&cfg.token_file);
    let token = match std::fs::read_to_string(&token_path) {
        Ok(t) => t.trim().to_string(),
        Err(e) => {
            warn!("relay disabled: can't read token {token_path:?}: {e}");
            return;
        }
    };
    let relay = Arc::new(Relay {
        hyperia: HyperiaClient::new(cfg.hyperia_url.clone(), Some(token)),
        ollaya: OllayaClient::new(cfg.ollaya_url.clone(), cfg.ollaya_model.clone()),
        verify: (!cfg.verify_url.is_empty())
            .then(|| TranscriptionService::new(cfg.verify_url.clone(), cfg.verify_model.clone())),
        min_probability: cfg.min_probability,
    });
    info!("relay: routing transcripts to Hyperia panes via ollaya");

    let mut rx = bus.subscribe_events();
    tokio::spawn(async move {
        loop {
            let ev = match rx.recv().await {
                Ok(ev) => ev,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            };
            let Event::Transcription {
                channel,
                freq_hz,
                text,
                recording,
                ..
            } = ev
            else {
                continue;
            };
            let call = Call {
                channel: channel.unwrap_or_else(|| "?".into()),
                mhz: freq_hz as f64 / 1e6,
                text,
                recording,
            };
            let relay = Arc::clone(&relay);
            tokio::task::spawn_blocking(move || relay.handle(&call));
        }
    });
}

impl Relay {
    fn handle(&self, call: &Call) {
        let ch = &call.channel;
        let pane = match route(
            &self.hyperia,
            &self.ollaya,
            &call.text,
            self.min_probability,
        ) {
            Ok(Route::Pane { pane, probability }) => {
                let body = format!(
                    "Radio call on Ch {ch} ({:.3} MHz), {}:\n\n{}",
                    call.mhz,
                    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
                    call.text
                );
                match self.hyperia.msg_send(&pane.pane_id, &call.subject(), &body) {
                    Ok(reply) => info!(
                        "RELAY | Ch {ch} | -> {} (p={probability:.2}) | {}",
                        pane.name,
                        summarize(&reply)
                    ),
                    Err(e) => {
                        warn!(
                            "RELAY | Ch {ch} | -> {} | msg_send failed: {e:#}",
                            pane.name
                        );
                        return;
                    }
                }
                pane
            }
            Ok(Route::NoMatch { best, probability }) => {
                info!(
                    "RELAY | Ch {ch} | can't find a pane for {:?} (best guess {best:?}, p={probability:.2})",
                    call.text
                );
                return;
            }
            Err(e) => {
                warn!("RELAY | Ch {ch} | routing failed: {e:#}");
                return;
            }
        };
        self.double_check(call, &pane.pane_id, &pane.name);
    }

    /// Second opinion after the send: re-transcribe on the service and follow up
    /// if it heard something the on-device transcript missed.
    fn double_check(&self, call: &Call, pane_id: &str, pane_name: &str) {
        let ch = &call.channel;
        let (Some(service), Some(wav)) = (&self.verify, &call.recording) else {
            return;
        };
        if !service.is_up() {
            debug!("RELAY | Ch {ch} | transcription service down; skipping double-check");
            return;
        }
        let server_text = match service.transcribe(Path::new(wav)) {
            Ok(t) => t,
            Err(e) => {
                warn!("RELAY | Ch {ch} | double-check transcription failed: {e:#}");
                return;
            }
        };
        match adds_information(&self.ollaya, &call.text, &server_text, self.min_probability) {
            Ok((true, p)) => {
                let body = format!(
                    "Follow-up: this call was re-processed on the transcription server and it heard more.\n\nServer transcript:\n{server_text}\n\nOn-device transcript (sent earlier):\n{}",
                    call.text
                );
                match self
                    .hyperia
                    .msg_send(pane_id, &format!("Re: {}", call.subject()), &body)
                {
                    Ok(reply) => info!(
                        "RELAY | Ch {ch} | double-check heard more (p={p:.2}): {server_text:?} -> follow-up to {pane_name} | {}",
                        summarize(&reply)
                    ),
                    Err(e) => warn!("RELAY | Ch {ch} | follow-up msg_send failed: {e:#}"),
                }
            }
            Ok((false, p)) => {
                info!("RELAY | Ch {ch} | double-check agrees (p={p:.2}): {server_text:?}")
            }
            Err(e) => warn!("RELAY | Ch {ch} | double-check compare failed: {e:#}"),
        }
    }
}

/// Hyperia's msg_send reply is JSON; surface its state (queued, delivered,
/// awaiting approval, ...) without dumping the whole object.
fn summarize(reply: &str) -> String {
    serde_json::from_str::<serde_json::Value>(reply)
        .ok()
        .and_then(|v| {
            v.get("state")
                .or_else(|| v.pointer("/operation/state"))
                .or_else(|| v.get("message"))
                .and_then(|s| s.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| reply.chars().take(160).collect())
}

fn expand_home(path: &str) -> std::path::PathBuf {
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(|h| std::path::PathBuf::from(h).join(rest))
            .unwrap_or_else(|| path.into()),
        None => path.into(),
    }
}
