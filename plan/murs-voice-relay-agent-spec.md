# MURS Voice-Relay Agent — Reference Specification

**Version:** 1.0
**Target:** onboard SBC (Raspberry Pi 4/5 or equivalent, Linux/ALSA)
**Language:** Rust (edition 2021)
**Status:** implementable reference for a dev agent

---

## 1. Overview & scope

A headless Linux service that gives an upstream **agent** (any decision/LLM process) a voice presence on a certified **MURS** radio, so it can speak to a person holding a handheld and — optionally — hear that person key back.

The service owns exactly one job: turn `{"say": "text"}` into correctly-sequenced, legal RF, and (Phase 2) turn received audio into `{"heard": "text"}`. It exposes a small local IPC boundary so the agent brain stays completely decoupled from radio timing, PTT, and legal guardrails.

**In scope (v1):** text-to-speech → PTT-keyed transmit on one MURS channel, listen-before-transmit, fail-safe PTT, a JSON control socket, config.
**In scope (Phase 2, optional):** receive → VAD → speech-to-text → `heard` events.
**Out of scope:** any SDR/non-certified transmit path; anything above 2 W (physically bounded by the radio); marine-VHF interfacing.

### System context

```
Agent brain (Rust)  ──JSON/Unix socket──►  voice-relay service  ──►  Digirig Mobile  ──►  BTECH MURS-V2 (fixed, on boat)
        ▲                                        │  PTT + audio           USB-C              MURS ch 3, 151.940 MHz
        └──────── heard/tx events ───────────────┘
                                                                   Captain holds 2nd MURS-V2 on ch 3
```

Hardware (fixed by the earlier build): certified MURS radio with a Kenwood **K1 2-pin** accessory port, a Digirig Mobile (CM108-class codec + CH340 serial), and the Digirig K1 cable. **The radio is FCC Part 95J certified; the service must never be pointed at a non-certified transmitter.**

---

## 2. Regulatory requirements (these are hard requirements, not guidance)

These are compiled into behavior, not left to the operator. A dev agent must implement all of them.

| ID | Requirement |
|----|-------------|
| **R1** | Certified radio only. The audio/PTT path connects to a Part 95-certified radio. Never route TX audio/PTT to an SDR or uncertified transmitter. |
| **R2** | **Listen-before-transmit (LBT).** Before every key-up, sample the channel; if busy, back off and retry. Never key a busy channel. |
| **R3** | **Half-duplex.** Never assert PTT while receive audio is active. TX and RX are mutually exclusive. |
| **R4** | **Max single-transmission duration** (`max_tx_seconds`, default 30). Messages longer than this are split into multiple transmissions separated by gaps. |
| **R5** | **No continuous carrier.** PTT is released between messages; enforce `inter_msg_gap_ms` (default 1500). |
| **R6** | **Fail-safe PTT release** on normal completion, error, panic, and process exit. Prefer serial-RTS PTT because the OS drops RTS when the port closes on process death (auto-release even on SIGKILL). |
| **R7** | **Max-key-time watchdog.** An independent timer force-releases PTT if a key-up ever exceeds `max_tx_seconds + tail`. |
| **R8** | **Station ID.** MURS has **no** ID requirement → default OFF. If reconfigured for GMRS, ID with the licensee callsign at least every 15 minutes of a conversation and at the end (`id_enabled`, `id_callsign`, `id_interval_secs`). |
| **R9** | Power/deviation are set by the certified radio and cannot be raised in software; the service must not attempt to. |

> Why RTS PTT matters (R6): a crashed process that latches a CM108 GPIO high keys the transmitter indefinitely — an illegal continuous carrier that also jams the channel. A serial RTS line is released by the kernel on port close, so the transmitter un-keys even on `kill -9`. Use RTS PTT unless you have a specific reason not to.

---

## 3. Architecture

Single process, a few threads, `crossbeam-channel` between them (matches the reader-thread + channels pattern used elsewhere in the stack).

