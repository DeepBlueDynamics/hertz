# Hertz → Lume: next-step plan

Goal: hertz logs into Lume the way agents and Signal K do on a HaLOS Pi
(`halos.local`), so radio traffic becomes searchable next to the boat's
telemetry and documents. "What was said on Ch 16 when the bilge alarm went off?"

Lume already has what we need: `lume serve --ti-store <store> --otlp` accepts
OpenTelemetry at `POST /v1/logs` and `POST /v1/metrics`, logs land in the `docs`
table (searchable) and metrics in `telemetry_agents`. Auth is
`Authorization: Bearer <token>` (a nuts JWT / `ahp_` token, or `--http-token-file`).

## Steps

1. **Config.** Add to hertz:
   ```toml
   [lume]
   url = "http://halos.local:5863"     # Lume serve with --otlp
   token_file = "~/.hertz/lume-token"  # nuts ahp_ token or Lume's http token
   ```
   Missing token file → warn and disable, the same as `[relay]`.

2. **Logs (the main value).** New `crates/hertz-daemon/src/lume.rs` subscribes to
   the bus and POSTs OTLP/JSON log records to `/v1/logs`:
   - `Transcription`: body = transcript; attributes `channel`, `freq_hz`,
     `recording`, `engine`, and the double-check text when it differs.
   - `SquelchEvent`: open/close, `signal_db`, classification.
   - Relay outcome: routed pane or call sign, `p`, delivery state.
   `service.name = hertz`, `host.name` = the boat. Batch, retry with backoff, and
   drop when Lume is down. Never block the DSP or relay paths.

3. **Metrics (cheap win).** Every 10 s (Lume's bucket), POST `/v1/metrics` with
   per-channel signal and noise floor, squelch-open seconds, transmissions,
   transcription latency, and dongle online state. Lume's TI SQL can then join
   radio activity to engine, depth and alarms.

4. **Login.** For a Pi on the nuts allowlist: mint an `ahp_` token for a
   `hertz-<boat>` identity once, store it in `token_file` (user-only permissions),
   and add it to Lume's `--nuts-allow`. Offline boats use `--http-token-file`.

5. **HaLOS Pi deploy.** Cross-compile `hertzd` for `aarch64-unknown-linux-gnu`
   (Whistle has a linux-arm64 engine). Add a container under `lume/deploy/halos/`
   next to the existing marine containers: RTL-SDR passthrough (udev rule in
   `docs/60-hertz-rtlsdr.rules`) and `[lume] url = http://localhost:5863`.

6. **Verify.** Key up on Ch 72, then on the Pi:
   `lume search "come in over"` finds the transcript, and
   `lume ti` SQL shows the squelch-open interval beside Signal K data.

## Open questions

- Lume's OTLP logs route: does it keep log attributes as searchable fields, or
  only the body? Check `crates/` in the lume repo before step 2.
- Should transcripts also be indexed as full documents (`lume_index`) rather than
  log lines, for graph extraction?
- On a Pi, keep routing on ollaya/Whisper, or run Whistle only (no GPU)?
