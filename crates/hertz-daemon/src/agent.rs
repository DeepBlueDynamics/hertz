//! The radio agent: a call to its call sign is carried out with a hertz tool.
//!
//! ollaya only answers multiple-choice questions, so every decision is one:
//! first which tool (each tool's agent spec gives its `when`, plus "nothing"),
//! then each of that tool's arguments, all in one `/api/decide` call. The code
//! builds the arguments from the picks, runs the tool through the registry
//! [`crate::ops`] and speaks the tool's stock reply with Hyperia
//! `hyperia_spoken_summary` as hertz-radio.
//!
//! The call sign is two NATO words. It changes after every call carried out,
//! so a sign heard on the air works once; each new one is logged, mailed to the
//! `agent_chat` pane and sent to Lume. Every call handled becomes one
//! `Event::Agent` with all questions, options and probabilities: the training
//! record for the decision models.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use hertz_relay::hyperia::HyperiaClient;
use hertz_relay::ollaya::{Choice, OllayaClient};
use hertz_types::{Event, RelayConfig};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::ops::{self, AgentSpec};
use crate::runtime::DaemonState;

/// Filled once the daemon state exists; the relay holds it from the start.
pub type AgentSlot = Arc<OnceLock<Arc<Agent>>>;

/// The `tool` option for a call that asks for nothing hertz can do.
pub const NOTHING: &str = "nothing";

/// At most this many calls carried out per window.
const RATE_CALLS: usize = 10;
const RATE_WINDOW: Duration = Duration::from_secs(600);

/// The longest a tool may run before the agent gives up on it.
const TOOL_TIMEOUT: Duration = Duration::from_secs(20);

const NATO: [&str; 26] = [
    "Alfa", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf", "Hotel", "India", "Juliett",
    "Kilo", "Lima", "Mike", "November", "Oscar", "Papa", "Quebec", "Romeo", "Sierra", "Tango",
    "Uniform", "Victor", "Whiskey", "X-ray", "Yankee", "Zulu",
];

/// A transcript addressed to the agent.
pub struct AgentCall {
    pub dongle_id: String,
    pub channel: Option<String>,
    pub freq_hz: u64,
    pub text: String,
    pub recording: Option<String>,
}

pub struct Agent {
    state: DaemonState,
    rt: tokio::runtime::Handle,
    ollaya: OllayaClient,
    hyperia: HyperiaClient,
    min_probability: f64,
    chat: Option<String>,
    callsign: Mutex<String>,
    /// Calls already handled (recording file, or channel and text).
    handled: Mutex<VecDeque<String>>,
    /// When recent calls were carried out, for the rate limit.
    recent: Mutex<VecDeque<Instant>>,
    /// Call signs not to take (Hyperia's, so the two never collide).
    avoid: Box<dyn Fn() -> Vec<String> + Send + Sync>,
}

impl Agent {
    pub fn new(
        state: DaemonState,
        rt: tokio::runtime::Handle,
        cfg: &RelayConfig,
        hyperia: HyperiaClient,
        avoid: Box<dyn Fn() -> Vec<String> + Send + Sync>,
    ) -> Arc<Self> {
        let agent = Arc::new(Self {
            state,
            rt,
            ollaya: OllayaClient::new(cfg.ollaya_url.clone(), cfg.ollaya_model.clone()),
            hyperia,
            min_probability: cfg.min_probability,
            chat: (!cfg.agent_chat.trim().is_empty()).then(|| cfg.agent_chat.trim().to_string()),
            callsign: Mutex::new(String::new()),
            handled: Mutex::new(VecDeque::new()),
            recent: Mutex::new(VecDeque::new()),
            avoid,
        });
        agent.rotate();
        agent
    }