```
                    ┌─────────────────────────────────────────────┐
   Unix socket ───► │ Control server (thread-per-conn)             │
   (agent)          │   parse commands → SayRequest                │
                    └───────────────┬─────────────────────────────┘
                                    │ tx_queue (crossbeam)
                                    ▼
                    ┌─────────────────────────────────────────────┐
                    │ TX worker (single thread, serializes RF)     │
                    │   LBT → key → lead → play(TTS) → tail → unkey │
                    │   watchdog + RAII PTT guard                  │
                    └───────────────┬─────────────────────────────┘
                                    │ events (crossbeam, fanned out)
                                    ▼
                            events → agent (tx_start/tx_done/channel_busy/heard)

   [Phase 2] RX worker: arecord → VAD/squelch segmenter → STT → "heard" event
```

Key design points:
- **One TX worker thread** is the only thing that touches PTT and playback → RF is serialized, R3/R5 are structurally guaranteed.
- **RAII PTT guard** (`Drop` releases the key) guarantees R6 within the process; RTS-on-close guarantees it across process death.
- **Audio via ALSA subprocess (`aplay`/`arecord`)** for v1 — dramatically simpler and more robust on a Pi than in-process ring buffers, and Piper pipes straight to it. A pure-`cpal` path is offered as an alternative (§7.6).

---

## 4. Control API

Transport: `SOCK_STREAM` Unix domain socket at `control_socket` (default `/run/mursrelay.sock`). Protocol: **newline-delimited JSON** (one object per line), bidirectional. The agent connects, writes commands, and reads events on the same connection.

### Commands (agent → service)

```jsonc
// speak text; queued and transmitted when the channel is clear
{"type":"say","text":"Captain, fuel at twenty percent.","priority":"normal"} // priority: "normal" | "high"

// drop everything queued (e.g., situation changed)
{"type":"flush"}

// health check
{"type":"ping"}
```

`priority:"high"` jumps the queue and uses a longer LBT max-wait before deferring (see §7.4). It never overrides R2/R3 — it will still wait for a clear channel.

### Events (service → agent)

```jsonc
{"type":"tx_start","text":"...","ts":1720483200.12}
{"type":"tx_done","text":"...","ts":1720483205.44}
{"type":"channel_busy","waiting_ms":4200}      // emitted when LBT is deferring
{"type":"dropped","text":"...","reason":"queue_full"|"flushed"|"too_long"}
{"type":"heard","text":"copy that","ts":...}   // Phase 2 only
{"type":"pong"}
```

The boundary is deliberately narrow: the agent decides *what* to say; the service decides *when it is legal to say it* and reports back.

---

## 5. Configuration (`/etc/mursrelay/config.toml`)

```toml
# --- radio identity (informational + ID behavior) ---
channel_label   = "MURS 3 (151.940 MHz)"
service         = "murs"          # "murs" | "gmrs"  (affects station-ID default)

# --- audio devices (ALSA names; find with `aplay -l` / `arecord -l`) ---
playback_device = "plughw:CARD=Device,DEV=0"   # Digirig output → radio mic
capture_device  = "plughw:CARD=Device,DEV=0"   # radio speaker → Digirig input

# --- PTT ---
ptt_method      = "rts"           # "rts" (recommended) | "dtr" | "cm108" | "none"(dry-run)
ptt_serial_port = "/dev/ttyUSB0"  # Digirig CH340 serial (rts/dtr only)
ptt_cm108_hid   = ""              # e.g. "/dev/hidraw0" (cm108 only; prefer hamlib, see §7.2)

# --- TTS (Piper, offline) ---
tts_engine      = "piper"
piper_bin       = "/usr/local/bin/piper"
piper_model     = "/etc/mursrelay/voices/en_US-lessac-medium.onnx"

# --- timing (ms) ---
ptt_lead_ms     = 250     # after key-up, before audio: TX rise + captain RX squelch open
ptt_tail_ms     = 150     # after audio, before un-key
inter_msg_gap_ms = 1500   # enforced gap between transmissions (R5)
max_tx_seconds  = 30      # per-transmission cap (R4/R7)

# --- listen-before-transmit (R2) ---
lbt_window_ms   = 300     # capture window used to judge channel
lbt_rms_threshold = 500   # i16 RMS above this = channel busy (tune per radio squelch)
lbt_retry_ms    = 400     # backoff between LBT attempts
lbt_max_wait_normal_ms = 8000
lbt_max_wait_high_ms   = 30000

# --- control ---
control_socket  = "/run/mursrelay.sock"
queue_capacity  = 32

# --- station ID (R8) ---
id_enabled      = false            # MURS: false. GMRS: true.
id_callsign     = ""               # e.g. "WRXY123"
id_interval_secs = 900             # ≤900 for GMRS
```

