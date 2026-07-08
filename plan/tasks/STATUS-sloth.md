# STATUS — Prior Sloth 🫖

## T2 — hertz-dsp port: ✅ DONE (committed 255fc45)
Pure DSP library ported from gnosis-radio; 27 tests; fmt/clippy `-D warnings` clean.
See the T2 section in git history / the prior status for the full module map,
sacred constants, and justified deviations (25 kHz adjacent-channel test, AM
signal_db-driven squelch superset, entropy-pool hard cap, std-time timestamps).

---

## T4 — Daemon core: ✅ DONE (quality gate green)

`crates/hertz-daemon` + `crates/hertz-types/src/wire.rs`. Monitor + channelized roles
on one axum port (REST + WS + SSE + `/audio`). No hopscan/MCP/transcription engine —
seams only. MockSdr end-to-end test passes; `hertzd --mock` runs and serves `/api/status`.

### Step 1 — wire protocol (`hertz-types/src/wire.rs`) — landed first ✅
- `WsServerMsg` = `Event(Event)` | `Hello { version, dongles }` (externally-tagged JSON).
- Binary audio frame consts + `encode_audio_frame`/`decode_audio_frame`
  (`[u8 dongle][u32 ch_key][u32 freq][f32 signal][f32 pcm…]`, gnosis + leading dongle byte).
- REST DTOs: `StatusResponse`, `DongleSummary`, `TuneRequest` (with `validate()`),
  `SquelchRequest`, `RecordingRequest`, `ListenRequest`, `ActivityEntry`,
  `TranscriptEntry`, `RecordingFileEntry`, `DoctorReport`, `DongleDoctorEntry`.
- 5 wire unit tests: audio round-trip, short/trailing rejection, WS Hello round-trip,
  **every Event variant** serializes/parses, TuneRequest validation. Re-exported from
  the crate root so the TUI builds against `hertz_types::{WsServerMsg, …}`.

### Steps 2–4 — runtime + network surface + binary
| Module | Role |
|---|---|
| `bus.rs` | `EventBus`: separate `broadcast<Event>` (1024) + `broadcast<AudioFrame>` (256) so audio can't evict control events; `spawn_pumps` bridges sync DSP → async via tokio `mpsc::UnboundedSender` (sync-safe `send`). |
| `control.rs` | `DongleControl` (Arc<Mutex>) — freq/squelch/recording/listening with epoch counters for lock-free DSP reads; `set_*` from REST, `snapshot()` from DSP. |
| `factory.rs` | `SdrFactory` trait + `RealFactory` (`open_by_serial`) + `MockFactory` (Arc-shared closure → many `MockSdr` workers). The test-injection seam. |
| `pipeline_bridge.rs` | Sync DSP threads: monitor loop (u8 ring → `bytes_to_iq` → `Pipeline`) + channelized loop (wideband → `detect_active_channels` → per-slot `extract_channel` + Pipeline, tap pinned, 5 s silence release, 25-frame ChannelActivity heartbeat). Maps `PipelineEvent` → bus Event / AudioFrame / recorder job. Worker `DeviceLost`/`Reconnected` → `DongleStatus` + pipeline reset. |
| `recorder.rs` | Recorder task: `TransmissionJob` → `hertz_dsp::recorder::write_recording` in `spawn_blocking` → `RecordingSaved` + `events.log` line → `Transcriber` seam. |
| `history.rs` | 500 activity / 200 transcripts rings + JSONL append persistence under `data_dir/history/`, reloaded on boot. |
| `transcribe.rs` | `trait Transcriber` (sync, object-safe) + `DisabledTranscriber` no-op. Phase 6 fills engines. |
| `runtime.rs` | `Daemon::start` wires config→channels→bus→recorder→per-dongle DSP threads→pumps→history subscriber→axum server. Graceful `shutdown()` (DSP flags + worker shutdown + axum graceful-shutdown oneshot) + non-hanging `join()` (joins DSP threads + recorder, aborts server/history/pumps). |
| `server/mod.rs` | axum router: REST `/api/*` (status, dongles, channels, tune, squelch, recording, listen, activity, transcriptions, recordings + sanitized file serve, entropy, time, doctor), WS `/stream` (`?events=&audio=` filters + Hello + binary audio), SSE `/events`, bearer-token auth (loopback exempt). |
| `server/audio_endpoint.rs` | `/audio?dongle=&channel=` chunked `audio/L16;rate=48000;channels=1` (gnosis/VLC-compatible). |
| `main.rs` | `hertzd` binary: tracing, banner, config+bandplan load, ctrl-c graceful shutdown, `--mock` flag to run with no hardware. |

