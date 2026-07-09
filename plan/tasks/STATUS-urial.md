# STATUS — Ytterbic Urial 🐉

Worker: daemon-side (GLM-5.2), supervised by Claude in pane "Strange Tiger 🥒".
Task: `plan/tasks/T4.1-daemon-spectrum-frames.md` — rate-limited FFT spectrum
frames on the WS wire. Prerequisite for the live (non-demo) waterfall in T6
(Crane's `hertz-tui` consumes these frames over `/stream`).

## T4.1 — Spectrum frames on the wire: ✅ DONE (quality gate green on working tree)

### What was built

**1. Wire codec — `hertz-types/src/wire.rs`** (extends the committed T4 module;
no existing types modified):
- New binary spectrum frame, layout per the brief:
  `[u8 0x02 magic][u8 dongle_idx][f64 LE center_hz][f64 LE span_hz][u8 squelch_open][u16 LE n][f32 LE bins...]`
  (21-byte header).
- `SPECTRUM_FRAME_MAGIC` (`0x02`) + offset/size consts, `encode_spectrum_frame`,
  `decode_spectrum_frame`, `DecodedSpectrumFrame`.
- `WireError::BadMagic` variant added (additive); the shared `ShortFrame`/`TrailingBytes`
  messages generalized so they read correctly for either frame type.
- **Disambiguation:** the leading magic `0x02` is the on-wire discriminator from
  audio frames (whose first byte is the dongle index, never a typed tag). A client
  on a connection that requested both streams decodes `buf[0] == 0x02` → spectrum,
  else audio. Documented in the codec.
- 4 new unit tests: round-trip (with bins), empty-bins round-trip, rejection
  (short / wrong magic / trailing bytes / bin-count mismatch), and an explicit
  magic-vs-audio discriminator test.

**2. Bus — `hertz-daemon/src/bus.rs`:** third `broadcast::Sender<SpectrumFrame>`
(cap 64) alongside events + audio, with `subscribe_spectrum` / `publish_spectrum`.
`spawn_pumps` now returns an `(event, audio, spectrum)` unbounded-sender triple
(sync-safe `send`, same bridge model as audio).

**3. DSP — `hertz-daemon/src/spectrum.rs` (new) + `pipeline_bridge.rs`:**
- `SpectrumFrame` bus payload (`dongle_id/idx, center_hz, span_hz, squelch_open,
  bins_db`) + `encode()` via the wire codec.
- `SpectrumEmitter`: per-dongle rate limiter enforcing **≤20 fps** (`MAX_FPS = 20`,
  50 ms min interval) regardless of DSP frame rate. First call always emits.
- `compute_spectrum_db`: 1024-pt forward FFT (cached `rustfft` plan reused across
  frames) → `10·log10(|bin|² + ε)` (matches the `detect_active_channels`
  convention) → **fft-shifted** so bin 0 = `center - span/2`, last bin =
  `center + span/2` (matches `tui-waterfall-spec.md` §3).
- Monitor loop: emits `center = tuned freq`, `span = SDR_RATE` (240 kS/s),
  `squelch_open = pipeline.is_squelch_open()`, on each 200 ms IQ frame.
- Channelized loop: emits `center = wideband center`, `span = sample_rate`
  (2.4 MS/s), `squelch_open = any slot open`, on each wideband frame.
- `DspThread` gained a `spectrum_tx` field, wired in `runtime.rs`.
- 3 unit tests: rate-limit, pure-tone peak lands at the correct **fft-shifted**
  bin, and frame encode→decode round-trip.

**4. WS `/stream` — `hertz-daemon/src/server/mod.rs`:** opt-in `?spectrum=<dongle|all>`
query param (default `none` — the ~40–80 KB/s stream only flows to clients that
ask). `SpectrumSel` + `spectrum_accepts`; `run_ws` subscribes to the spectrum
broadcast and forwards accepted frames as binary in a third `select!` arm. A
connection that doesn't request spectrum filters it server-side and never sees a
`0x02` frame.

**5. Test coverage — `hertz-daemon/tests/mock_e2e.rs`:** new
`spectrum_frames_arrive_only_when_requested` boots a mock monitor dongle and,
over **two real WS connections**:
- Conn A `?spectrum=all`: receives ≥3 binary frames that all decode as spectrum
  (`0x02`, non-empty bins, `dongle_idx == 0`, `center_hz ≈ 156.8 MHz`,
  `span_hz ≈ 240 kS/s`), and **no** non-spectrum binary frames.
- Conn B `?audio=all`: receives audio frames and **zero** spectrum frames
  (proves the opt-in filter is server-side). Passes in ~1.5 s alongside the
  existing lifecycle test.

### Verification (on this working tree)
```
$ cargo fmt  -p hertz-types -p hertz-daemon --check        # CLEAN
$ cargo clippy -p hertz-types -p hertz-daemon --all-targets # 0 warnings
$ cargo test   -p hertz-types -p hertz-daemon
   hertz-types:           10 passed  (was 6; +4 spectrum wire tests)
   hertz-daemon lib:       3 passed  (new spectrum module)
   hertz-daemon mock_e2e:  2 passed  (was 1; +1 spectrum test)
   hertz-daemon rest:      2 passed  (unchanged)
$ ./target/debug/hertzd --mock /tmp/hertz-mock-smoke.toml  # boots, no panic
$ curl 127.0.0.1:19080/api/status                          # 200 + dongle roster
```

### Design decisions / deviations
1. **Channelized FFT not literally reused.** The brief suggests reusing the
   channelized detector's FFT where it already runs; that FFT is internal to
   `hertz_dsp::channelizer::detect_active_channels` (out of this task's paths),
   and it's 8192-pt/leakage-shaped for detection, not display. So both roles run
   a separate cached 1024-pt FFT for the spectrum (the brief's explicit fallback
   for the monitor role). Cost is modest (1024-pt at ≤20 fps).
2. **fftshift applied** so bin ordering matches the waterfall spec's
   `bin 0 = center - span/2`. `10·log10(|bin|²+ε)` dB convention matches the
   existing channelizer (not `20·log10`); the TUI maps dB→color via its own
   floor/ceil so the absolute scale is cosmetic.
3. **No scratch-buffer optimization** in `compute_spectrum_db` — `rustfft`'s
   `process` allocates a scratch internally. At ≤20 fps this is negligible and
   it avoids the `process_with_scratch` length-contract pitfall. The expensive
   part (the plan) is still cached on the emitter.

### ⚠️ Note for the supervisor (git state)
`0bc56a5` ("Fix channelizer panic… + deployment fixes") snapshotted my work
**mid-debug** bundled with Crane's T6 and others. That commit's
`crates/hertz-daemon/src/spectrum.rs` **does not compile** — it has the
intermediate `db.rotate(n / 2)` (no such method on `Vec<f32>`; `rotate_left` is
required) and a `process_with_scratch` path that panics with "Not enough scratch
space". My **uncommitted working-tree changes** are the fixes that make the
daemon build and pass, plus the WS `?spectrum=` forwarding in `server/mod.rs`
(only the `WsQuery` field made it into the commit) and the new `mock_e2e`
spectrum test. Files left uncommitted for your review:
- `crates/hertz-daemon/src/spectrum.rs` — compile + test fixes (rotate_left,
  scratch removal, `mut planner`, corrected tone test, fmt).
- `crates/hertz-daemon/src/server/mod.rs` — full `?spectrum=` opt-in: `SpectrumSel`,
  `parse_filter`, `spectrum_accepts`, the `run_ws` `select!` arm.
- `crates/hertz-daemon/tests/mock_e2e.rs` — `spectrum_frames_arrive_only_when_requested`.
The committed files (`wire.rs`, `bus.rs`, `lib.rs`, `pipeline_bridge.rs`,
`runtime.rs`) already match my working tree exactly (no diff).

### Path discipline
Touched only: `crates/hertz-daemon/**` (`spectrum.rs` new; `bus.rs`,
`lib.rs`, `pipeline_bridge.rs`, `runtime.rs`, `server/mod.rs`, `tests/mock_e2e.rs`)
and `crates/hertz-types/src/wire.rs`. No git mutations performed (no
commit/push/checkout/stash — per the rules). Did not touch `hertz-dsp`,
`hertz-sdr`, `hertz-channels`, `hertz-tui`, any `plan/` brief, or `.claude/`.

The working-tree diffs on `README.md` and `plan/breif.md.txt` are **not mine**
(whitespace / CRLF-only churn; I never opened them — `plan/` is off-limits and
`cargo fmt` was scoped to `-p hertz-types -p hertz-daemon`).

### Blocked / deferred
- None for T4.1. Downstream T6 (Crane) already decodes these frames; the wire
  format matches the brief exactly.