---

## 6. Dependencies (`Cargo.toml`)

```toml
[package]
name = "mursrelay"
version = "1.0.0"
edition = "2021"

[dependencies]
anyhow = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
crossbeam-channel = "0.5"
serialport = "4"          # RTS/DTR PTT
log = "0.4"
env_logger = "0.11"
# optional / Phase 2 or alternative audio path:
# hidapi = "2"            # only if using raw CM108 GPIO PTT (prefer hamlib instead)
# cpal   = "0.15"         # only for the pure-Rust audio path (§7.6)
# hound  = "3"            # WAV read for cpal path
```

External binaries expected on the image: `piper`, `aplay`, `arecord` (ALSA utils). Phase 2 adds `whisper.cpp` (`whisper-cli`) or `vosk`.

---

## 7. Module reference (Rust)

### 7.1 PTT trait + fail-safe guard

```rust
use anyhow::Result;

/// Push-to-talk control. `key` asserts transmit, `unkey` releases.
pub trait Ptt: Send {
    fn key(&mut self) -> Result<()>;
    fn unkey(&mut self) -> Result<()>;
}

/// RAII guard: keys on construction, GUARANTEES un-key on drop
/// (covers early return, `?`, and panic within the TX thread — R6).
pub struct KeyedTx<'a> {
    ptt: &'a mut dyn Ptt,
    active: bool,
}

impl<'a> KeyedTx<'a> {
    pub fn new(ptt: &'a mut dyn Ptt) -> Result<Self> {
        ptt.key()?;
        Ok(Self { ptt, active: true })
    }
    /// Explicit release so we can surface un-key errors on the happy path.
    pub fn release(mut self) -> Result<()> {
        self.active = false;
        self.ptt.unkey()
    }
}

impl<'a> Drop for KeyedTx<'a> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.ptt.unkey(); // best-effort; never panic in Drop
        }
    }
}
```

### 7.2 PTT implementations

```rust
use serialport::SerialPort;
use std::time::Duration;

/// Recommended: PTT via the serial control line (RTS or DTR) on the Digirig CH340.
/// Fails safe — the kernel drops the line when the port closes on process death (R6).
pub struct SerialLinePtt {
    port: Box<dyn SerialPort>,
    use_rts: bool, // true = RTS, false = DTR
}

impl SerialLinePtt {
    pub fn open(path: &str, use_rts: bool) -> Result<Self> {
        let port = serialport::new(path, 9600)
            .timeout(Duration::from_millis(100))
            .open()?;
        let mut me = Self { port, use_rts };
        me.unkey()?; // ensure idle at startup
        Ok(me)
    }
}

impl Ptt for SerialLinePtt {
    fn key(&mut self) -> Result<()> {
        if self.use_rts { self.port.write_request_to_send(true)?; }
        else            { self.port.write_data_terminal_ready(true)?; }
        Ok(())
    }
    fn unkey(&mut self) -> Result<()> {
        if self.use_rts { self.port.write_request_to_send(false)?; }
        else            { self.port.write_data_terminal_ready(false)?; }
        Ok(())
    }
}

/// Dry-run PTT: logs instead of keying. Used by --dry-run and CI.
pub struct NullPtt;
impl Ptt for NullPtt {
    fn key(&mut self) -> Result<()> { log::info!("[dry-run] PTT key"); Ok(()) }
    fn unkey(&mut self) -> Result<()> { log::info!("[dry-run] PTT unkey"); Ok(()) }
}
```

