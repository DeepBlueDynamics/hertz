# Live tuning, MCP control and auto-tune for hertzd

Goal: the agent tunes the receiver while hertzd runs (squelch, scan list,
lockouts, gain, auto-tune), so nothing needs a rebuild or restart. Kord asked for
this on 2026-10-15 after a day of restart-per-fix on FRS/GMRS.

## Today (why every change needed a restart)

- **Scanner ignores live changes.** `spawn_scan_loop` (pipeline_bridge.rs) builds
  its per-channel pipelines once and never reads `squelch_epoch` or `freq_epoch`.
  `POST /api/dongles/:id/squelch` changes the number `/api/status` shows and
  nothing else; `POST /tune` does nothing while scanning. `/recording` and
  `/listen` do work live.
- **Scanner state is thread-local and fixed.** The channel list, pipelines,
  lockouts and the scan constants (`SCAN_LOCKOUT` 300 s, `SCAN_MAX_OPEN` 30 s,
  `SCAN_RESUME_AFTER` 5 s, `NEIGHBOUR_HZ` 15 kHz, `NEIGHBOUR_MARGIN_DB` 3) are
  hard-coded or set at start.
- **No live setters on a Pipeline.** `squelch_margin` and `noise_floor` are
  private; rebuilding a pipeline costs 5 visits of calibration per channel (about
  27 s deaf on a 22-channel scan). A setter plus `update_thresh()` is enough.
- **Tuner gain is Auto**, with no config field. A carrier pulls the gain down and
  the band reads low until it recovers (the -45 to -49 dB closes). Absolute
  thresholds need a fixed gain. `WorkerHandle::set_gain` already works live.
