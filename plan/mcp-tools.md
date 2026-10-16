# Every hertzd function as an MCP tool

Goal: an agent can do everything over `/mcp` that a person can do with the REST
API, the TUI, the config file or a restart. That covers reading what the radio
heard, tuning and scanning, recording and listening, weather, the Hyperia relay
and Lume. Kord asked for this on 2026-10-16, after `weather_status` and
`weather_set` shipped (commit `3f78565`).

This plan takes in phases 1–3 of [live-tuning.md](live-tuning.md); that plan's
design (ScanControl, levels, auto-tune, `tuning.json`) still holds.

## Today

- **MCP endpoint:** `/mcp` is hand-rolled JSON-RPC on the axum port (`initialize`,
  `ping`, `tools/list`, `tools/call`, batches, notifications). It uses the REST
  auth rule and refuses a non-localhost `Origin`.
- **MCP tools:** only `weather_status` and `weather_set`.
- **How tools are wired:** they're a `match` in `server/mcp.rs`, and only the
  weather logic is shared with REST.
- **REST endpoints with no MCP tool:**
  - status
  - dongles
  - channels
  - tune (does nothing while scanning)
  - squelch (works live)
  - recording
  - listen
  - activity
  - transcriptions
  - recordings list and get
  - entropy
  - time
  - doctor
  - the `/stream`, `/events` and `/audio` streams
- **Fixed at start or not exposed anywhere:**
  - scan list and priority
  - lockouts
  - gain
  - the scanner's current channel
  - relay on/off and threshold
  - the server double-check
  - Lume target and queue
  - live transcription
  - transcription keywords

## Tool catalogue

Names carry no `hertz_` prefix: clients already namespace by server
(`mcp__hertz__scan_set`). `dongle` is optional on every tool when one dongle is
attached. RO marks read-only tools, which get MCP `readOnlyHint`.

### Read: what's happening

| Tool | RO | Returns | Backend | Phase |
|---|---|---|---|---|
| `status` | ✓ | Version, uptime, each dongle (online, role, freq, squelch, gain, recording, listening), **the scanner's current channel and state** (scanning / call / weather / hold), weather and relay summary | `/api/status` + `/api/dongles` + new scan telemetry | 1 (current channel in 2) |
| `doctor` | ✓ | Platform, channels loaded, per-dongle health, warnings, plus service checks: Whistle, ollaya, Hyperia, the verify server, Lume | `/api/doctor` + new checks | 1 |
| `channels` | ✓ | Channels by `group` or `ids`, with frequency, mode and whether each is in the scan | `/api/channels` | 1 |
| `transcripts` | ✓ | Recent transcripts filtered by `channel`, `since`, `limit` and a `contains` text match; each with its recording file and the live/server variants when they differ | `/api/transcriptions` + recorder files | 1 |
| `activity` | ✓ | Squelch opens/closes with signal, classification and duration | `/api/activity` | 1 |
| `recordings` | ✓ | Recording files filtered by `channel`, `since`, `limit` | `/api/recordings` | 1 |
| `recording` | ✓ | One recording: metadata, transcript, path; `audio: true` adds it as an MCP audio content block (WAV, capped at 2 MB) | `/api/recordings/:file` | 1 |
| `wait_for_call` | ✓ | Blocks up to `timeout_s` (max 120) for the next transcript, optionally on given `channels`; returns it, or `null` on timeout. Lets an agent listen without polling | event bus | 1 |
| `levels` | ✓ | Per channel over the last N s: p50/p95/max signal, floor, threshold, fraction open, lockout | scan telemetry rings | 2 |
| `time` | ✓ | Daemon clock (unix and UTC) | `/api/time` | 1 |
| `weather_status` | ✓ | *(done)* | WeatherControl | ✅ |

### Control: receiver