> **CM108 GPIO PTT.** If your specific Digirig cable/config drives PTT through the CM108 GPIO rather than serial RTS, **do not hand-roll the HID feature report** — the exact GPIO byte layout differs across CM108/CM119/CM119B and getting it wrong latches the transmitter. Instead run `hamlib`'s `rigctld` with the `cm108` model and drive PTT via `rigctl` (or `libhamlib`/`hamlib` FFI), or use Direwolf's tested `cm108.c`. Wrap it behind the same `Ptt` trait. Confirm which path your cable uses with Digirig's own PTT test before wiring the service to it. RTS is preferred precisely because it avoids this.

### 7.3 TTS (Piper, offline)

```rust
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Synthesize `text` to a mono 16-bit WAV at `out` using Piper (offline).
/// Piper default output is 22050 Hz mono S16LE — aplay plays it directly.
pub fn synth_to_wav(piper_bin: &str, model: &Path, text: &str, out: &Path) -> Result<()> {
    let mut child = Command::new(piper_bin)
        .arg("--model").arg(model)
        .arg("--output_file").arg(out)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    child.stdin.take().expect("piper stdin").write_all(text.as_bytes())?;
    let status = child.wait()?;
    anyhow::ensure!(status.success(), "piper failed: {status}");
    Ok(())
}

/// Duration of a PCM WAV in seconds (used to enforce R4). Reads the WAV header.
pub fn wav_duration_secs(path: &Path) -> Result<f64> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hdr = [0u8; 44];
    f.read_exact(&mut hdr)?;
    let byte_rate = u32::from_le_bytes(hdr[28..32].try_into()?) as f64;
    let data_len  = u32::from_le_bytes(hdr[40..44].try_into()?) as f64;
    Ok(if byte_rate > 0.0 { data_len / byte_rate } else { 0.0 })
}
```

### 7.4 Channel-busy detector (LBT, R2)

The Digirig capture line carries the radio's **speaker** audio, which the radio only passes when its squelch is open. Set the radio's squelch so an idle channel is silent; then a simple RMS threshold cleanly distinguishes idle from busy.

```rust
use std::io::Read;
use std::process::{Command, Stdio};

/// Capture ~`window_ms` of mono S16LE at 16 kHz via arecord and return RMS.
pub fn channel_rms(capture_device: &str, window_ms: u64) -> Result<f32> {
    const RATE: u64 = 16_000;
    let n_samples = (RATE * window_ms / 1000) as usize;
    let n_bytes = n_samples * 2;

    let mut child = Command::new("arecord")
        .args(["-D", capture_device, "-q", "-f", "S16_LE", "-r", "16000",
               "-c", "1", "-t", "raw"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let mut buf = vec![0u8; n_bytes];
    child.stdout.take().expect("arecord stdout").read_exact(&mut buf)?;
    let _ = child.kill();
    let _ = child.wait();

    let mut sum_sq = 0f64;
    for ch in buf.chunks_exact(2) {
        let s = i16::from_le_bytes([ch[0], ch[1]]) as f64;
        sum_sq += s * s;
    }
    Ok(((sum_sq / n_samples.max(1) as f64).sqrt()) as f32)
}

pub fn channel_busy(dev: &str, window_ms: u64, threshold: f32) -> Result<bool> {
    Ok(channel_rms(dev, window_ms)? > threshold)
}
```

### 7.5 TX worker (LBT → key → lead → play → tail → unkey)

