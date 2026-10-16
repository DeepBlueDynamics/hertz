# AIS decoder design sketch (phase 3)

Companion to [mcp-tools.md](mcp-tools.md) § "Any frequency, any mode". This
sketches the `ais_start`/`ais_stop`/`ais_targets` decoder: the DSP chain, crate
placement, how it shares the scan dongle, and the test strategy. It is a sketch
for review, not a commitment to every constant.

## Signal and capture

AIS is GMSK (BT = 0.4), 9600 baud, NRZI, HDLC framing, CRC-16, payload
armored as 6-bit into NMEA `!AIVDM` sentences.

- AIS-1 161.975 MHz, AIS-2 162.025 MHz — ±25 kHz around a 162.000 MHz center.
- Both fit one existing monitor capture: the scan worker already runs at
  240 kS/s (`SDR_RATE`, `pipeline.rs:22`), Nyquist ±120 kHz, tuner bandwidth
  150 kHz. **No `WorkerCmd::SetSampleRate` and no wider capture are needed.**
  This is the one decoder that drops onto the current monitor role unchanged;
  WFM and sweep are the tools that need the rate change.

Consequence for ordering: AIS is lower-risk than the plan's phase-3 WFM/sweep
work and unblocks marine use, so it should be pulled ahead of (or decoupled
from) the `SetSampleRate` refactor.

## DSP chain (per channel)

Both channels are decoded from the same 200 ms frame by mixing each to 0 Hz —
exactly what the scan group already does for multiple members
(`pipeline_bridge.rs` `Mixer` + `groups[g].members`). Chain, per channel:

1. **Mix to baseband.** Phase-continuous `Mixer` at offset ±25 kHz (reuse the
   existing struct; it already carries phase across frames).
2. **Channel filter.** GMSK 9600 occupies ~12.5 kHz. A low-pass around 12.5 kHz
   rejects the adjacent AIS channel and noise. The NFM `CHAN_FIR` (11 kHz
   cutoff, `nfm.rs:20`) is close but tuned for 3 kHz voice; use a dedicated
   AIS LPF (e.g. 13–15 kHz cutoff) rather than reuse it.
3. **FM discriminator.** GMSK is continuous-phase FSK. Compute instantaneous
   frequency via `arg(x[n] · conj(x[n-1]))` per sample (differential detection),
   then low-pass to the data bandwidth. Do not reuse `NfmDemod`: it is coupled
   to the ÷5 decimation and a 3 kHz audio LPF (`nfm.rs`).
4. **Clock recovery.** 25 samples/symbol at 240 kS/s. Early-late gate (or
   Mueller–Müller) on the discriminator output to land on the symbol center.
5. **Bit decision + NRZI.** Sign of the frequency error → bit; NRZI decode
   (transition = 0, no transition = 1).
6. **Bit de-stuff.** HDLC inserts a 0 after five 1s; remove it.
7. **Frame extraction.** Sync on 0x7E flags; verify CRC-16 (CCITT, init 0xFFFF);
   emit the raw 6-bit AIS payload.

Reference prior art for the demod shape only: `rtl-ais` and `gr-ais` use the
same discriminator → LPF → clock-recovery → slicer → HDLC structure. No code is
copied; it is GPL, hertz is BSD-3.

## Crate placement

Split DSP from domain logic, matching how the codebase already separates
`hertz-dsp` (no daemon types) from `hertz-daemon` (`weather.rs`,
`transcribe.rs`):

- **`hertz-dsp/src/decode/ais.rs`** (new `decode` module): pure
  `&[Complex32]` → decoded frames. GMSK demod, NRZI, de-stuffing, HDLC framing,
  CRC, and the 6-bit→8-bit de-armoring of the payload. Expose
  `AisDecoder::new()`, `fn process(&mut self, iq: &[Complex32]) -> Vec<AisFrame>`
  where `AisFrame { channel, bits: Vec<u8>, crc_ok: bool }`. No `serde`, no
  `Event`, no `Channel` — testable against synthetic IQ in isolation.
- **`hertz-daemon/src/ais.rs`** (like `weather.rs`): the NMEA/AIVDM *interpretation*
  — fragment reassembly across multi-sentence messages, type 1/2/3/5/18/24 field
  decode (MMSI, lat/lon, SOG/COG, name), and the target table (MMSI → last
  report, last-seen, signal). Publishes `Event::Ais` and emits `radio.ais` to
  Lume. `ais_targets` reads this table.

Keep the 6-bit armoring (bit-level) in `hertz-dsp`; the sentence/ship semantics
are daemon concerns. A new `hertz-ais` crate is not worth it for one decoder;
revisit only if APRS/ADS-B (phase 4) grow a shared "data decoder" surface.

## Dongle sharing

