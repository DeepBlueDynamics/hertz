//! Radio call → Hyperia pane. On each transcript, pull Hyperia's live radio call
//! signs ("Quebec Lima") and pane names, let ollaya pick the one the caller used (or
//! "none"), and deliver as hertz's own agent identity: by call sign with
//! `radio_deliver` (which rotates the sign, so a sign heard on the air never works
//! twice), or by pane name with `msg_send`. Signs only exist for panes already
//! approved for hertz, so a new pane is reached by name first. Unmatched calls are
//! logged here; nothing is sent.
//!
//! After a send, if the transcription service is up, the recording is
//! re-transcribed there; when ollaya says that version heard something the
//! on-device one missed, a follow-up goes to the same pane.
//!
//! With live transcription on, the call's live-stream text rides along when its
//! words differ, and is routed in turn when the full-recording text names no one.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hertz_relay::hyperia::{HyperiaClient, Pane};
use hertz_relay::ollaya::OllayaClient;
use hertz_relay::route::{pane_labels, pick_name, spoken_name, terminal_panes};
use hertz_relay::verify::{adds_information, TranscriptionService};
use hertz_types::{Event, RelayConfig};
use tracing::{debug, info, warn};

use crate::bus::EventBus;
use crate::live::LiveTranscripts;
use crate::transcribe::LiveKeywords;

/// How often the pane list is re-read for transcription keywords (Hyperia's advice:
/// poll terminal_status every 2–5 s; it's cheap).
const PANE_NAME_REFRESH: Duration = Duration::from_secs(5);

/// Keep `live` set to the spoken names of the current terminal panes, so the
/// on-device recognizer is biased toward exactly the names a caller can use
/// ("Annual Echidna" instead of "Angel Economic").
fn spawn_pane_name_poller(relay: Arc<Relay>, live: LiveKeywords) {
    std::thread::Builder::new()
        .name("relay-pane-names".into())
        .spawn(move || loop {
            // From the pane memory, so a momentary empty layout keeps the names,
            // plus the live call signs (they rotate on every delivery).
            let mut names: Vec<String> = relay
                .panes()
                .iter()
                .map(|p| spoken_name(&p.name))
                .chain(relay.callsigns())
                .filter(|n| !n.is_empty())
                .collect();
            names.sort();
            names.dedup();
            if let Ok(mut cur) = live.write() {
                if *cur != names {
                    info!(
                        "relay: transcription keywords now {} name(s): {}",
                        names.len(),
                        names.join(", ")
                    );
                    *cur = names;
                }
            }
            std::thread::sleep(PANE_NAME_REFRESH);
        })
        .expect("spawn pane-name poller");
}

struct Relay {
    hyperia: HyperiaClient,
    ollaya: OllayaClient,
    verify: Option<TranscriptionService>,
    verify_model: String,
    min_probability: f64,
    bus: EventBus,
    /// The Hyperia pane hertzd runs in (`HYPERIA_PANE`), never a destination.
    own_pane: Option<String>,
    /// Terminal panes by paneId, with when each was last seen in a layout.
    seen: Mutex<HashMap<String, (Pane, Instant)>>,
    /// Each channel's finished live-stream transcript (empty when live is off).
    live: LiveTranscripts,
}

/// The live transcript of a call is kept until the full-recording transcript
/// arrives; it finishes on squelch close, usually before that. Wait at most this
/// long for it, and ignore one older than LIVE_MAX_AGE (from an earlier call).
const LIVE_WAIT: Duration = Duration::from_secs(2);
const LIVE_MAX_AGE: Duration = Duration::from_secs(10);

/// A pane stays routable this long after it was last seen. Hyperia's pane list
/// briefly came back empty or partial (it dropped every name for ~35 s), and a call
/// in that gap scored "none" for a pane it named outright. A pane that is really
/// gone just fails at msg_send, which is logged.
const PANE_MEMORY: Duration = Duration::from_secs(60);

/// One transcript to relay.
struct Call {
    dongle_id: String,
    channel: String,
    freq_hz: u64,
    mhz: f64,
    text: String,
    recording: Option<String>,
}

impl Call {
    fn subject(&self) -> String {
        format!("[radio] Ch {} {:.3} MHz", self.channel, self.mhz)
    }
}

/// Where ollaya says a call goes.
enum Dest {
    /// A live radio call sign: delivered with `radio_deliver`.
    Sign {
        sign: String,
        how: String,
    },
    /// A pane named outright: delivered with `msg_send`.
    Pane {
        pane: Pane,
        how: String,
    },
    NoMatch {
        best: String,
        probability: f64,
    },
}

/// A delivered call: who got it (for the log), the pane id for a follow-up, and
/// Hyperia's delivery state.
struct Sent {
    to: String,
    pane_id: Option<String>,
    state: String,
}

