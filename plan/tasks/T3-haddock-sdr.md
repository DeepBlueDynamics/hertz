# T3 — SDR device layer (Cognitive Haddock 🌀)

Phase 1 of `plan/PLAN.md` §11: `crates/hertz-sdr`. The device layer that everything sits on.
You will not have USB hardware in your container — design for testability with a mock, and
the supervisor verifies on real hardware from the host.

## Your paths
- `crates/hertz-sdr/**`
- Workspace root `Cargo.toml` ONLY to add `[workspace.dependencies]` entries you need
  (driver crate, `rtrb` for the SPSC ring, `crossbeam-channel`). Touch nothing else.

## Deliverables

### 1. Driver crate evaluation (do this first, note the outcome in STATUS)
Evaluate on docs.rs/crates.io: `librtlsdr-rs` (pure-Rust port, all tuner families, async)
vs `rtl-sdr-rs` (port of the RTL-SDR Blog librtlsdr fork, V4 support). Pick the one that
best covers: enumeration with serial strings, open by index, set sample rate / center freq /
manual+auto tuner gain / tuner bandwidth, reset buffer, blocking read. Record the decision
and API quirks in `plan/tasks/STATUS-haddock.md`. If both disappoint, say so and stop —
supervisor decides (do not fall back to a C-linked crate on your own).

### 2. `SdrDevice` trait + real impl
```rust
pub struct DongleInfo { pub index: u32, pub serial: String, pub product: String }
pub trait SdrDevice: Send {
    fn set_sample_rate(&mut self, hz: u32) -> Result<()>;
    fn set_center_freq(&mut self, hz: u32) -> Result<()>;
    fn set_gain(&mut self, gain: Gain) -> Result<()>;      // Gain::Auto | Gain::Db(f32)
    fn set_bandwidth(&mut self, hz: u32) -> Result<()>;
    fn reset_buffer(&mut self) -> Result<()>;
    fn read_sync(&mut self, buf: &mut [u8]) -> Result<usize>;
}
pub fn enumerate() -> Result<Vec<DongleInfo>>;
pub fn open_by_serial(serial: &str) -> Result<Box<dyn SdrDevice>>;
pub fn open_by_index(index: u32) -> Result<Box<dyn SdrDevice>>;
```
Error type with thiserror; distinguish NotFound / Busy / UsbError / Unsupported.

### 3. `MockSdr`
Implements `SdrDevice`; configurable to emit (a) synthetic IQ from a closure
(`Fn(u64 sample_index) -> (u8, u8)`), (b) playback of a raw IQ file, (c) injected errors.
This is what all downstream tests (daemon, integration) will use — make it ergonomic.

### 4. Reader thread + ring
`SdrWorker::spawn(device, config) -> WorkerHandle`:
- Dedicated OS thread; reads fixed-size buffers (default 48_000 IQ pairs) into an `rtrb`
  SPSC ring (capacity configurable, default ~2 s of samples).
- Consumer side: `WorkerHandle::reader() -> rtrb::Consumer<u8>` (or a chunk-oriented
  wrapper delivering `Vec<u8>` buffers — your call, justify in STATUS).
- Control: `WorkerHandle::retune(hz)`, `set_gain`, `shutdown()` via a command channel the
  reader drains between reads; retune must reset the device buffer and mark a stream
  discontinuity (sequence/epoch counter the DSP can see, so pipelines reset cleanly).
- Stats: dropped-sample counter (ring full), read-error counter, last-read timestamp,
  exposed via `WorkerHandle::stats()`.
- Device-loss handling: read errors N times in a row → emit `WorkerEvent::DeviceLost`
  (crossbeam channel) and park in a reconnect-by-serial retry loop (boats vibrate).

### 5. Doctor + FFT example
- `pub fn doctor() -> DoctorReport`: enumerate; for each device try open → configure
  240 kS/s → read 0.5 s → report ok/error, serial uniqueness warnings, approximate DC
  offset sanity. Pure function returning a struct (the TUI renders it later).
- `examples/dump_fft.rs`: `--serial|--index`, `--freq`, `--rate`; prints a 60-col ASCII
  spectrum every 500 ms (rustfft inline; don't depend on hertz-dsp). This is the Phase-1
  acceptance tool the supervisor runs against real hardware.

### 6. Tests
Mock-based: worker delivers exact bytes pushed by the mock; retune generates discontinuity
epoch; ring-full increments drop counter (tiny ring + slow consumer); device-lost event
fires on repeated errors; doctor report on a mock. Real-hardware tests: `#[ignore]`d,
named `hw_*`, so `cargo test -- --ignored` runs them on the host with a dongle.

## Done means
`cargo test -p hertz-sdr` green (mock tests), clippy/fmt clean, example compiles,
STATUS-haddock.md updated with the driver-crate decision and API notes.
