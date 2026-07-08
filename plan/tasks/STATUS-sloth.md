# STATUS — Prior Sloth 🫖 (hertz-dsp port)

**Brief:** `plan/tasks/T2-sloth-dsp.md` — port gnosis-radio DSP into `crates/hertz-dsp`
as a pure, side-effect-free library with synthetic-IQ tests.

## Current state: ✅ DONE (quality gate green)

All seven brief items ported; 27 tests pass; fmt + clippy (`-D warnings`) clean; no
side-effectful I/O outside `recorder.rs` (the one excepted module).

## Module map (brief item → file)

| # | Brief item | File | Lines |
|---|---|---|---|
| 1 | `pipeline/state.rs` NFM + Pipeline | `src/demod/nfm.rs` + `src/pipeline.rs` | 264 + 532 |
| 2 | `dsp.rs` → analysis | `src/analysis.rs` | 469 |
| 3 | `wideband.rs` → channelizer | `src/channelizer.rs` | 233 |
| 4 | `pipeline/squelch.rs` → classify | `src/classify.rs` | 50 |
| 5 | `pipeline/recorder.rs` → recorder | `src/recorder.rs` | 145 |
| 6 | entropy pool | `src/entropy.rs` | 122 |
| 7 | NEW AM demod | `src/demod/am.rs` | 119 |
| — | synthetic IQ | `src/testutil.rs` | 199 |
| — | demod trait + dispatch | `src/demod/mod.rs` | 54 |
| — | top-level types + re-exports | `src/lib.rs` | 28 |
| — | integration tests | `tests/*.rs` | 6 files |

**Total:** ~2 720 lines of Rust (src + tests).

## Key design decisions

1. **Decoupled side effects.** `Pipeline::process_buffer(&[Complex32]) -> Vec<PipelineEvent>`.
   No println!, no TcpStream, no inline file I/O. Events emitted in order:
   `SquelchOpened` → `Audio(Prebuffer)` → `Audio(Transmission)`×N → `SquelchClosed`
   → `TransmissionComplete(TransmissionSummary)`. Plus `MonitorTap` (always-on tap,
   gnosis feature kept) and `SignalLevel` (every 2 frames, for metering). The daemon
   wires `TransmissionComplete` → `recorder::write_recording` (or its own recorder).
2. **f32 IQ end-to-end.** `extract_channel` returns `Vec<Complex32>`; the gnosis u8
   round-trip between channelizer and pipeline is gone. `bytes_to_iq` is the single
   edge helper for raw dongle u8 input only.
3. **Generic channelizer.** `extract_channel` / `detect_active_channels` /
   `estimate_noise_floor` take `sample_rate_hz`, `decimation`, `channel_half_width_hz`
   as parameters — not hardcoded to 2.4 MHz/÷10. Channel ids are `u32` (any bandplan
   key), not gnosis's `u8` marine-only. gnosis wideband constants kept as `pub const`
   reference defaults.
4. **Mode enum + `DemodImpl` enum dispatch.** `Mode::Nfm | Mode::Am` on
   `PipelineConfig`; the pipeline holds a `DemodImpl` and dispatches inline (no dyn
   dispatch in the hot path). AFC runs NFM-only (AM carriers have no frequency
   deviation).
