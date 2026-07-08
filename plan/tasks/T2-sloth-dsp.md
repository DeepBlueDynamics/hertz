# T2 — DSP library port (Prior Sloth 🫖)

Phase 2 of `plan/PLAN.md` §11: port the proven gnosis-radio signal chain into
`crates/hertz-dsp` as a pure, hardware-free library with tests. The original code is
vendored at `plan/reference/gnosis-radio/src/` — port it, don't reinvent it. The field-tuned
constants are sacred (see `plan/gnosis-feature-inventory.md`, "Constants worth carrying").

## Your paths
- `crates/hertz-dsp/**` only. (Haddock creates the workspace root + a stub for this crate
  in his first minutes; if the stub isn't there yet, create `crates/hertz-dsp/Cargo.toml`
  + `src/lib.rs` yourself and start — do NOT create or edit the workspace root Cargo.toml;
  until it exists you can work with `cargo check` from inside the crate dir if needed.)
- Depend only on: num-complex, rustfft, hound, thiserror (+ serde for a couple of config
  structs). NO tokio, NO USB, NO network — this crate must stay pure.

## What to port (source file → module)

1. `pipeline/state.rs` → `pipeline.rs` + `demod/nfm.rs`:
   - `NFMDemod`: 81-tap channel FIR @ 11 kHz cutoff, phase-continuous history, ÷5 polyphase
     decimation, in-channel power measured post-filter, AFC rotator, quadrature
     discriminator, DC-block (α=0.001), 3 kHz one-pole LPF, 0.7 peak normalize.
   - `Pipeline` / `TransmissionState`: entropy squelch (flatness EMA 0.85/0.15, open <0.55,
     noise >0.70), hang counter (10 frames, ×2 decrement on noise), voice gate
     (3 frames), fade-in 4800 samples squared ramp, fade-out over last 3 hang frames,
     1.5 s prebuffer ring drained on open, recording-stop-in-hang-tail logic,
     noise-floor adaptation (α 0.15 noise / 0.02 ambiguous / frozen on signal).
   - **Decouple side effects:** the gnosis pipeline printed, logged, streamed, and recorded
     inline. The port instead emits `PipelineEvent` values (SquelchOpened, SquelchClosed,
     Audio(Vec<f32>), TransmissionComplete(Vec<f32>) with metadata) returned from
     `process_buffer` — the daemon (someone else's task) wires those to bus/recorder/logs.
     No println!, no file I/O, no TcpStream in this crate (recorder WAV writing excepted,
     see 5).
2. `dsp.rs` → `analysis.rs`: SignalDetector (2048-pt FFT power spectrum, Hann),
   noise-floor estimate, peak + parabolic interpolation, SpectralSquelch (flatness),
   ImpulseNoiseFilter, ClickSuppressor, AudioNotchFilter, FrequencyEstimator (AFC),
   NoiseFloorTracker.
3. `wideband.rs` → `channelizer.rs`: `design_lowpass_fir` (windowed-sinc + Hann),
   `extract_channel` (phase-continuous mixer + polyphase decimating FIR, generic over
   decimation factor and rate — not hardcoded to 2.4 MHz/÷10), FFT activity detection
   (`detect_active_channels`) and median noise floor, parameterized by a channel-offset
   list. Keep f32 IQ end-to-end (u8→f32 conversion is a helper at the edge; do NOT
   round-trip through u8 between channelizer and pipeline like gnosis did).
4. `pipeline/squelch.rs` → `classify.rs`: SignalClassification + classify_signal.
5. `pipeline/recorder.rs` → `recorder.rs`: WAV 48 kHz/16-bit/mono writer with 10 ms fades
   and the gnosis filename scheme, path-configurable.
6. Entropy pool (`create_entropy_pool` + `harvest_entropy`) → `entropy.rs`.
7. **NEW — AM demod** (`demod/am.rs`): envelope |x|, DC-block, simple AGC, same channel-FIR
   front end; needed for CB + airband (PLAN §4). Same PipelineEvent interface, selected by
   `Mode`.

## Tests (this is most of the value)
Synthetic IQ generators in `tests/` or a `testutil` module:
- NFM: generate an FM-modulated 1 kHz tone at channel center, run demod, assert output is a
  clean ~1 kHz tone (autocorrelation or FFT peak), SNR sane; add 10 kHz-offset NFM signal on
  the adjacent channel and assert in-channel power barely moves (channel-filter test — this
  was gnosis's hard-won fix).
- Squelch: feed noise → stays closed; FM voice-like signal (mixed tones, low flatness) →
  opens within N frames, prebuffer content appears in first Audio event; back to noise →
  closes after hang; TransmissionComplete emitted once with expected sample count.
- AM: modulated tone demodulates; carrier-only classifies as Carrier.
- Channelizer: two simultaneous carriers at different channel offsets in a synthetic
  wideband buffer → detect_active_channels finds exactly those two; extract_channel on one
  yields a baseband tone at the right frequency.
- Entropy pool fills and drains.

## Done means
`cargo test -p hertz-dsp` green, no clippy warnings, no side-effectful I/O in the crate
(grep yourself for println!/TcpStream), status file `plan/tasks/STATUS-sloth.md` updated
per `plan/tasks/README.md`, including which constants you carried and any deviation from
the gnosis behavior with justification.
