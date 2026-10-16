//! Radio traffic → Lume. Finds a running Lume (`lume serve --otlp`) and sends it
//! transcripts, squelch events and saved recordings as OTLP/JSON log records, plus
//! per-channel signal metrics every 10 s, so radio traffic is searchable next to
//! the boat's telemetry. See plan/lume-integration.md.
//!
//! Discovery, in order: `[lume] url`; Lume's discovery file (`~/.lume/serve.json`);
//! a probe of `http://127.0.0.1:5863/health`. While there's no Lume it checks again
//! every minute and drops what it would have sent. Nothing here can block the DSP
//! or relay paths: events reach the sender through a bounded queue, and records
//! that don't fit are dropped.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, RecvTimeoutError, TrySendError};
use hertz_types::{Event, LumeConfig};
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::bus::EventBus;

/// Lume's default `serve` address.
pub const DEFAULT_URL: &str = "http://127.0.0.1:5863";
/// Lume's TI bucket; metrics are sent this often.
const METRICS_EVERY: Duration = Duration::from_secs(10);
/// Logs wait at most this long, or until a batch is full.
const LOG_FLUSH: Duration = Duration::from_secs(1);
const LOG_BATCH: usize = 100;
/// How often to look for Lume while there is none.
const REDISCOVER: Duration = Duration::from_secs(60);
const QUEUE: usize = 2048;

/// Start the Lume feed. With no `[lume]` section it looks for Lume on its own; in
/// `--mock` mode only an explicit `[lume] url` is used, so smoke runs never write
/// fake traffic into a real Lume.
pub fn spawn(bus: &EventBus, cfg: Option<LumeConfig>, mock: bool) {
    let cfg = cfg.unwrap_or_default();
    if !cfg.enabled || (mock && cfg.url.is_none()) {
        debug!("lume: feed off");
        return;
    }
    let (tx, rx) = crossbeam_channel::bounded::<Event>(QUEUE);
    let mut events = bus.subscribe_events();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(
                    ev @ (Event::Transcription { .. }
                    | Event::ServerTranscription { .. }
                    | Event::Relayed { .. }
                    | Event::Agent { .. }
                    | Event::AgentCallsign { .. }
                    | Event::Weather { .. }
                    | Event::SquelchEvent { .. }
                    | Event::SignalLevel { .. }
                    | Event::RecordingSaved { .. }),
                ) => match tx.try_send(ev) {
                    Ok(()) | Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => break,
                },
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
    });
    std::thread::Builder::new()
        .name("lume".into())
        .spawn(move || run(cfg, rx))
        .expect("spawn lume feed");
}

/// Where Lume is and how to authenticate to it.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub url: String,
    pub token: Option<String>,
    /// How it was found, for the log line.
    pub via: &'static str,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
}

/// Find Lume: the configured URL, the discovery file, then the default port.
pub fn discover(cfg: &LumeConfig, agent: &ureq::Agent) -> Option<Target> {
    let token_from = |path: Option<&str>| path.and_then(read_token);
    if let Some(url) = &cfg.url {
        let url = url.trim_end_matches('/').to_string();
        return is_lume(agent, &url).then(|| Target {
            url,
            token: token_from(cfg.token_file.as_deref()),
            via: "config",
        });
    }
    if let Some((url, token_file)) = discovery_file(cfg.discovery_file.as_deref()) {
        if is_lume(agent, &url) {
            let token =
                token_from(cfg.token_file.as_deref()).or_else(|| token_from(token_file.as_deref()));
            return Some(Target {
                url,
                token,
                via: "discovery file",
            });
        }
    }
    is_lume(agent, DEFAULT_URL).then(|| Target {
        url: DEFAULT_URL.to_string(),
        token: token_from(cfg.token_file.as_deref()),
        via: "default port",
    })
}

/// `GET /health` answers 200. Newer Lume returns JSON naming itself; then it must
/// say `"service":"lume"` and not `"otlp":false`. Older Lume returns an empty body.
/// A 401 still means something is serving there; the OTLP post reports it.
fn is_lume(agent: &ureq::Agent, url: &str) -> bool {
    let resp = match agent
        .get(&format!("{url}/health"))
        .set("Accept", "application/json")
        .call()
    {
        Ok(r) => r,
        Err(ureq::Error::Status(401 | 403, _)) => return true,
        Err(_) => return false,
    };
    let body = resp.into_string().unwrap_or_default();
    match serde_json::from_str::<Value>(&body) {
        Ok(v) => v["service"] == "lume" && v["otlp"] != false,
        Err(_) => body.trim().is_empty(),
    }
}