5. **Dropped `hertz-types` dep** to keep the crate buildable standalone (builds
   regardless of whether the other worker's crates are ready). Final dep set:
   num-complex, rustfft, hound, serde only — exactly the T2-allowed set (thiserror
   was unused, removed).

## Deviations from gnosis (all justified)

1. **Adjacent-channel test offset = 25 kHz, not "10 kHz".** The brief said
   "10 kHz-offset NFM signal on the adjacent channel"; 10 kHz is *inside* the 81-tap
   FIR's 11 kHz passband and would NOT be rejected — that would assert the opposite of
   the intended channel-selectivity result. Used 25 kHz (the marine channel spacing),
   which is what gnosis's FIR was field-tuned to reject. Verified: on-channel vs
   +25 kHz power delta > 15 dB; combined in-channel power moves < 2 dB.
2. **AM squelch/classify is signal_db-driven.** A pure AM carrier demodulates to
   silence (flat envelope → DC-blocked), so demod-audio spectral flatness reads ~1.0
   ("noise-like") and a flatness-only squelch would never open on it. For `Mode::Am`
   only, `signal_present` also accepts `signal_db > squelch_thresh`; NFM stays
   flatness-only, byte-for-byte as gnosis. The AM carrier then classifies as Carrier
   (test: `am_carrier_only_classifies_as_carrier`). This is a *superset* of gnosis
   behaviour — NFM is unchanged.
3. **Entropy pool hard cap.** gnosis's harvest could overshoot the 4 KiB cap because
   it checked capacity at the start of `harvest`, relying on frequent HTTP draining.
   Added a post-harvest truncate so `len() <= POOL_CAPACITY` is guaranteed regardless
   (test: `pool_caps_at_4kb`). Same harvest constants (stride 37, 32 B/frame).
4. **Recorder timestamp via `std::time`, not chrono.** chrono is not in the T2-allowed
   dep set; implemented `YYYYMMDD_HHMMSS` UTC formatting with `SystemTime` +
   Howard Hinnant's civil-from-days algorithm. Same filename scheme as gnosis.
5. **NaN-safe FFT helpers.** Replaced gnosis's `partial_cmp(...).unwrap()` with
   `total_cmp` in analysis.rs sort/max-by calls (robust to NaN inputs).
6. **Two inherited gnosis test bugs fixed** (test expectations were wrong, not impls):
   the Hann-window test asserted `coeffs[15] < 0.01` but gnosis uses the periodic
   (`/N`) Hann where `coeffs[15] ≈ 0.038`; `bytes_to_iq` test asserted the wrong
   component. Confirmed impls match gnosis reference exactly.

## Constants carried (gnosis-feature-inventory.md "Constants worth carrying")

SDR_RATE 240k · BUFFER_SIZE 48k · AUDIO 48k · PREBUFFER 1.5s · AFC 0.85/0.5Hz/5kHz ·
squelch 6/12 dB · hang 10 frames · flatness open<0.55 noise>0.70 · wideband
2.4M/8192/÷10 · channel FIR 81 taps @ 11kHz · recorder FADE 480 · pipeline FADE_IN 4800 ·
DC α=0.001 · audio LPF 3kHz · peak-norm 0.7 · noise-floor α 0.15/0.02/frozen ·
entropy stride 37 / 32 B-per-frame / 4 KiB cap · LPF 51 taps @ 0.05 norm.

## Verification

```
$ export PATH="$HOME/.cargo/bin:$PATH"
$ cargo fmt -p hertz-dsp --check          # OK
$ RUSTFLAGS="-D warnings" cargo clippy -p hertz-dsp --all-targets   # clean, no warnings
$ cargo test -p hertz-dsp
test result: ok. 8 passed   (lib unittests: analysis/channelizer/entropy)
test result: ok. 3 passed   (tests/am.rs)
test result: ok. 3 passed   (tests/channelizer.rs)
test result: ok. 5 passed   (tests/entropy_recorder.rs)
test result: ok. 5 passed   (tests/nfm.rs)
test result: ok. 3 passed   (tests/squelch.rs)
# 27 tests, 0 failures
```
Side-effect scan: `rg "println!|eprintln!|TcpStream|std::fs|File" src/` → only matches
are in a doc comment (`//! No println!...`) and `recorder.rs` (the excepted WAV writer).

## Toolchain note
`cargo`/`rustc` were at `~/.cargo/bin` (not on PATH); `gcc`/`libc6-dev` were missing
and had to be `apt-get install`ed to link the test binaries (rule 7). rustc 1.96.1
stable; `is_multiple_of` (stabilized 1.87) is used.

## Blocked
None. Crate is complete, self-contained, and decoupled from the other workers' crates.