```rust
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

pub struct TxParams {
    pub playback_device: String,
    pub lead: Duration,
    pub tail: Duration,
    pub gap: Duration,
    pub max_tx: Duration,
    pub lbt_window_ms: u64,
    pub lbt_threshold: f32,
    pub lbt_retry: Duration,
    pub capture_device: String,
}

fn aplay(device: &str, wav: &Path) -> Result<()> {
    let status = Command::new("aplay")
        .args(["-D", device, "-q"]).arg(wav)
        .status()?;
    anyhow::ensure!(status.success(), "aplay failed: {status}");
    Ok(())
}

/// Transmit one already-synthesized clip. Enforces R2/R3/R4/R5/R6/R7.
/// Returns Ok(true) if transmitted, Ok(false) if deferred (channel stayed busy).
pub fn transmit_clip(
    ptt: &mut dyn Ptt,
    wav: &Path,
    clip_secs: f64,
    p: &TxParams,
    lbt_max_wait: Duration,
) -> Result<bool> {
    // R4: refuse over-long clips (caller should have split them).
    anyhow::ensure!(
        clip_secs <= p.max_tx.as_secs_f64(),
        "clip {clip_secs:.1}s exceeds max_tx {}s", p.max_tx.as_secs()
    );

    // R2: listen-before-transmit with bounded wait.
    let start = Instant::now();
    loop {
        if !channel_busy(&p.capture_device, p.lbt_window_ms, p.lbt_threshold)? {
            break;
        }
        if start.elapsed() >= lbt_max_wait {
            return Ok(false); // defer; caller re-queues or drops
        }
        thread::sleep(p.lbt_retry);
    }

    // Key up (guard guarantees release — R6).
    let guard = KeyedTx::new(ptt)?;

    // R7: independent watchdog force-releases if playback wedges.
    let watchdog_budget = p.lead + p.max_tx + p.tail + Duration::from_secs(2);
    let key_instant = Instant::now();

    thread::sleep(p.lead);            // TX rise + captain RX squelch open
    let play_res = aplay(&p.playback_device, wav);
    thread::sleep(p.tail);

    // Explicit release to surface un-key errors; guard still covers panic paths.
    guard.release()?;

    // Sanity check against the watchdog budget (log if exceeded).
    if key_instant.elapsed() > watchdog_budget {
        log::warn!("key-up exceeded watchdog budget");
    }
    play_res?;

    thread::sleep(p.gap);            // R5: enforced inter-message gap
    Ok(true)
}
```

> **Hard watchdog (recommended).** For defense in depth beyond the in-thread budget above, run a second thread that, whenever PTT is keyed, arms a timer and calls `unkey()` if the key persists past `max_tx + tail + margin`. Share PTT state via `Arc<Mutex<>>` or an `AtomicInstant`-style flag. The RAII guard covers panics; the watchdog covers deadlocks in `aplay`.

### 7.6 Alternative audio path (pure Rust, no ALSA subprocess)

If you must avoid `aplay`/`arecord`, replace §7.3–7.5 playback/capture with `cpal`: open the Digirig output stream, select it by name, and feed samples from a bounded buffer in the data callback; for capture, accumulate input-callback samples and compute RMS over a sliding window. Add `cpal = "0.15"` and `hound = "3"`. This is more code and more failure modes on a Pi; only do it if the subprocess approach is unacceptable. The PTT, sequencing, LBT logic, and control API are unchanged.

### 7.7 Control server (Unix socket, JSON lines)