| Tool | Does | Backend | Phase |
|---|---|---|---|
| `set_squelch` | Default margin (`squelch_db`), and later per-channel margin or absolute `open_db` | squelch already live; per-channel needs ScanControl | 1 (default), 2 (per channel) |
| `set_recording` | Recording on/off | works live | 1 |
| `set_listen` | Speaker on/off | works live | 1 |
| `set_gain` | Fixed dB (0–49.6) or `auto` | `WorkerHandle::set_gain` (works live) | 2 |
| `hold` | Stay on a channel or raw frequency for N s (or until resumed): replaces `tune`, which is a no-op while scanning | ScanCmd::Hold | 2 |
| `resume` | End a hold and go back to scanning | ScanCmd::Resume | 2 |

### Control: scanner

| Tool | Does | Backend | Phase |
|---|---|---|---|
| `scan_get` | Scan list, groups, priority, lockouts with time left, scan constants | ScanControl | 2 |
| `scan_set` | Replace, or `add` / `remove` entries (ids or band groups) and set `priority`; applied between hops, never mid-call; learned floors kept | ScanControl epoch | 2 |
| `lockout` | Lock a channel out for N minutes or permanently, with a reason | ScanCmd::LockOut | 2 |
| `unlock` | Unlock one channel or all | ScanCmd::Unlock | 2 |
| `autotune` | start (passive / dwell), status, cancel; returns the report | live-tuning auto-tune | 3 |

### Control: weather

| Tool | Does | Phase |
|---|---|---|
| `weather_set` | *(done)* on/off, channels, interval | ✅ |
| `weather_sample_now` | Take one sample now without changing the schedule | 1 |
| `weather_latest` | Latest sample per channel (transcript, signal, time, WAV path) without going through Lume | 1 |

### Relay (Hyperia + ollaya)

| Tool | RO | Does | Phase |
|---|---|---|---|
| `relay_status` | ✓ | On/off, Hyperia and ollaya reachable, `min_probability`, the live call-sign list, the last N routing decisions (heard, pane, p, confirm, sent/no match) | 2 |
| `relay_set` | | On/off, `min_probability`, server double-check on/off | 2 |
| `relay_resend` | | Route a past transcript again (by recording file), for example after a pane came back | 3 |
| `retranscribe` | | Run a recording through the transcription server (medium) and return the text, without routing it | 3 |

`relay_send` (route arbitrary text to a pane) is left out on purpose: it would
let any MCP caller post as `hertz-radio`. See the open questions.

### Lume

| Tool | RO | Does | Phase |
|---|---|---|---|
| `lume_status` | ✓ | Target URL and how it was found (config, discovery file, default), connected or not, records queued (of 2000), last error | 2 |
| `lume_reconnect` | | Run discovery again now (after Lume restarts on a new port) | 2 |

### Transcription

| Tool | RO | Does | Phase |
|---|---|---|---|
| `transcription_status` | ✓ | Engine, language, live on/off, keywords (configured + live call signs) | 2 |
| `transcription_set` | | Live on/off; extra keywords (place names, vessel names) | 3 |

### Any frequency, any mode: FM broadcast, sweeps, AIS and other data

Kord's addition: tune to an FM broadcast station, scan FM or other ranges, and
detect AIS and similar. An RTL-SDR covers about 24–1766 MHz. hertz demodulates
only NFM and AM today (`hertz_dsp::demod::Mode`), and the monitor role captures
240 kS/s.

