# Gnosis-Radio Feature Inventory (scan of ../gnosis-radio, 2026-07-08)

This is the parity checklist for the Hertz rewrite. Every feature here must exist in Hertz
(sometimes in adapted form — noted per item). Source: full read of `gnosis-radio/src/*` at
~7,270 lines of Rust.

## Identity

- Package `gnosis-radio` 0.1.0, brands itself "Meridian Radio" at runtime.
- Windows-native build: old C-binding crate `rtlsdr = "0.1"`, links a checked-in `rtlsdr.lib`
  via `build.rs` link-search. This is the single biggest thing the rewrite replaces.

## Modes (CLI, clap)

### `monitor` — single channel/frequency
- `--channel <u8>` (marine channel) or `--frequency <Hz>`; `--device <index>` (default 0).
- `--squelch` dB above noise floor (default **6.0**), `--log` (default `vhf_monitor.log`),
  `--record`, `--listen` (default true), `--viz` (ASCII), `--tui` (stub — prints "not yet
  integrated"), `--stream host:port` (newline-delimited JSON audio frames over TCP).
- SDR config: 240 kS/s, tuner gain manual 49.6 dB, tuner bandwidth 150 kHz, direct sampling off.
- Startup calibration: 10 buffers averaged for initial noise floor (fallback −60 dB).
- Runtime control loop reacts to frequency/recording/listen/squelch changes from the HTTP API.

### `scan` — wideband simultaneous multi-channel
- 2.4 MS/s centered at **156.7375 MHz** (whole US marine band in one capture).
- FFT detection: 8192-point, per-channel power integration over ±12.5 kHz, squelch margin
  default **12.0 dB** over a median-of-FFT-bins noise floor (EMA 0.95/0.05, updated every
  50 frames when idle).
- Per-channel extraction (DDC): complex mixer (phase-continuous across frames) + 51-tap
  windowed-sinc LPF (cutoff 120 kHz norm) + decimate ÷10 → 240 kS/s, re-encoded to u8 IQ and
  fed to an independent `Pipeline` instance per active channel.
- Channel slots auto-created on detection, auto-released after **5 s** silence
  (`AUTO_RESUME_SCAN_SEC`), with squelch-close broadcast on timeout.
- **Always-on tap channel**: one channel (user-selected via API, default ch 16) keeps a
  pipeline alive permanently and streams demodulated audio continuously (static included)
  so UIs always have a live waterfall. `--exclude` channel list supported.

## DSP pipeline (`pipeline/state.rs` — the crown jewels)

- IQ u8 → f32 conversion ((x−127.5)/127.5).
- **NFM demod**: 81-tap channel-select FIR, 11 kHz cutoff at 240 kS/s, polyphase-decimated ÷5
  → 48 kHz; filter history preserved across buffers (phase-continuous); in-channel power (dB)
  measured post-filter so adjacent channels can't move the squelch meter; AFC rotator applied
  pre-discriminator; quadrature discriminator `arg(x·conj(prev))`; DC-block IIR (α=0.001);
  3 kHz one-pole audio LPF; peak-normalize to 0.7.
- **Entropy squelch**: spectral flatness of demod audio in 300–3400 Hz (2048-pt FFT bins),
  EMA 0.85/0.15. Open when flatness < 0.55; "definitely noise" > 0.70. Hang counter
  10 frames (~2 s), decrements ×2 on noise, resets on structured audio. 3-frame voice gate,
  recording stops during hang tail, squared fade-in (~100 ms / 4800 samples), fade-out over
  last 3 hang frames.
- **Noise floor adaptation** driven by flatness: aggressive (α 0.15) on noise, slow (0.02)
  when ambiguous-below-threshold, frozen when signal present.
- **AFC**: FrequencyEstimator on raw IQ, offset clamped ±5 kHz, EMA smoothing 0.85, applied
  when |err| > 0.5 Hz, released below 0.25 Hz.
- **Prebuffer**: 1.5 s ring of demod audio while squelch closed; drained (with fade-in) into
  recording + broadcast at squelch open — captures the syllable before the trigger.
- Signal classification: Static / Carrier / Voice (flatness + harmonic-bin heuristics; richer
  spectral path when viz enabled).
- Support DSP (`dsp.rs`): Hann/Blackman windows, 2048-pt FFT power spectrum,
  parabolic peak interpolation, `NoiseFloorTracker`, `SpectralSquelch` (flatness),
  `ImpulseNoiseFilter`, `AudioNotchFilter`, `ClickSuppressor`, `FrequencyEstimator`.
- **Entropy pool**: LSBs of demod audio harvested every frame (every 37th sample, 32 B/frame)
  into a 4 KB pool drained over HTTP (RF-noise TRNG).

## Recording (`pipeline/recorder.rs`)

- WAV 48 kHz / 16-bit / mono, 10 ms fade-in/out, per-transmission files:
  `recordings/transmission_<UTC ts>_Ch<NN>_<LABEL>_<MHz>MHz_<n>.wav`.
- Squelch open/close + recording-saved lines appended to a log file.

## Transcription & AI (`transcribe.rs`, `control.rs`, `voicepaint.rs`)

- On recording save: WAV POSTed (multipart) to external Whisper service at
  `http://localhost:8765` (`/transcribe`, poll `/status/{job}` every 2 s, max 4 min,
  `/download/{job}`), model `large-v3`; result broadcast as `Transcription` message.
- `POST /api/clean-transcript`: Claude Haiku cleans raw Whisper output into marine shorthand
  (V/L, STN, CH, SEC, PAN, INBD/OUTBD, NM, HDG, ETA). Uses `ANTHROPIC_API_KEY`.
- **Voicepaint**: `POST /api/voice-paint` sends latest spectrogram screenshot PNG to Claude
  vision (Sonnet) → JSON paint regions (formants, sibilance, noise bands) broadcast to clients.
  Screenshot is uploaded by the web UI (`POST /api/screenshot`). *Hertz adaptation: daemon
  renders the spectrogram PNG itself; no browser needed.*

## Event bus (`broadcast.rs`, `agentic.rs`)

- `AudioBroadcaster`: multi-subscriber fan-out, bounded(64) per subscriber, drop-on-full,
  auto-prune on disconnect.
- Message types: `Audio` (f32 samples + channel/freq/signal_db), `ChannelActivity`
  (periodic snapshot + noise floor, heartbeats even when idle), `SquelchEvent`
  (open/close + classification), `SignalLevel` (every ~2 frames: signal/noise/squelch/flatness),
  `Transcription`, `VoicePaint`.
- `AgenticState` accumulator: last 200 activity entries, last 50 transcripts, live signal
  state, screenshot, voice painting, window layout — served by the HTTP API for agents.

## Network APIs

### HTTP control (tiny_http, port 9080, CORS *)
- `GET /` embedded web UI (RADIO_HTML — **dropped in Hertz**, TUI-only), `GET /test` diag page.
- `GET /status`, `POST /channel` (channel or frequency_hz), `POST /recording`,
  `POST /listen`, `POST /squelch` (clamped 0–30 dB).
- `GET /api/status` (full agentic state), `/api/activity`, `/api/transcriptions`,
  `/api/recordings` (list, newest 100) + `/api/recordings/<file>` (serve WAV, sanitized),
  `/api/extracts` (list/upload/serve WAV|PNG|stft.gz),
  `GET|POST /api/screenshot`, `GET|POST /api/voice-paint`, `POST /api/clean-transcript`,
  `POST /api/errors` (frontend error log), `GET /api/time`,
  `GET /api/entropy?bytes=&format=hex|raw|json`, `GET /api/entropy/stream` (SSE, 1 Hz),
  `GET|POST /api/layout` + `/api/layout/reset` (**layout is web-UI-specific — drop**),
  `GET /audio` — chunked HTTP raw PCM stream (L16 big-endian, 48 kHz mono).
- Port-conflict pre-check with friendly netstat/taskkill hints.

### WebSocket (tungstenite, port 9081)
- Binary frames: `[u32 channel][u32 freq][f32 signal_db][f32 PCM...]` little-endian.
- Text frames: JSON for signal_level / channel_activity / squelch / transcription / voicepaint.

### TCP audio stream (`pipeline/streamer.rs`)
- Outbound push to `--stream host:port`: newline-delimited JSON `{source, samples[≤512]}`,
  normalized (freq_monitor integration).

## Local audio (`audio.rs`)

- cpal default output device, monitor volume 0.035, channel-replicated, crossbeam bounded(20)
  feed with try_send (drop on backpressure).

## Channels (`channels.rs`)

- US marine VHF only: 48 channels (1–28, 63–88 subset) with label taxonomy
  (SAFETY-CALL, DSC, BRIDGE, PORT-VTS, USCG, MARINE-OP, NON-COMM…), freq↔channel both ways.

## Viz (`viz.rs`) and TUI (`tui.rs`)

- ASCII spectrum / waveform / scrolling spectrogram renderers (viz mode).
- `tui.rs`: ratatui scaffolding (UiState, controls for squelch/zoom/rate) — **never wired in**.
  Hertz makes the TUI the primary (only) interface.

## Constants worth carrying

| Constant | Value |
|---|---|
| SDR_RATE (narrowband) | 240 000 |
| BUFFER_SIZE | 48 000 IQ pairs (200 ms) |
| Audio rate | 48 000 |
| PREBUFFER_SECONDS | 1.5 |
| AFC smoothing / threshold / max step | 0.85 / 0.5 Hz / 5 kHz |
| Squelch defaults | 6 dB (monitor), 12 dB (scan) |
| Hang | 10 frames ≈ 2 s |
| Flatness open / noise | < 0.55 / > 0.70 |
| Silence release (scan) | 5 s |
| Wideband | 2.4 MS/s @ 156.7375 MHz, 8192-pt FFT, ÷10 DDC |
| Channel FIR | 81 taps @ 11 kHz cutoff |
| Monitor volume | 0.035 |
| History | 200 activity / 50 transcripts |

## Explicitly absent in gnosis-radio (new in Hertz)

- Multi-dongle orchestration (only a single `--device` index per process).
- Any band other than US marine VHF (no CB/FRS/GMRS/MURS/NOAA/ham/air/rail).
- AM / SSB demod (NFM only).
- Transmit of any kind.
- MCP server.
- Containerization (native Windows build with vendored .lib).
- Working TUI.
- In-process Whisper (external service only).
- Channel database as data (tables are hardcoded Rust).