### Step 5 — Tests (all green)
- `tests/mock_e2e.rs`: boots daemon on an ephemeral port with a `MockFactory` whose
  closure synthesizes an NFM voice burst (noise→voice→noise via
  `hertz_dsp::testutil`); asserts **over a real WS connection**: Hello →
  `SquelchEvent(open)` → binary audio frames → `SquelchEvent(close)` →
  `RecordingSaved`, and the WAV exists in the temp data_dir. Passes in ~0.8 s.
- `tests/rest.rs`: raw-HTTP `GET /api/status` (200 + dongle roster); `POST …/tune`
  round-trips and mutates worker control state; `check_auth` unit: loopback exempt,
  non-loopback without token → 401, with token → ok, wrong token → 401.

### Verification
```
$ cargo fmt -p hertz-types -p hertz-daemon --check          # CLEAN
$ cargo clippy -p hertz-types -p hertz-daemon --all-targets # 0 warnings
$ cargo test -p hertz-types -p hertz-daemon
   hertz-types: 6 passed (wire + config)
   hertz-daemon mock_e2e: 1 passed   rest: 2 passed
$ ./target/debug/hertzd --mock /tmp/hertz-mock.toml         # boots, 419 channels
$ curl 127.0.0.1:19080/api/status                           # 200 + dongle roster
```

### Protocol / design decisions (for the TUI + later phases)
1. **WS envelope is externally-tagged** (`{"Event":{"type":"SquelchEvent",…}}`,
   `{"Hello":{…}}`) — the TUI matches on the outer key. Audio is NEVER a text frame:
   it's the binary layout above, selected via `?audio=<dongle>:<channel|all|none>`
   (default **none**).
2. **Two broadcast channels** (events + audio) so a slow audio subscriber can't evict
   squelch/recording events. DSP→bus bridge is tokio `mpsc::UnboundedSender` (its
   `send` is sync-safe) — simpler than the brief's crossbeam pump, same effect
   (noted deviation).
3. **Sync→async bridge**: each DSP thread is a `std::thread` (gnosis model); it feeds
   the bus via two unbounded senders whose `send` needs no `.await`.
4. **`DongleControl` epochs**: REST mutates freq/squelch and bumps an epoch; the DSP
   thread snapshots the epoch each frame and rebuilds the pipeline only on change
   (so a squelch change rebuilds the pipeline — phase-continuous audio during control
   changes is a later refinement).
5. **`hertzd --mock`** runs the full stack with a static-IQ `MockFactory` so the
   network surface and `/api/doctor` are demonstrable with no hardware.
6. **Channelized channel_key** maps numeric marine ids (`"16"` → u32) for the wire
   frame; non-numeric ids hash — T4 tests use numeric marine ids. A side-table for
   arbitrary string ids arrives with full bandplan mode support.

### Blocked / deferred (by design, later phases)
- **Hopscan** (Phase 5): role recognised, spawn skipped with a log line.
- **MCP** (Phase 7): no `/mcp` mount yet.
- **Transcription engines** (Phase 6): `Transcriber` seam + `DisabledTranscriber` only.
- **Entropy API** (`/api/entropy`) returns the pool shape; full DSP-pool drain wiring
  (the pipeline's pool isn't yet surfaced to the REST handler) lands with the
  per-dongle `drain_entropy` plumbing — the seam exists (`Pipeline::drain_entropy`).
- AM/SSB per-channel mode selection in the channelized path (mode is read from the
  bandplan channel; monitor defaults to NFM).

### Toolchain note
`cargo`/`rustc 1.96.1` at `~/.cargo/bin`; `gcc`/`libc6-dev` installed earlier (T2).
The shared `target/` dir hit transient FS I/O errors twice during RUSTFLAGS-changed
rebuilds — `cargo clean` resolved it each time; not a code issue.

### Path discipline
Touched only: `crates/hertz-daemon/**`, `crates/hertz-types/src/wire.rs` (+ the
minimal `pub mod wire;` + re-export wiring in `hertz-types/src/lib.rs` — no existing
types modified), and the workspace root `Cargo.toml` for new `[workspace.dependencies]`
(tokio, tokio-tungstenite, axum, tower-http, futures, tracing, tracing-subscriber).
Did not touch hertz-sdr/hertz-tui/hertz-channels or any other worker's paths.