| Tool | Does | New backend | Phase |
|---|---|---|---|
| `tune` | Any frequency with a `mode`: `nfm`, `am`, `wfm` (broadcast FM), later `usb`/`lsb`. Holds there until `resume` (it's `hold` with a mode). Plays on the speaker; recording and transcription optional (`transcribe: true` for talk radio) | **WFM demod**: 200 kHz wide, a capture of about 1 MS/s, then decimate; 75 µs de-emphasis (US). Mono first; stereo and RDS later | 2 (NFM/AM), 3 (WFM) |
| `sweep` | Step across `start_hz`..`stop_hz` (for example 88–108 MHz, or 150–174 MHz) at `step_hz` with a short dwell, and return the active frequencies: power over the floor, occupied width, and a guess at the kind (WFM broadcast, NFM voice, steady carrier, data burst) | Sweep loop in the scan thread, using FFT power per step at 2.4 MS/s (2 MHz per step, so 88–108 MHz takes 10 steps) | 3 |
| `scan_range` | Like `scan_set`, but for a frequency range instead of band-plan ids: hop the active frequencies `sweep` found, with squelch, and record and transcribe voice | Build the scan list from sweep results (ad-hoc channels outside the band plans) | 3 |
| `ais_start` / `ais_stop` | Decode AIS on 161.975 and 162.025 MHz (both fit one 240 kS/s capture at 162.000 MHz). GMSK 9600 baud → HDLC → NMEA `!AIVDM` → position, MMSI, ship name, course and speed | **New decoder** (`hertz-dsp/src/decode/ais.rs`); the `marine-ais` band plan already exists | 3 |
| `ais_targets` | Vessels heard in the last N minutes: MMSI, name, position, course/speed, last seen, signal | AIS target table; also sent to Lume as `radio.ais` (and could feed Signal K) | 3 |
| `decoders` | Status and on/off for every decoder: AIS, and later DSC (marine ch 70, 1200 baud FSK: distress and digital calls), APRS (144.390 MHz AFSK 1200), ADS-B (1090 MHz: needs 2 MS/s and its own dongle) | Decoder registry | 4 |

**One dongle is one frequency window.** The `tune` and `sweep` tools and the AIS
decoder share the dongle with the scan, as weather does. Each takes receive
offline (`RADIO RECEIVE OFFLINE | tuned to 101.5 MHz WFM` or `| sweeping 88–108
MHz`), publishes `ScanState`, and returns to scanning when done. With a second
dongle, a `[[dongle]] role = "decoder"` (or `"wideband"`) can run AIS full
time, and `tune`, `sweep` and the decoders take a `dongle` argument. `status`
shows which dongle is doing what.

**WFM needs a wider capture.** The monitor role runs at 240 kS/s, too narrow for
a 200 kHz broadcast. A `tune` in WFM switches the worker to about 1 MS/s and
back to 240 kS/s on `resume`. The device already has `set_sample_rate`, but the
worker thread only takes `Retune` and `SetGain` (`hertz-sdr/src/lib.rs`), so a
`WorkerCmd::SetSampleRate` is needed, plus the read size and frame math that
follow the rate. `sweep` at 2.4 MS/s needs the same command.

### Left out

- **`/stream`, `/events` and `/audio`:** live streams belong on WebSocket/SSE.
  `wait_for_call` covers what an agent needs from them.
- **`/api/entropy`:** an RNG tap, not radio control. It could be added
  read-only if wanted.
- **`hertz-tx`:** transmit isn't built yet.

## Design

### One operations layer for REST and MCP

New `crates/hertz-daemon/src/ops/`: one function per operation, taking typed
arguments and returning `Result<Value, OpError>`. The REST handlers and MCP tools
are both thin wrappers over it, so the two can't drift (`weather_set` already
works this way). `OpError` maps to an HTTP status code and to MCP `isError`.

### Tool registry instead of a `match`

```rust
struct Tool {
    name: &'static str,
    description: &'static str,
    input_schema: fn() -> Value,   // JSON Schema
    read_only: bool,
    call: fn(&DaemonState, Value) -> BoxFuture<Result<Value, OpError>>,
}
```

`tools/list` is generated from the registry, with `annotations.readOnlyHint` and
`destructiveHint`. A test checks that each tool's schema accepts its own
examples and that each REST mirror returns the same JSON.

### ScanControl (from live-tuning.md)

Shared scan state the scan loop reads each frame, following the same epoch
pattern as `WeatherControl`:
- the scan list and priority;
- a command channel for lockout, unlock, hold and resume;
- telemetry rings written with `try_lock`.

This one refactor unlocks `scan_get`/`scan_set`, `lockout`/`unlock`,
`hold`/`resume`, `levels`, per-channel squelch and the current channel in
`status`.

### Audit