Follow the weather pattern (`WeatherControl` epoch + idle-point sampling in
`spawn_scan_loop`), not a new mechanism:

- `AisControl` (epoch + `enabled`, `window_seconds`, `every_minutes`), cloned
  like `WeatherControl`.
- The scan loop, at its existing idle point (where weather is checked,
  `pipeline_bridge.rs` ~1100), checks the AIS epoch and, when due, takes receive
  offline via `set_receive_state("ais", …)`, retunes to 162.000 MHz, drains +
  settles (`SETTLE_BYTES`), then loops 200 ms frames feeding the two `AisDecoder`s.
  On `every_minutes` it returns to the scan and restores `set_receive_state`.
- AIS is bursty, so a short window yields a sparse snapshot (class A ~2–10 s
  period, class B ~3 min). Default `window_seconds` 60 s, `every_minutes` 5,
  both configurable — state the expected yield in `ais_status`.
- A full-time `decoder` dongle role (phase 4) is the real answer; the timed
  window is a stopgap. `ais_start`/`ais_stop` take a `dongle` argument so a
  second dongle can be dedicated.

Two things this forces:

1. **Generalize the idle-point sideband.** Today the scan loop hard-codes one
   sideband (weather). Adding AIS, sweep and WFM `tune` means N sidebands that
   each borrow idle time. Introduce one "idle sideband scheduler" (a due-time
   per sideband, checked at the idle point) rather than a second `if let`.
   This is the refactor that keeps phase 3 from re-implementing
   `sample_weather`'s settle/drain/restore a third time.
2. **AIS does not reuse `sample_weather`.** That function demodulates NFM to
   audio; AIS needs raw IQ. Factor the *capture* half (retune → settle → drain →
   loop frames → restore) out of both `sample_weather` and the scan loop into a
   `capture_window(iq | audio sink)` helper, then let weather and AIS differ
   only in what they do with each frame.

## Band-plan mode

`marine-ais.toml` currently declares `mode = "nfm"`. That is wrong for a
decoder: `scan_channels` (`pipeline_bridge.rs`) admits only
`hertz_types::Mode::Nfm` into a voice scan list, so an AIS channel would be
"scanned" as NFM voice (static → locked out). Add `Mode::Ais` (or a
`kind = "data"` field) to `hertz_types::Mode` and set the AIS channels to it,
so the voice scanner naturally excludes them and the decoder registry claims
them. `hertz_types::Mode` already has unused `Usb`/`Lsb`; a `Wfm` and `Ais`
variant are consistent additions.

## Test strategy

Synthetic-first; recorded IQ is a later on-air fixture, not the primary harness.

1. **Synthetic GMSK modulator** (test-only, `hertz-dsp/src/decode/ais.rs`
   `#[cfg(test)]`): build a known NMEA sentence → 6-bit armor → bit-stuff →
   CRC → HDLC framing → NRZI → GMSK modulation (Gaussian pulse BT = 0.4,
   integrate phase, emit `Complex32` at 240 kS/s) at a chosen ±25 kHz offset,
   with knobs for SNR (additive noise), frequency offset (±1 kHz), sample-clock
   offset (±50 ppm) and symbol-timing phase.
2. **Decoder round-trip (the golden test):** modulated IQ → `AisDecoder` →
   exact NMEA string, across a matrix of SNR {20, 10 dB} × frequency offset ×
   clock offset. This is what catches a discriminator/timing-recovery bug.
3. **CRC & framing:** one injected bit error → frame rejected; the
   five-ones-then-zero de-stuffing edge; multi-sentence (type 5) fragment
   reassembly → correct ship name.
4. **Integration (mock daemon, `tests/mcp.rs` style, 127.0.0.1 port 0):** a
   `MockSdr`/`MockSource` emitting a pre-generated AIS IQ buffer at 162.000 MHz;
   `ais_start` → `ais_targets` reports the vessel. Feeds the real scan-loop
   capture path end-to-end without a dongle.
5. **On air:** record 30–60 s of real AIS IQ as a fixture (gzip; 240 kS/s ×
   2 B × 60 s ≈ 28.8 MB raw) and assert ≥1 vessel decoded. Kept as a regression
   asset, not run in CI by default.

## Open risks to fold into the plan

- GMSK + HDLC + AIVDM is green-field in this codebase (only Nfm/Am demods
  exist). The plan's "AIS about 2 days" is optimistic; the demod + timing
  recovery + bit layer is the bulk, message decode is mechanical.
- Clock recovery is the fiddly part: 25 samples/symbol at 240 kS/s is
  comfortable, but ±50 ppm RTL-SDR crystals and symbol-timing phase need a real
  loop, not a fixed decimation.
- Timed-window sharing biases the target table toward frequent (class A)
  transmitters; class B is mostly invisible in a 60 s window. State this in
  `ais_status`/docs rather than surprise anyone.