```rust
use crossbeam_channel::{Sender, Receiver};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Command {
    Say { text: String, #[serde(default)] priority: Priority },
    Flush,
    Ping,
}

#[derive(Deserialize, Clone, Copy, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Priority { #[default] Normal, High }

#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    TxStart { text: String, ts: f64 },
    TxDone  { text: String, ts: f64 },
    ChannelBusy { waiting_ms: u64 },
    Dropped { text: String, reason: String },
    Heard   { text: String, ts: f64 },
    Pong,
}

pub struct SayRequest { pub text: String, pub priority: Priority }

/// Simple event fan-out: connected clients register a sender here.
pub type EventBus = Arc<Mutex<Vec<Sender<Event>>>>;

pub fn broadcast(bus: &EventBus, ev: &Event) {
    let mut subs = bus.lock().unwrap();
    subs.retain(|s| s.send(ev.clone()).is_ok()); // drop dead clients
}

pub fn run_control_server(
    socket_path: &str,
    tx_queue: Sender<SayRequest>,
    flush_flag: Arc<Mutex<bool>>,
    bus: EventBus,
) -> Result<()> {
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    for stream in listener.incoming() {
        let stream = stream?;
        let tx_queue = tx_queue.clone();
        let flush_flag = flush_flag.clone();
        let bus = bus.clone();
        thread::spawn(move || {
            if let Err(e) = handle_conn(stream, tx_queue, flush_flag, bus) {
                log::warn!("control conn ended: {e}");
            }
        });
    }
    Ok(())
}

fn handle_conn(
    stream: UnixStream,
    tx_queue: Sender<SayRequest>,
    flush_flag: Arc<Mutex<bool>>,
    bus: EventBus,
) -> Result<()> {
    // register for events
    let (ev_tx, ev_rx): (Sender<Event>, Receiver<Event>) = crossbeam_channel::unbounded();
    bus.lock().unwrap().push(ev_tx);

    // writer thread: events → client
    let mut wr = stream.try_clone()?;
    thread::spawn(move || {
        for ev in ev_rx.iter() {
            let line = serde_json::to_string(&ev).unwrap();
            if writeln!(wr, "{line}").is_err() { break; }
            let _ = wr.flush();
        }
    });

    // reader loop: client → commands
    let rd = BufReader::new(stream);
    for line in rd.lines() {
        let line = line?;
        if line.trim().is_empty() { continue; }
        match serde_json::from_str::<Command>(&line) {
            Ok(Command::Say { text, priority }) => {
                let _ = tx_queue.send(SayRequest { text, priority });
            }
            Ok(Command::Flush) => { *flush_flag.lock().unwrap() = true; }
            Ok(Command::Ping)  => broadcast(&bus, &Event::Pong),
            Err(e) => log::warn!("bad command: {e}"),
        }
    }
    Ok(())
}
```

### 7.8 Wiring (`main`)

```rust
fn main() -> anyhow::Result<()> {
    env_logger::init();
    let cfg = load_config("/etc/mursrelay/config.toml")?;   // toml → Config struct

    // PTT
    let mut ptt: Box<dyn Ptt> = match cfg.ptt_method.as_str() {
        "rts"  => Box::new(SerialLinePtt::open(&cfg.ptt_serial_port, true)?),
        "dtr"  => Box::new(SerialLinePtt::open(&cfg.ptt_serial_port, false)?),
        "none" => Box::new(NullPtt),
        // "cm108" => Box::new(HamlibPtt::open(...)?),   // see §7.2
        other  => anyhow::bail!("unsupported ptt_method: {other}"),
    };

    let (tx_send, tx_recv) = crossbeam_channel::bounded::<SayRequest>(cfg.queue_capacity);
    let flush_flag = std::sync::Arc::new(std::sync::Mutex::new(false));
    let bus: EventBus = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    // control server
    {
        let (tx, ff, b) = (tx_send.clone(), flush_flag.clone(), bus.clone());
        let sock = cfg.control_socket.clone();
        std::thread::spawn(move || { let _ = run_control_server(&sock, tx, ff, b); });
    }

    // TX worker (owns PTT, serializes RF)
    let params = cfg.tx_params();
    let piper = (cfg.piper_bin.clone(), cfg.piper_model.clone());
    let bus2 = bus.clone();
    tx_worker_loop(&mut *ptt, tx_recv, flush_flag, params, piper, bus2)?;
    Ok(())
}
```

Sketch of `tx_worker_loop`: drain the queue (high-priority first), synth each `SayRequest` to a temp WAV, check `wav_duration_secs`; if it exceeds `max_tx`, split the text into sentence chunks and enqueue as multiple clips (or emit `dropped{reason:"too_long"}` if a single sentence is still too long); emit `tx_start`, call `transmit_clip(...)`, emit `tx_done` or (on defer) `channel_busy`/`dropped`; honor `flush_flag` between clips. All PTT access stays on this one thread.

---

## 8. Phase 2 — receive path (optional)

Enables the captain→agent direction so it's a real two-way link.