- **No MCP server**, no config reload, no per-channel level summary outside the
  Lume feed. History timestamps are f32 (128 s resolution at today's epoch).
- **Auth:** `check_auth` lets everything through when no `auth_token` is set,
  even off loopback.

## Design

### Transport: `/mcp` on the existing axum port

Hand-rolled stateless streamable-HTTP JSON-RPC: `initialize`,
`notifications/initialized`, `ping`, `tools/list`, `tools/call`; JSON responses,
no SSE. rmcp's server needs axum 0.8 (the workspace is on 0.7); about 250 lines
of `serde_json::json!` instead, matching the hand-rolled Hyperia client. Register
with `claude mcp add --transport http hertz http://127.0.0.1:9080/mcp`.

### Shared state: `ScanControl` beside `DongleControl`

New `crates/hertz-daemon/src/tuning/`:
- `ScanSettings` (versioned with an epoch): scan entries, per-channel
  `ChannelTuning { margin_db, open_db }`, and the five scan durations and margins.
- `ScanCmd` channel for one-shot actions: `LockOut`, `Unlock`, `Hold` (a channel
  or raw frequency, until a time), `Resume`.
- `ScanTelemetry` written by the scan thread with `try_lock` (never blocks DSP):
  mode, current channel, open state, and per channel the noise floor, threshold,
  lock, and a ring of per-frame `LevelSample`s (signal, floor, flatness, open,
  voice).

The scan thread checks the epochs and drains commands each frame. Pipelines are
kept by channel id, so learned floors survive scan-list edits; list changes apply
only between hops, never mid-call. It honours `freq_epoch` (fixing the tune bug)
and emits `ScanState` (exists, never sent).

### Pipeline (hertz-dsp)

`set_squelch_margin`, `set_open_floor(Option<f32>)` (an absolute minimum
threshold), `set_noise_floor` (re-seed), `squelch_threshold_db`, `last_frame()`.
Threshold = `max(floor + margin, open_db)`: the adaptive floor still follows
drift, and `open_db` stops opens on the noise tail. Defaults unchanged.

### Tools (each with a REST mirror)

| Tool | Does |
|---|---|
| `hertz_status` | Dongles, gain, mode, current channel, scan list, margins, per-channel floor/threshold/lock, autotune job |
| `hertz_levels` | Per channel over the last N s: p50/p95/p99/max, stddev, floor, threshold, fraction open, recent opens with classification |
| `hertz_set_squelch` | Default or per-channel margin, absolute `open_db`, clear |
| `hertz_scan` | Get/set/add/remove entries; set lockout, max-open, resume and neighbour values |
| `hertz_lockout` / `hertz_unlock` | Lock a channel for N minutes or permanently; unlock one or all |
| `hertz_tune` | Hold a channel or frequency for N s, or resume the scan |
| `hertz_set_gain` | Manual gain in dB, or auto |
| `hertz_autotune` | Start/status/cancel; passive or dwell; returns a report |
| `hertz_recent` | Recent opens joined with transcripts and recordings |

### Auto-tune

Pure `analyze(samples, voice_peaks, params)`:
- Noise = non-voice frames; needs at least 20 samples per channel.
- `open_db = noise p95 + guard (3 dB)`, capped 2 dB under the weakest recent
  voice peak, with a warning when headroom is under 2 dB.
- Re-seeds the floor to p50.
- A carrier is flagged when: median at least 6 dB over the band median with
  stddev under 1.5 dB, or open at least half the time without voice, or a CARRIER
  lockout in the window. Flagged carriers are locked out (30 min by default)
  instead of having their threshold raised over them.

Modes:
- **Passive (default):** the scan keeps running, about 4 min of samples, then
  analyse and apply. No calls missed.
- **Dwell (opt-in, fast):** hold each channel for 8 s. Only starts when idle;
  aborts and resumes on any voice open. About 3 min away from the scan for 22
  channels.

Under Auto gain it tunes margins only, or asks for `hertz_set_gain` first.

### Persistence: `<data_dir>/tuning.json`

Gain, default squelch, scan list, scanner values, per-channel tuning (with source
and time) and long lockouts. Loaded at start and overriding the TOML; `open_db`
values are dropped if the gain changed. Debounced (2 s) write via temp file and
rename. `DELETE /api/tuning` resets to the config.

### Safety

- `/mcp`: loopback always; off loopback needs a configured token and a matching
  bearer (403 otherwise, stricter than `check_auth`); a non-localhost `Origin` gets
  403 (DNS rebinding).
- hertzd refuses to bind `0.0.0.0` or `::` (project rule: never listen on all
  interfaces).
- Every request validated (ranges, known channel ids); one autotune job per dongle.
- Every change logged and published as `Event::Tuning { actor, action, channel,
  before, after, reason }`, including the scanner's own lockouts. Lume records it
  as `radio.tuning`.

## Phases

| Phase | Work | Gets Kord |
|---|---|---|
| **0** (about an hour) | `Pipeline::set_squelch_margin` + `update_thresh`; the scan loop applies `squelch_epoch` to every pipeline | The existing squelch REST call works live |
| **1** | `ScanControl`; scan-loop refactor (live list, lockout table, configurable values); level rings; `Event::Tuning` to Lume; `/mcp` with status, levels, set_squelch, scan, lockout, unlock; REST mirrors; MCP auth and the 0.0.0.0 refusal; a minimal `tuning.json` for squelch and lockouts | The agent watches and fixes live, and it survives restarts |
| **2** | `hertz_set_gain`, `hertz_tune` with hold (fixes tune), `hertz_recent` (f64 timestamps), passive and dwell autotune | Auto-tune |
| **3** | Full `tuning.json` with reset | Everything survives restarts |
| **4** | Periodic passive autotune; per-channel margins in the channelized role | Polish |

## Tests

- **Unit:**
  - the pipeline setters keep the floor (no recalibration);
  - with `open_db` -10, noise at -14.5 ± 1.5 never opens and a -7.9 dB burst does;
  - `analyze` handles noise, a steady carrier, the voice-peak cap and too few samples;
  - scan-list rebuild keeps floors;
  - persistence round trip and gain mismatch;
  - MCP protocol (errors, batches, initialize, `tools/list`, bad arguments);
  - MCP auth, including Origin.
- **Mock integration** (`tests/mcp.rs`, 127.0.0.1 port 0): initialize, set_squelch
  live, lockout and unlock events, scan add/remove, hold, dwell autotune with a
  voice abort, values back after restart. Needs a noisy `MockFactory`.

## Risks

- Auto gain makes absolute thresholds unreliable.
- Little headroom: noise p95 + 3 dB against -7.9 dB calls is about 2 dB, so weak
  calls can be lost (the report shows headroom).
- DSP thread must never block on telemetry (`try_lock`, poison-tolerant).
- Scan-list changes must not drop an open call's recording.
- Neighbour detection with per-channel thresholds needs a test.

## Decided (Kord, 2026-10-15)

- **Gain:** fixed manual gain, saved in `tuning.json`; auto-tune sets absolute
  per-channel levels.
- **Auto-tune applies right away**, logs and reports every change.
- **Carriers:** locked out for 30 min, then rechecked.
- **Build phases 0 and 1 now.**

## Open questions for Kord

1. Fix the tuner gain (for example 30-40 dB, saved in `tuning.json`)? Under Auto,
   should auto-tune refuse absolute thresholds or tune margins only?
2. Should auto-tune apply its changes right away, or report a dry run first?
3. Carrier channels: lock out for 30 min, permanently, or raise their threshold?
4. Passive auto-tune (about 4 min, no scan pause) as the default, dwell opt-in?
5. On `0.0.0.0`: hard-fail at start, or only disable `/mcp` with a warning?
6. When `tuning.json` and the TOML disagree, does the file always win?
7. Is loopback enough for the hertzd MCP for now, or should it join a Hyperia
   identity or allowlist?