    pub fn callsign(&self) -> String {
        self.callsign
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Take a new call sign and post it.
    fn rotate(&self) {
        let avoid = (self.avoid)();
        let previous = self.callsign();
        let mut seed = seed();
        let sign = loop {
            let s = new_callsign(&mut seed);
            if s != previous && !avoid.iter().any(|a| a.eq_ignore_ascii_case(&s)) {
                break s;
            }
        };
        *self.callsign.lock().unwrap_or_else(|e| e.into_inner()) = sign.clone();
        info!("AGENT | call sign now {sign}");
        self.state.bus.publish_event(Event::AgentCallsign {
            callsign: sign.clone(),
            previous: (!previous.is_empty()).then_some(previous),
        });
        if let Some(chat) = &self.chat {
            let body = format!(
                "The hertz radio agent's call sign is now {sign}. Call it on the radio: \
                 \"{sign}, <what you want>\". It changes after each call it carries out."
            );
            if let Err(e) = self.hyperia.msg_send(chat, "hertz agent call sign", &body) {
                warn!("AGENT | can't post the call sign to {chat}: {e:#}");
            }
        }
    }

    /// Handle one call to the agent (blocking; run it off the async runtime).
    /// Returns the outcome for the relay's log.
    pub fn handle(&self, call: &AgentCall) -> String {
        let key = call
            .recording
            .clone()
            .unwrap_or_else(|| format!("{:?}|{}", call.channel, call.text));
        {
            let mut seen = self.handled.lock().unwrap_or_else(|e| e.into_inner());
            if seen.contains(&key) {
                return "already handled".into();
            }
            seen.push_back(key);
            while seen.len() > 200 {
                seen.pop_front();
            }
        }
        let callsign = self.callsign();
        let mut rec = Record::default();
        let outcome = self.run(call, &callsign, &mut rec);
        let reply = rec.reply.clone();
        if !reply.is_empty() {
            if let Err(e) = self.hyperia.spoken_summary(&reply) {
                warn!("AGENT | can't speak {reply:?}: {e:#}");
            }
        }
        info!(
            "AGENT | {callsign} | {:?} -> {} {} | {outcome} | said {reply:?}",
            call.text, rec.tool, rec.args
        );
        self.state.bus.publish_event(Event::Agent {
            dongle_id: call.dongle_id.clone(),
            channel: call.channel.clone(),
            freq_hz: call.freq_hz,
            text: call.text.clone(),
            callsign,
            tool: rec.tool.clone(),
            args: rec.args.to_string(),
            decisions: Value::Array(rec.decisions).to_string(),
            outcome: outcome.clone(),
            reply,
        });
        if rec.ran {
            self.rotate();
        }
        outcome
    }

    fn run(&self, call: &AgentCall, callsign: &str, rec: &mut Record) -> String {
        rec.args = json!({});
        let tools: Vec<(&'static ops::Tool, AgentSpec)> = ops::registry()
            .filter_map(|t| t.agent.map(|spec| (t, spec(&self.state))))
            .collect();
        let state = json!(format!(
            "Radio call to {callsign} on channel {}: {}",
            call.channel.as_deref().unwrap_or("unknown"),
            call.text
        ));

        let mut criteria: BTreeMap<String, String> = tools
            .iter()
            .map(|(t, spec)| (t.name.to_string(), spec.when.clone()))
            .collect();
        criteria.insert(
            NOTHING.into(),
            "The call asks for nothing the radio can do, or it is only a greeting or a test".into(),
        );
        let tool_q = BTreeMap::from([("tool".to_string(), criteria)]);
        let picked = match self.ollaya.choose(&state, &tool_q) {
            Ok(mut a) => a.remove("tool"),
            Err(e) => {
                rec.tool = "error".into();
                rec.reply = "Unable, the decision service is down.".into();
                return format!("ollaya failed: {e:#}");
            }
        };
        let Some(pick) = picked else {
            rec.tool = "error".into();
            return "ollaya gave no tool".into();
        };
        rec.decisions.push(decision("tool", &tool_q["tool"], &pick));
        if pick.confidence < self.min_probability {
            rec.tool = "say_again".into();
            rec.reply = "Say again.".into();
            return format!("tool unsure ({} p={:.2})", pick.choice, pick.confidence);
        }
        rec.tool = pick.choice.clone();
        if pick.choice == NOTHING {
            rec.reply = "Roger.".into();
            return "nothing to do".into();
        }
        let Some((tool, spec)) = tools.iter().find(|(t, _)| t.name == pick.choice) else {
            return format!("ollaya picked {:?}, not a tool", pick.choice);
        };

        if !self.allow() {
            rec.reply = "Unable, too many commands. Stand by.".into();
            return "rate limited".into();
        }

        // All argument questions in one decision.
        let mut picks = BTreeMap::new();
        if !spec.questions.is_empty() {
            let qs: BTreeMap<String, BTreeMap<String, String>> = spec
                .questions
                .iter()
                .map(|q| (q.arg.clone(), q.criteria()))
                .collect();
            let answers = match self.ollaya.choose(&state, &qs) {
                Ok(a) => a,
                Err(e) => {
                    rec.reply = "Unable, the decision service is down.".into();
                    return format!("ollaya failed on arguments: {e:#}");
                }
            };
            for q in &spec.questions {
                let Some(a) = answers.get(&q.arg) else {
                    rec.reply = "Say again.".into();
                    return format!("no answer for {}", q.arg);
                };
                rec.decisions.push(decision(&q.arg, &qs[&q.arg], a));
                if a.confidence < self.min_probability {
                    rec.reply = format!("Say again {}.", q.arg.replace('_', " "));
                    return format!("{} unsure ({} p={:.2})", q.arg, a.choice, a.confidence);
                }
                picks.insert(q.arg.clone(), a.choice.clone());
            }
        }
        let args = match spec.build_args(&picks) {
            Ok(a) => a,
            Err(e) => {
                rec.reply = "Say again.".into();
                return e;
            }
        };
        rec.args = args.clone();
        let st = self.state.clone();
        let name = tool.name;
        let result = self.rt.block_on(async move {
            tokio::time::timeout(TOOL_TIMEOUT, ops::call(&st, name, args)).await
        });
        rec.ran = true;
        match result {
            Ok(Ok(v)) => {
                rec.reply = spec.fill_reply(&picks, &v);
                "ok".into()
            }
            Ok(Err(e)) => {
                rec.reply = format!("Unable, {}.", short(e.message()));
                e.to_string()
            }
            Err(_) => {
                rec.reply = "Unable, it took too long.".into();
                "timed out".into()
            }
        }
    }

    /// The rate limit: true when another call may be carried out now.
    fn allow(&self) -> bool {
        let mut recent = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        while recent
            .front()
            .is_some_and(|t| now.duration_since(*t) > RATE_WINDOW)
        {
            recent.pop_front();
        }
        if recent.len() >= RATE_CALLS {
            return false;
        }
        recent.push_back(now);
        true
    }
}

/// Start the agent into `slot` with the relay's Hyperia identity (hertz-radio).
/// Must be called inside the tokio runtime. A missing token leaves it off.
pub fn start(slot: &AgentSlot, state: DaemonState, cfg: &RelayConfig) {
    let path = crate::relay::expand_home(&cfg.token_file);
    let token = match std::fs::read_to_string(&path) {
        Ok(t) => t.trim().to_string(),
        Err(e) => {
            warn!("agent disabled: can't read token {path:?}: {e}");
            return;
        }
    };
    let hyperia = HyperiaClient::new(cfg.hyperia_url.clone(), Some(token.clone()));
    let signs = HyperiaClient::new(cfg.hyperia_url.clone(), Some(token));
    let avoid = Box::new(move || signs.radio_callsigns().unwrap_or_default());
    let rt = tokio::runtime::Handle::current();
    let cfg = cfg.clone();
    let slot = Arc::clone(slot);
    // Rotating posts to Hyperia (blocking HTTP), so not on the runtime.
    std::thread::Builder::new()
        .name("agent-start".into())
        .spawn(move || {
            let agent = Agent::new(state, rt, &cfg, hyperia, avoid);
            let sign = agent.callsign();
            if let Err(e) = agent
                .hyperia
                .spoken_summary(&format!("Radio agent on. Call sign {sign}."))
            {
                warn!("AGENT | can't speak the call sign: {e:#}");
            }
            if slot.set(agent).is_ok() {
                info!("AGENT | listening for {sign}");
            }
        })
        .expect("spawn agent start");
}

#[derive(Default)]
struct Record {
    tool: String,
    args: Value,
    decisions: Vec<Value>,
    reply: String,
    /// A tool was called (the call sign rotates).
    ran: bool,
}

/// One question as asked and answered, for the training record.
fn decision(name: &str, criteria: &BTreeMap<String, String>, c: &Choice) -> Value {
    json!({
        "question": name,
        "options": criteria,
        "choice": c.choice,
        "confidence": c.confidence,
        "probabilities": c.probabilities,
    })
}

/// The first sentence of an error, short enough to speak.
fn short(msg: &str) -> String {
    let end = ["; ", ". ", "\n"]
        .iter()
        .filter_map(|p| msg.find(p))
        .min()
        .unwrap_or(msg.len());
    let first = msg[..end].trim().trim_end_matches('.');
    let mut s: String = first.chars().take(120).collect();
    if s.is_empty() {
        s = "it failed".into();
    }
    s
}

fn seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.finish() | 1
}

/// Two different NATO words.
fn new_callsign(seed: &mut u64) -> String {
    let mut next = || {
        // xorshift64
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        (*seed % NATO.len() as u64) as usize
    };
    let a = next();
    let mut b = next();
    while b == a {
        b = next();
    }
    format!("{} {}", NATO[a], NATO[b])
}

/// Every call sign this module can make, for tests and keyword lists.
pub fn is_agent_style(sign: &str) -> bool {
    let words: Vec<&str> = sign.split(' ').collect();
    words.len() == 2
        && words[0] != words[1]
        && words.iter().all(|w| NATO.contains(w))
        && words.iter().collect::<HashSet<_>>().len() == 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callsigns_are_two_different_nato_words() {
        let mut s = seed();
        for _ in 0..500 {
            let c = new_callsign(&mut s);
            assert!(is_agent_style(&c), "{c}");
        }
    }

    #[test]
    fn short_error_is_the_first_sentence() {
        assert_eq!(
            short("\"72\" is not a NOAA weather channel; use WX1-WX7"),
            "\"72\" is not a NOAA weather channel"
        );
        assert_eq!(short(""), "it failed");
    }
}