/// Lume's discovery file: `{"url": ..., "token_file": ...}` (Lume writes it while
/// `serve` runs; not in every version yet).
fn discovery_file(path: Option<&str>) -> Option<(String, Option<String>)> {
    let path = match path {
        Some(p) => expand(p)?,
        None => home()?.join(".lume").join("serve.json"),
    };
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    if v["otlp"] == false {
        return None;
    }
    let url = v["url"].as_str()?.trim_end_matches('/').to_string();
    Some((url, v["token_file"].as_str().map(str::to_string)))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// `~/` expands to the home directory.
fn expand(path: &str) -> Option<PathBuf> {
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => Some(home()?.join(rest)),
        None => Some(PathBuf::from(path)),
    }
}

fn read_token(path: &str) -> Option<String> {
    let path = expand(path)?;
    match std::fs::read_to_string(&path) {
        Ok(t) if !t.trim().is_empty() => Some(t.trim().to_string()),
        Ok(_) => None,
        Err(e) => {
            warn!("lume: can't read token file {path:?}: {e}");
            None
        }
    }
}

fn host_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|h| h.trim().to_lowercase())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "hertz".to_string())
}

fn run(cfg: LumeConfig, rx: Receiver<Event>) {
    let agent = agent();
    let instance = cfg.instance.clone().unwrap_or_else(host_name);
    let mut announced_missing = false;
    // One feed for the whole run, so calls heard while Lume is down are kept (up to
    // BACKLOG) and sent when it's found.
    let mut feed = Feed::new(None, instance, agent.clone());
    loop {
        let Some(target) = discover(&cfg, &agent) else {
            if !announced_missing {
                info!("lume: no Lume found (set [lume] url, or run `lume serve --otlp`); checking every minute, keeping calls until then");
                announced_missing = true;
            }
            let until = Instant::now() + REDISCOVER;
            while let Some(left) = until.checked_duration_since(Instant::now()) {
                match rx.recv_timeout(left) {
                    Ok(ev) => feed.take(ev),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            continue;
        };
        announced_missing = false;
        let queued = feed.logs.len();
        info!(
            "lume: sending radio traffic to {} (found via {}){}",
            target.url,
            target.via,
            if queued > 0 {
                format!("; {queued} record(s) kept while it was away")
            } else {
                String::new()
            }
        );
        feed.target = Some(target);
        // Signal sums from the gap would land in one window; start a fresh one.
        feed.reset_metrics();
        match feed.run(&rx) {
            Ended::Disconnected => return,
            Ended::LostLume(why) => {
                let url = feed.target.take().map(|t| t.url).unwrap_or_default();
                warn!("lume: stopped sending to {url}: {why}");
            }
        }
    }
}

/// Log records kept while Lume is unreachable; the oldest go first past this.
const BACKLOG: usize = 2000;

enum Ended {
    Disconnected,
    LostLume(String),
}

/// Per-channel signal over the current metrics window.
#[derive(Default)]
struct ChannelStats {
    signal_db: Option<f32>,
    noise_floor_db: Option<f32>,
    open_since: Option<Instant>,
    open_s: f64,
    transmissions: u64,
    /// Strongest signal while the current call's squelch is open.
    call_peak_db: Option<f32>,
    /// The last finished call: (duration s, peak signal dB, noise floor dB). Its
    /// transcript arrives after the squelch closes and carries these.
    last_call: Option<(f64, f32, Option<f32>)>,
}

/// A finished voice call, for the per-call metrics.
struct CallPoint {
    channel: String,
    end: String,
    duration_s: f64,
    signal_db: f32,
    noise_floor_db: Option<f32>,
}

pub struct Feed {
    /// None while no Lume is found; records accumulate meanwhile.
    target: Option<Target>,
    instance: String,
    agent: ureq::Agent,
    logs: Vec<Value>,
    channels: HashMap<String, ChannelStats>,
    /// Voice calls finished since the last metrics post, sent as per-call gauges.
    calls: Vec<CallPoint>,
    window_start: SystemTime,
}

impl Feed {
    pub fn new(target: Option<Target>, instance: String, agent: ureq::Agent) -> Self {
        Self {
            target,
            instance,
            agent,
            logs: Vec::new(),
            channels: HashMap::new(),
            calls: Vec::new(),
            window_start: SystemTime::now(),
        }
    }

    fn reset_metrics(&mut self) {
        for s in self.channels.values_mut() {
            s.open_s = 0.0;
            s.transmissions = 0;
            if s.open_since.is_some() {
                s.open_since = Some(Instant::now());
            }
        }
        self.window_start = SystemTime::now();
    }

    fn run(&mut self, rx: &Receiver<Event>) -> Ended {
        let mut backoff = Duration::from_secs(1);
        let mut next_metrics = Instant::now() + METRICS_EVERY;
        let mut next_flush = Instant::now() + LOG_FLUSH;
        loop {
            let wake = next_metrics.min(next_flush);
            match rx.recv_timeout(wake.saturating_duration_since(Instant::now())) {
                Ok(ev) => self.take(ev),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = self.flush_logs();
                    return Ended::Disconnected;
                }
            }
            let now = Instant::now();
            let mut result = Ok(());
            if self.logs.len() >= LOG_BATCH || (now >= next_flush && !self.logs.is_empty()) {
                result = self.flush_logs();
                next_flush = now + LOG_FLUSH;
            } else if now >= next_flush {
                next_flush = now + LOG_FLUSH;
            }
            if result.is_ok() && now >= next_metrics {
                result = self.flush_metrics();
                next_metrics = now + METRICS_EVERY;
            }
            match result {
                Ok(()) => backoff = Duration::from_secs(1),
                Err(Failure::Refused(why)) => return Ended::LostLume(why),
                Err(Failure::Down(why)) => {
                    if backoff >= Duration::from_secs(60) {
                        return Ended::LostLume(why);
                    }
                    debug!("lume: send failed ({why}); retrying in {backoff:?}");
                    std::thread::sleep(backoff);
                    backoff *= 2;
                }
            }
        }
    }

    /// Turn a bus event into a log record and/or metric update.
    pub fn take(&mut self, ev: Event) {
        self.record(ev);
        if self.logs.len() > BACKLOG {
            let extra = self.logs.len() - BACKLOG;
            self.logs.drain(..extra);
        }
    }

    fn record(&mut self, ev: Event) {
        let now = Instant::now();
        match ev {
            Event::Transcription {
                dongle_id,
                channel,
                freq_hz,
                text,
                recording,
            } => {
                if text.trim().is_empty() {
                    return;
                }
                let mut attrs = vec![
                    ("dongle", dongle_id),
                    ("freq_mhz", mhz(freq_hz)),
                    ("engine", "whistle".to_string()),
                ];
                self.push_call_stats(&mut attrs, channel.as_deref(), freq_hz);
                if let Some(c) = &channel {
                    attrs.push(("channel", c.clone()));
                }
                if let Some(r) = recording {
                    attrs.push(("recording", file_name(&r)));
                }
                self.logs.push(log_record("radio.transcript", &text, attrs));
            }
            Event::Relayed {
                dongle_id,
                channel,
                freq_hz,
                text,
                source,
                to,
                outcome,
            } => {
                let body = match &to {
                    Some(to) => format!("-> {to} | {outcome} | {text}"),
                    None => format!("not delivered: {outcome} | {text}"),
                };
                let mut attrs = vec![
                    ("dongle", dongle_id),
                    ("freq_mhz", mhz(freq_hz)),
                    ("source", source),
                    ("outcome", outcome),
                    ("transcript", text),
                ];
                if let Some(c) = channel {
                    attrs.push(("channel", c));
                }
                if let Some(to) = to {
                    attrs.push(("to", to));
                }
                self.logs.push(log_record("radio.relay", &body, attrs));
            }
            Event::Agent {
                dongle_id,
                channel,
                freq_hz,
                text,
                callsign,
                tool,
                args,
                decisions,
                outcome,
                reply,
            } => {
                let body = format!(
                    "{callsign}: {tool} {args} | {outcome} | said: {reply} | heard: {text}"
                );
                let mut attrs = vec![
                    ("dongle", dongle_id),
                    ("freq_mhz", mhz(freq_hz)),
                    ("callsign", callsign),
                    ("tool", tool),
                    ("args", args),
                    ("decisions", decisions),
                    ("outcome", outcome),
                    ("reply", reply),
                    ("transcript", text),
                ];
                if let Some(c) = channel {
                    attrs.push(("channel", c));
                }
                self.logs.push(log_record("radio.agent", &body, attrs));
            }
            Event::AgentCallsign { callsign, previous } => {
                let mut attrs = vec![("callsign", callsign.clone())];
                if let Some(p) = previous {
                    attrs.push(("previous", p));
                }
                self.logs.push(log_record(
                    "radio.agent.callsign",
                    &format!("agent call sign now {callsign}"),
                    attrs,
                ));
            }
            Event::Weather {
                dongle_id,
                channel,
                freq_hz,
                text,
                seconds,
                signal_db,
                ..
            } => {
                let mut attrs = vec![
                    ("dongle", dongle_id),
                    ("channel", channel),
                    ("freq_mhz", mhz(freq_hz)),
                    ("sample_s", format!("{seconds:.0}")),
                    ("engine", "whistle".to_string()),
                ];
                if let Some(db) = signal_db {
                    attrs.push(("signal_db", format!("{db:.1}")));
                }
                self.logs.push(log_record("radio.weather", &text, attrs));
            }
            Event::ServerTranscription {
                dongle_id,
                channel,
                freq_hz,
                text,
                on_device,
                recording,
                model,
            } => {
                let mut attrs = vec![
                    ("dongle", dongle_id),
                    ("freq_mhz", mhz(freq_hz)),
                    ("engine", "server".to_string()),
                    ("model", model),
                    ("on_device", on_device),
                ];
                self.push_call_stats(&mut attrs, channel.as_deref(), freq_hz);
                if let Some(c) = channel {
                    attrs.push(("channel", c));
                }
                if let Some(r) = recording {
                    attrs.push(("recording", file_name(&r)));
                }
                self.logs.push(log_record("radio.transcript", &text, attrs));
            }
            Event::SquelchEvent {
                dongle_id,
                channel,
                freq_hz,
                open,
                signal_db,
                classification,
            } => {
                let label = channel.clone().unwrap_or_else(|| mhz(freq_hz));
                let stats = self.channels.entry(label.clone()).or_default();
                let mut duration = None;
                if open {
                    stats.open_since.get_or_insert(now);
                    stats.transmissions += 1;
                    stats.call_peak_db = Some(signal_db);
                } else if let Some(since) = stats.open_since.take() {
                    let secs = now.duration_since(since).as_secs_f64();
                    stats.open_s += secs;
                    let peak = stats.call_peak_db.take().unwrap_or(signal_db);
                    stats.last_call = Some((secs, peak, stats.noise_floor_db));
                    duration = Some(secs);
                    if classification == "VOICE" {
                        self.calls.push(CallPoint {
                            channel: label.clone(),
                            end: nanos(SystemTime::now()),
                            duration_s: secs,
                            signal_db: peak,
                            noise_floor_db: stats.noise_floor_db,
                        });
                    }
                }
                let floor = stats.noise_floor_db;
                let body = format!(
                    "squelch {} Ch {label}",
                    if open { "open" } else { "closed" }
                );
                let mut attrs = vec![
                    ("dongle", dongle_id),
                    ("freq_mhz", mhz(freq_hz)),
                    ("signal_db", format!("{signal_db:.1}")),
                    ("classification", classification),
                    ("open", open.to_string()),
                ];
                if let Some(secs) = duration {
                    attrs.push(("duration_s", format!("{secs:.1}")));
                }
                if let Some(db) = floor {
                    attrs.push(("noise_floor_db", format!("{db:.1}")));
                }
                if let Some(c) = channel {
                    attrs.push(("channel", c));
                }
                self.logs.push(log_record("radio.squelch", &body, attrs));
            }
            Event::SignalLevel {
                channel,
                freq_hz,
                signal_db,
                noise_floor,
                ..
            } => {
                let stats = self
                    .channels
                    .entry(channel.unwrap_or_else(|| mhz(freq_hz)))
                    .or_default();
                stats.signal_db = Some(signal_db);
                stats.noise_floor_db = Some(noise_floor);
                if stats.open_since.is_some() {
                    stats.call_peak_db =
                        Some(stats.call_peak_db.map_or(signal_db, |p| p.max(signal_db)));
                }
            }
            Event::RecordingSaved {
                dongle_id,
                channel,
                filename,
                duration_sec,
                ..
            } => {
                let mut attrs = vec![
                    ("dongle", dongle_id),
                    ("duration_s", format!("{duration_sec:.1}")),
                ];
                if let Some(c) = channel {
                    attrs.push(("channel", c));
                }
                self.logs
                    .push(log_record("radio.recording", &file_name(&filename), attrs));
            }
            _ => {}
        }
    }

    /// The finished call's duration, peak signal and noise floor on this channel.
    fn push_call_stats(
        &self,
        attrs: &mut Vec<(&str, String)>,
        channel: Option<&str>,
        freq_hz: u64,
    ) {
        let label = channel.map(str::to_string).unwrap_or_else(|| mhz(freq_hz));
        let Some((secs, peak, floor)) = self.channels.get(&label).and_then(|s| s.last_call) else {
            return;
        };
        attrs.push(("duration_s", format!("{secs:.1}")));
        attrs.push(("signal_db", format!("{peak:.1}")));
        if let Some(db) = floor {
            attrs.push(("noise_floor_db", format!("{db:.1}")));
        }
    }

    fn resource(&self) -> Value {
        json!({ "attributes": [
            kv("service.name", "hertz"),
            kv("service.instance.id", &self.instance),
            kv("service.version", env!("CARGO_PKG_VERSION")),
        ]})
    }

    fn flush_logs(&mut self) -> Result<(), Failure> {
        if self.logs.is_empty() {
            return Ok(());
        }
        let body = json!({ "resourceLogs": [{
            "resource": self.resource(),
            "scopeLogs": [{ "scope": { "name": "hertz" }, "logRecords": self.logs }],
        }]});
        let result = self.post("/v1/logs", &body);
        // Sent, or refused for good: either way these records are done.
        if !matches!(result, Err(Failure::Down(_))) {
            self.logs.clear();
        }
        result
    }

    /// Per-channel gauges (signal, noise floor) and window sums (seconds open,
    /// transmissions), then start a new window.
    fn flush_metrics(&mut self) -> Result<(), Failure> {
        let now = SystemTime::now();
        let (start, end) = (nanos(self.window_start), nanos(now));
        let instant_now = Instant::now();
        let mut signal = Vec::new();
        let mut noise = Vec::new();
        let mut open_s = Vec::new();
        let mut tx = Vec::new();
        for (channel, s) in &mut self.channels {
            let attrs = json!([kv("channel", channel)]);
            if let Some(db) = s.signal_db {
                signal.push(json!({ "timeUnixNano": end, "asDouble": db, "attributes": attrs }));
            }
            if let Some(db) = s.noise_floor_db {
                noise.push(json!({ "timeUnixNano": end, "asDouble": db, "attributes": attrs }));
            }
            // Count a squelch still open up to now, and carry it into the next window.
            let mut secs = s.open_s;
            if let Some(since) = s.open_since.as_mut() {
                secs += instant_now.duration_since(*since).as_secs_f64();
                *since = instant_now;
            }
            open_s.push(json!({ "startTimeUnixNano": start, "timeUnixNano": end, "asDouble": secs, "attributes": attrs }));
            tx.push(json!({ "startTimeUnixNano": start, "timeUnixNano": end, "asInt": s.transmissions.to_string(), "attributes": attrs }));
        }
        // Per-call gauges at each call's end, and a per-channel count of calls:
        // Lume makes these columns (radio.call.duration_s.channel.72, ...).
        let (mut call_dur, mut call_sig, mut call_floor) = (Vec::new(), Vec::new(), Vec::new());
        let mut calls_by_channel: HashMap<&str, u64> = HashMap::new();
        for call in &self.calls {
            let attrs = json!([kv("channel", &call.channel)]);
            let point =
                |v: f64| json!({ "timeUnixNano": call.end, "asDouble": v, "attributes": attrs });
            call_dur.push(point(call.duration_s));
            call_sig.push(point(call.signal_db as f64));
            if let Some(db) = call.noise_floor_db {
                call_floor.push(point(db as f64));
            }
            *calls_by_channel.entry(&call.channel).or_default() += 1;
        }
        let calls: Vec<Value> = calls_by_channel
            .iter()
            .map(|(ch, n)| json!({ "startTimeUnixNano": start, "timeUnixNano": end, "asInt": n.to_string(), "attributes": [kv("channel", ch)] }))
            .collect();
        if signal.is_empty() && open_s.is_empty() && calls.is_empty() {
            self.window_start = now;
            return Ok(());
        }
        // Delta sums (aggregationTemporality 1): each window counts only itself.
        let sum = |points: Vec<Value>| json!({ "aggregationTemporality": 1, "isMonotonic": false, "dataPoints": points });
        let body = json!({ "resourceMetrics": [{
            "resource": self.resource(),
            "scopeMetrics": [{ "scope": { "name": "hertz" }, "metrics": [
                { "name": "radio.signal_db", "unit": "dB", "gauge": { "dataPoints": signal } },
                { "name": "radio.noise_floor_db", "unit": "dB", "gauge": { "dataPoints": noise } },
                { "name": "radio.squelch_open_s", "unit": "s", "sum": sum(open_s) },
                { "name": "radio.transmissions", "sum": sum(tx) },
                { "name": "radio.calls", "sum": sum(calls) },
                { "name": "radio.call.duration_s", "unit": "s", "gauge": { "dataPoints": call_dur } },
                { "name": "radio.call.signal_db", "unit": "dB", "gauge": { "dataPoints": call_sig } },
                { "name": "radio.call.noise_floor_db", "unit": "dB", "gauge": { "dataPoints": call_floor } },
            ]}],
        }]});
        let result = self.post("/v1/metrics", &body);
        if result.is_ok() {
            self.calls.clear();
            for s in self.channels.values_mut() {
                s.open_s = 0.0;
                s.transmissions = 0;
            }
            self.window_start = now;
        }
        result
    }

    fn post(&self, path: &str, body: &Value) -> Result<(), Failure> {
        let Some(target) = &self.target else {
            return Err(Failure::Down("no Lume found yet".into()));
        };
        let mut req = self.agent.post(&format!("{}{path}", target.url));
        if let Some(t) = &target.token {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        match req.send_json(body) {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(code @ (401 | 403), _)) => Err(Failure::Refused(format!(
                "Lume refused hertz ({code}); set [lume] token_file to a token with write access"
            ))),
            Err(ureq::Error::Status(404, _)) => Err(Failure::Refused(
                "no OTLP endpoint (start Lume with `lume serve --otlp`)".to_string(),
            )),
            Err(ureq::Error::Status(code, _)) => Err(Failure::Down(format!("{path}: HTTP {code}"))),
            Err(e) => Err(Failure::Down(e.to_string())),
        }
    }
}

enum Failure {
    /// Lume answered but won't take records from hertz (auth, no OTLP).
    Refused(String),
    /// Lume didn't answer; retry with backoff.
    Down(String),
}

fn log_record(event_name: &str, body: &str, attrs: Vec<(&str, String)>) -> Value {
    let mut attributes = vec![kv("event.name", event_name)];
    attributes.extend(attrs.iter().map(|(k, v)| typed_kv(k, v)));
    json!({
        "timeUnixNano": nanos(SystemTime::now()),
        "eventName": event_name,
        "severityNumber": 9,
        "severityText": "INFO",
        "body": { "stringValue": body },
        "attributes": attributes,
    })
}

/// Attributes that are numbers go as OTLP doubles, so Lume can compare and sort
/// them; everything else is a string.
const NUMERIC: &[&str] = &[
    "freq_mhz",
    "signal_db",
    "noise_floor_db",
    "duration_s",
    "sample_s",
];

fn typed_kv(key: &str, value: &str) -> Value {
    match value.parse::<f64>() {
        Ok(n) if NUMERIC.contains(&key) => json!({ "key": key, "value": { "doubleValue": n } }),
        _ => kv(key, value),
    }
}

fn kv(key: &str, value: &str) -> Value {
    json!({ "key": key, "value": { "stringValue": value } })
}

fn nanos(t: SystemTime) -> String {
    t.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}

fn mhz(freq_hz: u64) -> String {
    // Four places: FRS/GMRS channels sit on 12.5 kHz steps (462.5625 MHz).
    format!("{:.4}", freq_hz as f64 / 1e6)
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// Requests the fake Lume received: (path, body).
    type Seen = Arc<Mutex<Vec<(String, String)>>>;

    /// A stand-in Lume on 127.0.0.1 (port 0): `/health` and `/v1/*` answer 200,
    /// and request paths and bodies are recorded.
    fn fake_lume() -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut len = 0;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                log.lock()
                    .unwrap()
                    .push((path, String::from_utf8(body).unwrap()));
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .unwrap();
            }
        });
        (url, seen)
    }

    fn transcript(text: &str) -> Event {
        Event::Transcription {
            dongle_id: "00000001-0".into(),
            channel: Some("72".into()),
            freq_hz: 156_625_000,
            text: text.into(),
            recording: Some("data/recordings/transmission_1.wav".into()),
        }
    }

    #[test]
    fn configured_url_is_used_when_it_answers() {
        let (url, _) = fake_lume();
        let cfg = LumeConfig {
            url: Some(format!("{url}/")),
            ..Default::default()
        };
        let found = discover(&cfg, &agent()).expect("found");
        assert_eq!(found.url, url);
        assert_eq!(found.via, "config");
    }

    #[test]
    fn discovery_file_gives_url_and_token() {
        let (url, _) = fake_lume();
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("otlp-token");
        std::fs::write(&token, "ahp_test\n").unwrap();
        let file = dir.path().join("serve.json");
        let serve = json!({ "url": url, "otlp": true, "token_file": token.to_str().unwrap() });
        std::fs::write(&file, serve.to_string()).unwrap();
        let cfg = LumeConfig {
            discovery_file: Some(file.to_str().unwrap().into()),
            ..Default::default()
        };
        let found = discover(&cfg, &agent()).expect("found");
        assert_eq!(
            (found.url.as_str(), found.via),
            (url.as_str(), "discovery file")
        );
        assert_eq!(found.token.as_deref(), Some("ahp_test"));

        // otlp off in the file: not a target.
        std::fs::write(&file, json!({ "url": url, "otlp": false }).to_string()).unwrap();
        assert_eq!(discovery_file(cfg.discovery_file.as_deref()), None);
    }

    #[test]
    fn configured_url_that_does_not_answer_is_not_lume() {
        // Bind and drop to get a port nothing listens on.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let cfg = LumeConfig {
            url: Some(format!("http://127.0.0.1:{port}")),
            ..Default::default()
        };
        assert_eq!(discover(&cfg, &agent()), None);
    }

    #[test]
    fn transcripts_and_signal_reach_lume_as_otlp() {
        let (url, seen) = fake_lume();
        let target = Target {
            url,
            token: None,
            via: "test",
        };
        let mut feed = Feed::new(Some(target), "viridis".into(), agent());
        feed.take(transcript("Alpha India, come in, over."));
        feed.take(transcript("   "));
        feed.take(Event::SignalLevel {
            dongle_id: "00000001-0".into(),
            channel: Some("72".into()),
            freq_hz: 156_625_000,
            signal_db: -1.5,
            noise_floor: -20.0,
            squelch_open: true,
            audio_flatness: 0.2,
        });
        assert!(feed.flush_logs().is_ok());
        assert!(feed.flush_metrics().is_ok());

        let seen = seen.lock().unwrap();
        let (path, logs) = &seen[0];
        assert_eq!(path, "/v1/logs");
        let logs: Value = serde_json::from_str(logs).unwrap();
        let records = &logs["resourceLogs"][0]["scopeLogs"][0]["logRecords"];
        assert_eq!(
            records.as_array().unwrap().len(),
            1,
            "empty transcript skipped"
        );
        assert_eq!(records[0]["eventName"], "radio.transcript");
        assert_eq!(
            records[0]["body"]["stringValue"],
            "Alpha India, come in, over."
        );
        let attrs = records[0]["attributes"].to_string();
        assert!(
            attrs.contains("\"72\"")
                && attrs.contains("156.625")
                && attrs.contains("transmission_1.wav")
        );
        let resource = logs["resourceLogs"][0]["resource"].to_string();
        assert!(resource.contains("\"hertz\"") && resource.contains("viridis"));

        let (path, metrics) = &seen[1];
        assert_eq!(path, "/v1/metrics");
        assert!(metrics.contains("radio.signal_db") && metrics.contains("-1.5"));
    }

    #[test]
    fn server_transcript_is_a_second_transcript_record() {
        let mut feed = Feed::new(None, "x".into(), agent());
        feed.take(Event::ServerTranscription {
            dongle_id: "d".into(),
            channel: Some("72".into()),
            freq_hz: 156_625_000,
            text: "Contact the Lume Claude agent.".into(),
            on_device: "Contact the womb, clawed agent.".into(),
            recording: Some("transmission_2.wav".into()),
            model: "medium".into(),
        });
        let record = &feed.logs[0];
        assert_eq!(record["eventName"], "radio.transcript");
        assert_eq!(
            record["body"]["stringValue"],
            "Contact the Lume Claude agent."
        );
        let attrs = record["attributes"].to_string();
        assert!(
            attrs.contains("\"server\"")
                && attrs.contains("womb, clawed")
                && attrs.contains("medium")
        );
    }

    #[test]
    fn calls_are_kept_while_lume_is_away_and_sent_when_found() {
        let (url, seen) = fake_lume();
        let mut feed = Feed::new(None, "x".into(), agent());
        feed.take(transcript("heard while Lume was down"));
        assert!(feed.flush_logs().is_err(), "nowhere to send yet");
        assert_eq!(feed.logs.len(), 1, "kept, not dropped");
        feed.target = Some(Target {
            url,
            token: None,
            via: "test",
        });
        assert!(feed.flush_logs().is_ok());
        assert!(seen.lock().unwrap()[0]
            .1
            .contains("heard while Lume was down"));
        assert!(feed.logs.is_empty());
    }

    #[test]
    fn backlog_keeps_the_newest() {
        let mut feed = Feed::new(None, "x".into(), agent());
        for i in 0..BACKLOG + 5 {
            feed.take(transcript(&format!("call {i}")));
        }
        assert_eq!(feed.logs.len(), BACKLOG);
        assert_eq!(feed.logs[0]["body"]["stringValue"], "call 5");
    }

    #[test]
    fn a_wrong_route_is_on_record() {
        let mut feed = Feed::new(None, "x".into(), agent());
        feed.take(Event::Relayed {
            dongle_id: "d".into(),
            channel: Some("gmrs-15".into()),
            freq_hz: 462_550_000,
            text: "Stable Xerinae, Lume is not running in Yucky Fowl.".into(),
            source: "on-device".into(),
            to: Some("Yucky Fowl (pane name)".into()),
            outcome: "awaiting_approval".into(),
        });
        let r = &feed.logs[0];
        assert_eq!(r["eventName"], "radio.relay");
        let body = r["body"]["stringValue"].as_str().unwrap();
        assert!(body.starts_with("-> Yucky Fowl") && body.contains("Lume is not running"));
    }

    #[test]
    fn transcript_carries_the_calls_numbers_as_numbers() {
        let mut feed = Feed::new(None, "x".into(), agent());
        let squelch = |open, signal_db| Event::SquelchEvent {
            dongle_id: "d".into(),
            channel: Some("gmrs-1".into()),
            freq_hz: 462_562_500,
            open,
            signal_db,
            classification: "VOICE".into(),
        };
        let level = |signal_db| Event::SignalLevel {
            dongle_id: "d".into(),
            channel: Some("gmrs-1".into()),
            freq_hz: 462_562_500,
            signal_db,
            noise_floor: -20.0,
            squelch_open: true,
            audio_flatness: 0.2,
        };
        feed.take(level(-20.0));
        feed.take(squelch(true, -8.0));
        feed.take(level(-3.5));
        std::thread::sleep(Duration::from_millis(30));
        feed.take(squelch(false, -14.0));
        feed.take(Event::Transcription {
            dongle_id: "d".into(),
            channel: Some("gmrs-1".into()),
            freq_hz: 462_562_500,
            text: "Zulu Echo, come in.".into(),
            recording: None,
        });
        let attrs = feed.logs.last().unwrap()["attributes"]
            .as_array()
            .unwrap()
            .clone();
        let get = |k: &str| {
            attrs
                .iter()
                .find(|a| a["key"] == k)
                .map(|a| a["value"].clone())
                .unwrap()
        };
        assert_eq!(get("channel")["stringValue"], "gmrs-1");
        assert_eq!(get("signal_db")["doubleValue"], -3.5, "the call's peak");
        assert_eq!(get("noise_floor_db")["doubleValue"], -20.0);
        assert_eq!(get("freq_mhz")["doubleValue"], 462.5625);
        assert!(get("duration_s")["doubleValue"].as_f64().unwrap() >= 0.0);
    }

    #[test]
    fn voice_calls_become_per_call_metrics() {
        let (url, seen) = fake_lume();
        let mut feed = Feed::new(
            Some(Target {
                url,
                token: None,
                via: "test",
            }),
            "x".into(),
            agent(),
        );
        let squelch = |open, classification: &str| Event::SquelchEvent {
            dongle_id: "d".into(),
            channel: Some("gmrs-1".into()),
            freq_hz: 462_562_500,
            open,
            signal_db: -5.0,
            classification: classification.into(),
        };
        feed.take(squelch(true, "VOICE"));
        feed.take(squelch(false, "VOICE"));
        feed.take(squelch(true, "CARRIER"));
        feed.take(squelch(false, "CARRIER"));
        assert_eq!(feed.calls.len(), 1, "only the voice call counts");
        assert!(feed.flush_metrics().is_ok());
        let metrics = &seen.lock().unwrap()[0].1;
        for name in [
            "radio.calls",
            "radio.call.duration_s",
            "radio.call.signal_db",
        ] {
            assert!(metrics.contains(name), "{name} missing");
        }
        assert!(feed.calls.is_empty());
    }

    #[test]
    fn squelch_time_and_transmissions_count_per_channel() {
        let mut feed = Feed::new(None, "x".into(), agent());
        let squelch = |open| Event::SquelchEvent {
            dongle_id: "d".into(),
            channel: Some("16".into()),
            freq_hz: 156_800_000,
            open,
            signal_db: 3.0,
            classification: "VOICE".into(),
        };
        feed.take(squelch(true));
        std::thread::sleep(Duration::from_millis(30));
        feed.take(squelch(false));
        let s = &feed.channels["16"];
        assert_eq!(s.transmissions, 1);
        assert!(s.open_s >= 0.03 && s.open_since.is_none());
        assert_eq!(feed.logs.len(), 2);
        assert_eq!(feed.logs[0]["eventName"], "radio.squelch");
    }
}
