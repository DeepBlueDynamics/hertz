# STATUS — Prior Sloth 🫖 (hertz-dsp port)

**Brief:** `plan/tasks/T2-sloth-dsp.md` — port gnosis-radio DSP into `crates/hertz-dsp` as a
pure, side-effect-free library with synthetic-IQ tests.

## Current state: IN PROGRESS

### Done
- Read brief, PLAN.md, gnosis-feature-inventory.md, all reference sources
  (`dsp.rs`, `pipeline/state.rs`, `pipeline/recorder.rs`, `pipeline/squelch.rs`,
  `pipeline/streamer.rs`, `wideband.rs`, `main.rs`).
- Confirmed crate stub exists at `crates/hertz-dsp/` (Cargo.toml + stub lib.rs);
  workspace root Cargo.toml already present.

### In progress
- Designing module layout and PipelineEvent decoupling (see plan below).

### Planned module map (brief item → module)
1. `pipeline/state.rs` + NFM → `pipeline.rs` (Pipeline, TransmissionState, events) +
   `demod/nfm.rs`
2. `dsp.rs` → `analysis.rs`
3. `wideband.rs` → `channelizer.rs` (generic, f32 end-to-end)
4. `pipeline/squelch.rs` → `classify.rs`
5. `pipeline/recorder.rs` → `recorder.rs` (path-configurable)
6. entropy pool → `entropy.rs`
7. NEW AM → `demod/am.rs`
8. Synthetic IQ → `testutil.rs` (pub, reused by tests + downstream)
9. Integration tests → `tests/{nfm,squelch,am,channelizer,entropy,recorder}.rs`

### Key design decisions
- **Decoupled side effects**: `Pipeline::process_buffer(&[Complex32]) -> Vec<PipelineEvent>`.
  No println!, no TcpStream, no inline file I/O. Events: `SquelchOpened`,
  `SquelchClosed`, `Audio`, `TransmissionComplete`, `MonitorTap` (always-on tap),
  `SignalLevel`. Recorder ships in-crate as a leaf utility (`write_recording`) but is
  NOT called from the pipeline — daemon/test wires TransmissionComplete → recorder.
- **f32 IQ end-to-end**: channelizer `extract_channel` returns `Vec<Complex32>` (no u8
  round-trip). `bytes_to_iq` is an edge helper for raw dongle u8 input only.
- **Generic channelizer**: `extract_channel`/`detect_active_channels` take
  `sample_rate_hz` + `decimation`/`channel_half_width_hz` params (not hardcoded to
  2.4 MHz/÷10).
- **Mode enum** selects NFM vs AM demod; pipeline dispatches via `DemodImpl` enum.
- **Dropped hertz-types dep** to keep crate buildable standalone (brief allows exactly
  num-complex, rustfft, hound, thiserror, serde).

### Constants carried (from gnosis-feature-inventory.md "Constants worth carrying")
SDR_RATE 240k, BUFFER_SIZE 48k, AUDIO 48k, PREBUFFER 1.5s, AFC 0.85/0.5Hz/5kHz,
squelch 6/12 dB, hang 10 frames, flatness open<0.55 noise>0.70, wideband 2.4M/8192/÷10,
chan FIR 81 taps @ 11kHz, FADE 480 samples (recorder) / 4800 (pipeline), DC α=0.001,
audio LPF 3kHz, peak-norm 0.7, noise-floor α 0.15/0.02/frozen.

### Blocked
- None yet.

### Deviations from gnosis (to be justified here)
- (none yet — will note e.g. adjacent-channel test offset interpretation)

## Verification (to fill at end)
- `cargo fmt`, `cargo clippy --all-targets -p hertz-dsp`, `cargo test -p hertz-dsp`
- grep for `println!`/`eprintln!`/`TcpStream`/`File` outside recorder.rs
