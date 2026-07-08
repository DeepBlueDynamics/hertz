# T4 — Daemon core (Prior Sloth 🫖)

Phase 3 of `plan/PLAN.md` §11: `crates/hertz-daemon` — the `hertzd` binary that owns
dongles, DSP, storage, and the network surface. Scope is **monitor + channelized roles
only** (hopscan is Phase 5), **no MCP, no transcription engine** yet (Phases 6–7) — but
leave clean seams for both.

## Your paths
- `crates/hertz-daemon/**`
- `crates/hertz-types/src/wire.rs` (NEW module — you own it; nothing else in hertz-types)
- Workspace root `Cargo.toml` only for new `[workspace.dependencies]`
  (tokio, axum, tower-http, futures, tracing, tracing-subscriber, serde_yaml if needed).

## Step 1 — WIRE PROTOCOL FIRST (within your first hour)
Write `hertz-types/src/wire.rs` and get it compiling before anything else — the TUI worker
builds against it in parallel:
- `WsServerMsg` (tagged serde JSON): `Event(Event)` for every bus event except audio;
  plus `Hello { version, dongles: Vec<DongleSummary> }` on connect.
- Binary audio frame layout (document as consts + an encode/decode fn pair):
  `[u8 dongle_idx][u32 LE channel_key][u32 LE freq_hz][f32 LE signal_db][f32 LE pcm...]`
  (gnosis-compatible plus leading dongle byte). `encode_audio_frame` / `decode_audio_frame`
  with a round-trip unit test.
- REST DTOs: `StatusResponse`, `DongleSummary`, `TuneRequest { channel_id | freq_hz }`,
  `SquelchRequest`, `RecordingRequest`, `ActivityEntry`, `TranscriptEntry`,
  `RecordingFileEntry`, `DoctorReport` mirror types.

## Step 2 — Core runtime
- `DaemonConfig` (hertz-types) loaded from `--config` path or `HERTZ_CONFIG`
  (default `/etc/hertz/hertz.toml`, dev fallback `./hertz.toml`).
- **EventBus**: `tokio::sync::broadcast<Event>` (cap ~1024). DSP threads are sync — bridge
  with a `crossbeam-channel` → tokio task pump. Audio frames go on a separate broadcast
  channel (they're high-rate; don't let them evict control events).
- **Dongle workers**: for each `[[dongle]]`, spawn per role:
  - `monitor`: hertz-sdr worker at 240 kS/s on the configured channel/freq → one
    `hertz_dsp::Pipeline` → PipelineEvents mapped to `hertz_types::Event` (+dongle_id).
  - `channelized`: wideband capture at the bandplan group's center/rate →
    `detect_active_channels` per buffer → per-active-channel `extract_channel` + Pipeline
    slot map (create on detect, drop after 5 s silence, tap channel pinned) — this is the
    gnosis wideband loop restructured on hertz-dsp's event API. ChannelActivity heartbeat
    every ~25 frames even when idle.
  - Use `open_by_serial`; on `DeviceLost` emit `DongleStatus` and let the sdr layer's
    reconnect loop recover; pipelines reset on the worker's discontinuity epochs.
- **Recorder task**: subscribes to bus; on `TransmissionComplete` writes WAV via
  `hertz_dsp::recorder` under `data_dir/recordings/`, emits `RecordingSaved`, appends a
  line to `data_dir/logs/events.log`.
- **History**: ring buffers (500 activity / 200 transcripts / 50 tx) + JSONL append
  persistence under `data_dir/history/`, reloaded on boot.
- **Entropy**: wire the hertz-dsp entropy pool to the API.
- Transcription seam: `trait Transcriber` + a no-op `Disabled` impl behind config; on
  RecordingSaved call it (Phase 6 fills in real engines).

## Step 3 — Network surface (axum, single port, default 9080)
REST under `/api`: `GET /api/status`, `GET /api/dongles`, `GET /api/channels?group=`,
`POST /api/dongles/{id}/tune|squelch|recording|listen`, `GET /api/activity`,
`GET /api/transcriptions`, `GET /api/recordings` + `GET /api/recordings/{file}` (sanitized),
`GET /api/entropy?bytes=&format=`, `GET /api/time`, `GET /api/doctor`.
- `GET /stream` WS upgrade: JSON `WsServerMsg` text frames + binary audio frames; query
  filters `?events=...&audio=<dongle>:<channel|all|none>` (default audio=none).
- `GET /events` SSE: JSON events only.
- `GET /audio?dongle=&channel=` chunked `audio/L16;rate=48000;channels=1` (gnosis-compatible).
- Auth: `Authorization: Bearer <token>` middleware for non-loopback peers when
  `auth_token` configured. Loopback exempt.
- Port-conflict friendly error at bind time (gnosis nicety, keep it).

## Step 4 — Binary + shutdown
`src/main.rs` → `hertzd`: tracing to stdout, config summary banner, ctrl-c → graceful
shutdown (workers joined, history flushed).

## Step 5 — Tests (MockSdr end-to-end; no hardware, no sleeps longer than needed)
- Boot daemon from a test config with a `MockSdr` (via a test-only `SdrFactory` seam in
  the dongle manager — inject mock instead of real open) whose closure synthesizes an NFM
  voice-like burst; assert over WS: SquelchEvent(open) → audio frames → SquelchEvent(close)
  → RecordingSaved, and the WAV exists in tmp data_dir.
- REST: status/dongles/tune round-trip mutates worker state; auth: non-loopback without
  token → 401 (simulate via middleware unit test).
- wire.rs: audio frame encode/decode round-trip; every Event variant serializes/parses.

## Done means
`cargo test -p hertz-daemon -p hertz-types` green, clippy clean, `hertzd` runs on a mock
config and serves `/api/status`, STATUS-sloth.md updated (note any protocol decisions).