/// Start the relay. Must be called inside the tokio runtime. A missing token file
/// disables the relay with a warning.
pub fn spawn(
    bus: &EventBus,
    cfg: &RelayConfig,
    live_keywords: LiveKeywords,
    live_transcripts: LiveTranscripts,
) {
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
        verify_model: cfg.verify_model.clone(),
        min_probability: cfg.min_probability,
        bus: bus.clone(),
        own_pane: std::env::var("HYPERIA_PANE").ok().filter(|p| !p.is_empty()),
        seen: Mutex::new(HashMap::new()),
        live: live_transcripts,
    });
    info!("relay: routing transcripts to Hyperia panes via ollaya");
    spawn_pane_name_poller(Arc::clone(&relay), live_keywords);

    let mut rx = bus.subscribe_events();
    tokio::spawn(async move {
        loop {
            let ev = match rx.recv().await {
                Ok(ev) => ev,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            };
            let Event::Transcription {
                dongle_id,
                channel,
                freq_hz,
                text,
                recording,
            } = ev
            else {
                continue;
            };
            let call = Call {
                dongle_id,
                channel: channel.unwrap_or_else(|| "?".into()),
                freq_hz,
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
    /// Refresh the pane memory from the live layout (if Hyperia answers), drop panes
    /// unseen for PANE_MEMORY, and return what is left.
    fn panes(&self) -> Vec<Pane> {
        let fresh = match self.hyperia.layout() {
            Ok(layout) => terminal_panes(&layout)
                .into_iter()
                .filter(|p| !is_hertz_pane(p, self.own_pane.as_deref()))
                .collect(),
            Err(e) => {
                debug!("relay: pane list fetch failed: {e:#}");
                Vec::new()
            }
        };
        let now = Instant::now();
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        for p in fresh {
            seen.insert(p.pane_id.clone(), (p, now));
        }
        seen.retain(|_, (_, at)| now.duration_since(*at) < PANE_MEMORY);
        let mut panes: Vec<Pane> = seen.values().map(|(p, _)| p.clone()).collect();
        panes.sort_by(|a, b| a.name.cmp(&b.name));
        panes
    }

    /// The live call signs, or none when Hyperia has no radio support or refuses.
    fn callsigns(&self) -> Vec<String> {
        self.hyperia.radio_callsigns().unwrap_or_else(|e| {
            debug!("relay: radio_callsigns unavailable: {e:#}");
            Vec::new()
        })
    }

    /// One ollaya question over the live call signs and the pane names together.
    fn route(&self, text: &str) -> anyhow::Result<Dest> {
        let signs = self.callsigns();
        let panes = pane_labels(&self.panes());
        let mut names = signs.clone();
        names.extend(panes.keys().filter(|k| !signs.contains(k)).cloned());
        let pick = pick_name(&names, &self.ollaya, text, self.min_probability)?;
        let how = pick.how();
        Ok(match pick.name {
            Some(n) if signs.contains(&n) => Dest::Sign { sign: n, how },
            Some(n) => match panes.get(&n) {
                Some(pane) => Dest::Pane {
                    pane: pane.clone(),
                    how,
                },
                None => Dest::NoMatch {
                    best: n,
                    probability: pick.probability,
                },
            },
            None => Dest::NoMatch {
                best: pick.best,
                probability: pick.probability,
            },
        })
    }

    /// Deliver to a routed destination.
    fn send(&self, dest: &Dest, subject: &str, body: &str) -> anyhow::Result<Sent> {
        match dest {
            Dest::Sign { sign, how } => {
                let reply = self.hyperia.radio_deliver(sign, subject, body)?;
                Ok(Sent {
                    to: format!("call sign {sign} ({how})"),
                    pane_id: reply_target(&reply),
                    state: summarize(&reply),
                })
            }
            Dest::Pane { pane, how } => {
                let reply = self.hyperia.msg_send(&pane.pane_id, subject, body)?;
                Ok(Sent {
                    to: format!("{} ({how})", pane.name),
                    pane_id: Some(pane.pane_id.clone()),
                    state: summarize(&reply),
                })
            }
            Dest::NoMatch { .. } => Err(anyhow::anyhow!("no destination")),
        }
    }

    /// The live-stream transcript of the call just finished on `channel`, if it
    /// differs from the full-recording one in its words.
    fn live_text(&self, call: &Call) -> Option<String> {
        let deadline = Instant::now() + LIVE_WAIT;
        loop {
            {
                let mut m = self.live.lock().unwrap_or_else(|e| e.into_inner());
                if let Some((text, at)) = m.remove(&call.channel) {
                    if at.elapsed() > LIVE_MAX_AGE || words(&text) == words(&call.text) {
                        return None;
                    }
                    return Some(text);
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Put a routing decision on the bus (Lume records it), whatever the outcome.
    fn report(&self, call: &Call, text: &str, source: &str, to: Option<&str>, outcome: String) {
        self.bus.publish_event(Event::Relayed {
            dongle_id: call.dongle_id.clone(),
            channel: (call.channel != "?").then(|| call.channel.clone()),
            freq_hz: call.freq_hz,
            text: text.to_string(),
            source: source.to_string(),
            to: to.map(str::to_string),
            outcome,
        });
    }

    /// Send to `dest` and report it; the delivery when it went out.
    fn send_reported(
        &self,
        call: &Call,
        text: &str,
        source: &str,
        dest: &Dest,
        body: &str,
    ) -> anyhow::Result<Sent> {
        let result = self.send(dest, &call.subject(), body);
        match &result {
            Ok(sent) => self.report(call, text, source, Some(&sent.to), sent.state.clone()),
            Err(e) => self.report(call, text, source, None, format!("delivery failed: {e:#}")),
        }
        result
    }

    fn handle(&self, call: &Call) {
        let ch = &call.channel;
        let live = self.live_text(call);
        let dest = match self.route(&call.text) {
            Ok(Dest::NoMatch { best, probability }) => {
                info!(
                    "RELAY | Ch {ch} | can't find a call sign or pane for {:?} (best guess {best:?}, p={probability:.2})",
                    call.text
                );
                self.report(
                    call,
                    &call.text,
                    "on-device",
                    None,
                    format!("no match (best guess {best:?}, p={probability:.2})"),
                );
                if let Some(live) = &live {
                    if self.retry_with_live(call, live) {
                        return;
                    }
                }
                self.retry_with_server(call);
                return;
            }
            Ok(dest) => dest,
            Err(e) => {
                warn!("RELAY | Ch {ch} | routing failed: {e:#}");
                self.report(
                    call,
                    &call.text,
                    "on-device",
                    None,
                    format!("routing failed: {e:#}"),
                );
                return;
            }
        };
        let mut body = format!(
            "Radio call on Ch {ch} ({:.3} MHz), {}:\n\n{}",
            call.mhz,
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
            call.text
        );
        if let Some(live) = &live {
            body.push_str(&format!(
                "\n\n(Live transcript, heard while the caller talked: {live})"
            ));
        }
        match self.send_reported(call, &call.text, "on-device", &dest, &body) {
            Ok(sent) => {
                info!("RELAY | Ch {ch} | -> {} | {}", sent.to, sent.state);
                if let Some(pane_id) = &sent.pane_id {
                    self.double_check(call, pane_id, &sent.to);
                }
            }
            Err(e) => warn!("RELAY | Ch {ch} | delivery failed: {e:#}"),
        }
    }

    /// The full-recording transcript named no one; route the live-stream one, which
    /// sometimes hears the call-up the full pass missed. True when it was sent.
    fn retry_with_live(&self, call: &Call, live: &str) -> bool {
        let ch = &call.channel;
        match self.route(live) {
            Ok(Dest::NoMatch { best, probability }) => {
                info!(
                    "RELAY | Ch {ch} | live transcript {live:?} names no call sign or pane either (best guess {best:?}, p={probability:.2})"
                );
                false
            }
            Ok(dest) => {
                let body = format!(
                    "Radio call on Ch {ch} ({:.3} MHz), {}:\n\n{live}\n\n(Routed from the live transcript. Full-recording transcript: {})",
                    call.mhz,
                    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
                    call.text
                );
                match self.send_reported(call, live, "live", &dest, &body) {
                    Ok(sent) => {
                        info!(
                            "RELAY | Ch {ch} | live transcript {live:?} -> {} | {}",
                            sent.to, sent.state
                        );
                        true
                    }
                    Err(e) => {
                        warn!("RELAY | Ch {ch} | delivery failed: {e:#}");
                        false
                    }
                }
            }
            Err(e) => {
                warn!("RELAY | Ch {ch} | routing live transcript failed: {e:#}");
                false
            }
        }
    }

    /// The on-device transcript named no pane. Pane names are unusual words that
    /// Whistle often mishears ("You're echinada" for "Annual Echidna"), so when the
    /// transcription service is up, route its transcript instead. A match is sent
    /// once, from the server text, with the on-device text alongside.
    fn retry_with_server(&self, call: &Call) {
        let ch = &call.channel;
        let Some(server_text) = self.server_transcript(call) else {
            return;
        };
        if server_text.is_empty() {
            info!("RELAY | Ch {ch} | server transcript empty too; nothing sent");
            return;
        }
        match self.route(&server_text) {
            Ok(Dest::NoMatch { best, probability }) => {
                info!(
                    "RELAY | Ch {ch} | server transcript {server_text:?} names no call sign or pane either (best guess {best:?}, p={probability:.2})"
                );
                self.report(
                    call,
                    &server_text,
                    "server",
                    None,
                    format!("no match (best guess {best:?}, p={probability:.2})"),
                );
            }
            Ok(dest) => {
                let body = format!(
                    "Radio call on Ch {ch} ({:.3} MHz), {}:\n\n{server_text}\n\n(Routed from the transcription server's version. On-device transcript: {})",
                    call.mhz,
                    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
                    call.text
                );
                match self.send_reported(call, &server_text, "server", &dest, &body) {
                    Ok(sent) => info!(
                        "RELAY | Ch {ch} | server transcript {server_text:?} -> {} | {}",
                        sent.to, sent.state
                    ),
                    Err(e) => warn!("RELAY | Ch {ch} | delivery failed: {e:#}"),
                }
            }
            Err(e) => {
                warn!("RELAY | Ch {ch} | routing server transcript failed: {e:#}");
                self.report(
                    call,
                    &server_text,
                    "server",
                    None,
                    format!("routing failed: {e:#}"),
                );
            }
        }
    }

    /// The transcription service's version of this call, or None when it is
    /// disabled, down, or fails.
    fn server_transcript(&self, call: &Call) -> Option<String> {
        let ch = &call.channel;
        let (Some(service), Some(wav)) = (&self.verify, &call.recording) else {
            return None;
        };
        if !service.is_up() {
            debug!("RELAY | Ch {ch} | transcription service down");
            return None;
        }
        match service.transcribe(Path::new(wav)) {
            Ok(t) => {
                self.publish_server_transcript(call, &t);
                Some(t)
            }
            Err(e) => {
                warn!("RELAY | Ch {ch} | server transcription failed: {e:#}");
                None
            }
        }
    }

    /// Put the server's version on the bus when its words differ, so Lume and the
    /// TUI keep both. The relay itself only routes on-device transcripts.
    fn publish_server_transcript(&self, call: &Call, server_text: &str) {
        if server_text.trim().is_empty() || words(server_text) == words(&call.text) {
            return;
        }
        self.bus.publish_event(Event::ServerTranscription {
            dongle_id: call.dongle_id.clone(),
            channel: (call.channel != "?").then(|| call.channel.clone()),
            freq_hz: call.freq_hz,
            text: server_text.to_string(),
            on_device: call.text.clone(),
            recording: call.recording.clone(),
            model: self.verify_model.clone(),
        });
    }

    /// Second opinion after the send: re-transcribe on the service and follow up
    /// if it heard something the on-device transcript missed.
    fn double_check(&self, call: &Call, pane_id: &str, pane_name: &str) {
        let ch = &call.channel;
        let Some(server_text) = self.server_transcript(call) else {
            return;
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

/// hertz's own panes: the one hertzd runs in, and any running hertzd or the hertz
/// console. A call never goes to them; they can't act on it (a caller saying
/// "Fond Egret" about the hertzd pane was routed back to it).
fn is_hertz_pane(p: &Pane, own_pane: Option<&str>) -> bool {
    own_pane == Some(p.pane_id.as_str())
        || matches!(
            p.process
                .trim_end_matches(".exe")
                .rsplit(['\\', '/'])
                .next(),
            Some("hertzd" | "hertz")
        )
}

/// Lowercase words without punctuation, to compare two transcripts.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
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

/// The pane a delivery landed on (`operation.target` in Hyperia's reply), so a
/// follow-up can reach it after its call sign has rotated.
fn reply_target(reply: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(reply).ok()?;
    v.pointer("/operation/target")
        .or_else(|| v.get("target"))
        .and_then(|t| t.as_str())
        .map(str::to_string)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: &str, process: &str) -> Pane {
        Pane {
            name: "Fond Egret 🦞".into(),
            pane_id: id.into(),
            active: false,
            focused: false,
            kind: "terminal".into(),
            process: process.into(),
        }
    }

    #[test]
    fn hertz_never_routes_to_its_own_panes() {
        assert!(
            is_hertz_pane(&pane("4dab", "pwsh"), Some("4dab")),
            "the pane hertzd runs in"
        );
        assert!(is_hertz_pane(&pane("x", "hertzd"), None));
        assert!(is_hertz_pane(
            &pane("x", r"C:\hertz\target\release\hertzd.exe"),
            None
        ));
        assert!(is_hertz_pane(&pane("x", "hertz"), None), "the console");
        assert!(!is_hertz_pane(&pane("x", "claude"), Some("4dab")));
        assert!(!is_hertz_pane(&pane("x", "hertz-panes"), None));
    }
}