- **Capture:** continuous `arecord` mono 16 kHz off `capture_device`. Because the radio squelches idle audio, presence of audio ≈ a received transmission.
- **Segment:** squelch-gated (RMS opens/closes a segment) or WebRTC/Silero VAD to bound utterances. End a segment after ~800 ms of silence.
- **STT (offline):** pipe each segment WAV to `whisper.cpp` (`whisper-cli -m ggml-base.en.bin -f seg.wav -otxt`) or `vosk` for streaming. Whisper = better accuracy; Vosk = lighter/streaming.
- **Emit:** `{"type":"heard","text":...,"ts":...}` on the event bus.
- **R3 interaction:** while a segment is open (RX active), the TX worker must not key. Share an `AtomicBool rx_active`; `transmit_clip` treats `rx_active == true` as channel-busy for LBT.

Keep STT models on the image (offline). `base.en` runs acceptably on a Pi 5; use `tiny.en` on a Pi 4 if latency matters.

---

## 9. Testing & acceptance

| Test | Method | Pass criteria |
|------|--------|---------------|
| Dry-run end-to-end | `ptt_method="none"`, `playback_device` = laptop speakers | `say` produces spoken audio; `tx_start`/`tx_done` events fire; no PTT asserted |
| PTT sequencing | scope/meter or a second radio | key → 250 ms → audio → 150 ms → un-key, in order |
| **Stuck-PTT fail-safe (R6)** | key a transmission, `kill -9` mid-clip | transmitter un-keys within one serial-close (RTS drops); channel clears |
| Watchdog (R7) | inject a hang in `aplay` (e.g., wrong device) | PTT force-released within `max_tx + margin` |
| LBT (R2) | hold the captain's radio keyed, send `say` | service defers, emits `channel_busy`; transmits only after channel clears |
| Half-duplex (R3) | Phase 2: speak into captain radio while agent tries to TX | agent waits until RX segment closes |
| Max length (R4) | `say` a 90 s message | split into ≤30 s transmissions with gaps, or `dropped{too_long}` for an unsplittable sentence |
| Gap (R5) | back-to-back `say` | ≥ `inter_msg_gap_ms` between key-ups; PTT released between |
| RF check | dummy load + second radio | intelligible audio on ch 3; no clipping of first/last words |

Bench everything into a **dummy load** (or at minimum a second radio a few feet away) before an antenna. Verify the captured audio isn't clipped at the front (increase `ptt_lead_ms`) or the tail (increase `ptt_tail_ms`).

---

## 10. Operational notes

- **Autostart:** ship a `systemd` unit (`Restart=on-failure`, `After=sound.target`). On restart, `SerialLinePtt::open` calls `unkey()` first, so a crash-restart cannot leave the line asserted.
- **Power:** run the agent radio from USB-C, not its battery, for 24/7 operation.
- **Radio programming:** program both radios to MURS ch 3 (151.940 MHz), narrowband, matching CTCSS/DCS tone so they only open squelch to each other. Set the agent radio's squelch so an idle channel is silent on the capture line (this is what makes the RMS LBT reliable).
- **Device names:** ALSA card names can reorder across reboots; pin the Digirig with a `plughw:CARD=<name>` selector (from `aplay -l`) or a udev rule, not `hw:1,0`.
- **GMRS swap:** set `service="gmrs"`, `id_enabled=true`, `id_callsign="<your WRXY...>"`, point the audio/PTT at a certified GMRS radio (same K1 cable). Everything else is unchanged. Requires the $35 GMRS license.

---

## 11. Deliverables checklist for the dev agent

- [ ] `Ptt` trait + `SerialLinePtt` (RTS/DTR) + `NullPtt` + `KeyedTx` guard
- [ ] Piper TTS wrapper + WAV duration check + text splitter for R4
- [ ] LBT channel-busy detector (arecord RMS)
- [ ] TX worker: queue drain, priority, LBT, key/lead/play/tail/unkey, gap, watchdog
- [ ] Control server: Unix socket, JSON-line commands + event fan-out
- [ ] Config loader (TOML) + `--dry-run`
- [ ] `systemd` unit
- [ ] Acceptance tests from §9 (esp. the stuck-PTT fail-safe)
- [ ] (Phase 2) RX capture → VAD/squelch → whisper.cpp/vosk → `heard`, with `rx_active` gating TX