Every control call publishes `Event::Tuning { actor: "mcp" | "rest" | "scanner",
action, target, before, after }`. It's logged and sent to Lume as `radio.tuning`,
so changes to the radio can always be traced afterwards.

### Persistence

Phase 2 writes a minimal `tuning.json` (squelch, scan list, lockouts, gain,
weather, relay threshold), and it overrides the TOML at start. Phase 3 adds
the rest and `reset_tuning` (DELETE /api/tuning).

### Safety

- **Read and write the same rule:** loopback is exempt; off loopback a bearer
  token is required.
- **Off loopback, write tools need a token:** if no `auth_token` is configured,
  they return 403 even when reads are open. That's stricter than `check_auth`
  today.
- **Origin check:** stays as it is.
- **Bind address:** hertzd refuses to bind `0.0.0.0` or `::` (project rule).
- **Limits:** argument ranges are validated; `wait_for_call` is capped at 120 s;
  recording audio is capped at 2 MB; one autotune job per dongle.

## Phases

| Phase | Work | Size | Gets Kord |
|---|---|---|---|
| **1** | ops layer and registry; wrap all existing REST: `status`, `doctor`, `channels`, `transcripts`, `activity`, `recordings`, `recording`, `time`, `set_squelch` (default), `set_recording`, `set_listen`; new `wait_for_call`, `weather_sample_now`, `weather_latest` | about a day | An agent reads everything and flips what works live today |
| **2** | ScanControl refactor; `scan_get`/`scan_set`, `lockout`/`unlock`, `hold`/`resume`, `levels`, per-channel squelch, `set_gain`; current channel in `status`; `relay_status`/`relay_set`; `lume_status`/`lume_reconnect`; `transcription_status`; `Event::Tuning`; minimal `tuning.json` | 2–3 days | Full live control with no restarts, and changes survive restarts |
| **3** | `autotune`; `relay_resend`, `retranscribe`, `transcription_set`; full `tuning.json` and reset; **WFM demod and `tune` in WFM; `sweep` and `scan_range`; AIS decoder with `ais_start`/`ais_stop`/`ais_targets`** | 4–5 days (WFM about 1, sweep about 1, AIS about 2) | Broadcast FM, finding what's on the air, vessels on AIS |
| **4** | `decoders`: DSC (ch 70), APRS, ADS-B on a second dongle; WFM stereo/RDS; a `decoder` dongle role | open | More of the spectrum |

## Tests

- **Unit:**
  - each op's argument validation;
  - the registry: unique names, schemas valid, read-only flags right;
  - the REST mirror and the MCP tool return the same JSON;
  - Origin and auth rules, including write-off-loopback without a token.
- **Mock integration** (`tests/mcp.rs`, 127.0.0.1 port 0):
  - every phase-1 tool against the mock daemon;
  - `wait_for_call` returns the mock voice burst's transcript (fixed
    transcriber) and times out on silence;
  - phase 2: `scan_set` add/remove is seen in `ScanState` hops, lockout skips a
    channel, `hold` stays on a channel and `resume` leaves it.
- **On air:** Kord drives each phase-2 control from Claude Code while keying
  up on 72 and GMRS 1.

## Open questions for Kord

1. **Tool names:** plain (`scan_set`) or prefixed (`hertz_scan_set`)? Clients
   namespace anyway; the plan uses plain names.
2. **`relay_send`:** should an agent be able to post arbitrary text as
   `hertz-radio`? Left out for now.
3. **Recording audio over MCP:** inline (capped at 2 MB, about 20 s of 48 kHz
   audio), or only a path? It could be downsampled to 16 kHz to fit about 60 s.
4. **Off-loopback writes:** require a token always (the plan), or allow them
   when no `auth_token` is set (today's REST rule)?
5. **Should `tuning.json` win over the TOML at start?** (Also open in
   live-tuning.md.)
6. **AIS first, or broadcast FM first?** AIS fits the marine use and the
   existing capture. WFM needs the sample-rate change.
7. **Second dongle:** is one available to run AIS full time, or should AIS share
   the scan dongle in timed windows like weather?
8. **Start phase 1 now?**
