# T1 — Workspace scaffold, types, channel database (Cognitive Haddock 🌀)

Phase 0 of `plan/PLAN.md` §11, plus the channel database of §4. Read PLAN.md §2 (workspace
layout), §4 (channel DB) and `plan/gnosis-feature-inventory.md` first.

## Your paths
- `Cargo.toml` (workspace root), `.gitignore`, `rust-toolchain.toml`
- `crates/hertz-types/**`
- `crates/hertz-channels/**`
- `bandplans/**`
- Stub-only (Cargo.toml + empty lib.rs so the workspace builds — DO NOT implement):
  `crates/hertz-sdr`, `crates/hertz-dsp`, `crates/hertz-daemon`, `crates/hertz-tui`,
  `crates/hertz-tx`. **Create these stubs FIRST (within your first few minutes)** — the
  other worker fills in `hertz-dsp` and must not be blocked on the workspace root.
  After creating stubs, never touch `crates/hertz-dsp` again.

## Deliverables

### 1. Workspace root
- `Cargo.toml` workspace with members = all six crates; `[workspace.dependencies]` pinning:
  serde, serde_json, thiserror, num-complex, rustfft, hound, chrono, toml, anyhow.
  Edition 2021, resolver 2. Sensible release profile (opt-level 3, lto thin).
- `.gitignore`: target/, data/, *.wav outside fixtures, .env.
- `rust-toolchain.toml`: stable.

### 2. `hertz-types`
Shared serde types used across crates (PLAN §5):
- `Event` enum (tagged, serde): Audio, SignalLevel, SquelchEvent, ChannelActivity,
  Transcription, Translation, RecordingSaved, ScanState, DongleStatus, TxEvent, VoicePaint —
  fields per gnosis inventory (`plan/gnosis-feature-inventory.md` "Event bus" section),
  every variant carries `dongle_id: String`.
- `Mode` enum: Nfm, Am, Usb, Lsb.
- `Channel` struct matching the bandplan TOML schema in PLAN §4 (id, name, freq_hz, mode,
  bandwidth_hz, group, label, rx, tx_policy, ctcss_hz optional, continuous_carrier bool,
  notes).
- `TxPolicy` enum: Never, CertifiedRadio { license: LicenseReq }, SdrOrRadio { license: LicenseReq };
  LicenseReq: None, Gmrs, Ham. **Marine/air/rail groups must deserialize only to Never** —
  enforce in hertz-channels validation, and add a unit test proving a bandplan file that
  sets tx on a marine channel fails validation.
- `DongleRole` enum: Channelized, Hopscan, Monitor + config structs (PLAN §3 TOML shape).
- `DaemonConfig` (the hertz.toml schema from PLAN §3) with serde + defaults + a `load()`
  that reads path or env.

### 3. `hertz-channels`
- Loader: reads every `*.toml` in a bandplans dir (+ optional user dir), validates
  (unique ids, freq ranges per group, tx-policy legality rules), returns an indexed
  `ChannelDb` (by id, by group, by freq lookup with tolerance).
- Marine channel↔frequency mapping preserved from gnosis
  (`plan/reference/gnosis-radio/src/channels.rs`) — same 48 US channels, same labels, as
  part of the marine bandplan data.
- Channelized-capture metadata per group: center_hz + sample_rate (PLAN §4).

### 4. `bandplans/*.toml` — the ~430-channel database
Create the data files per PLAN §4 table. Accuracy matters — these are real allocations:
- `marine-vhf-us.toml` (from gnosis channels.rs: 48 ch + WX stations; labels preserved)
- `marine-vhf-intl.toml` (intl duplex variants)
- `marine-ais.toml` (161.975, 162.025, rx-only data)
- `noaa-wx.toml` (7 ch 162.400–162.550, `continuous_carrier = true`)
- `frs-gmrs.toml` (22 shared FRS/GMRS 462 MHz + 8 GMRS repeater inputs 467 MHz; correct
  interstitial frequencies; tx_policy certified-radio, gmrs license class on GMRS-only)
- `murs.toml` (151.820, 151.880, 151.940, 154.570, 154.600; certified-radio, no license)
- `cb.toml` (40 ch 26.965–27.405, mode am; ch 36–40 also listed as usb variants)
- `ham-2m.toml` (~45: 146.520 calling, common simplex, repeater-output segments; ham license)
- `ham-70cm.toml` (~25 incl. 446.000 calling)
- `airband.toml` (~30 common: 121.500 guard, 122.750, 123.025, 123.450, CTAF/UNICOM set; am)
- `railroad-aar.toml` (AAR channels 7–97: 160.215–161.565 in 15 kHz steps, generated
  programmatically is fine but committed as data)
- `public-safety-interop.toml` (VCALL10 155.7525, VTAC11–14, plus marine mutual aid)
Each group header carries channelized capture params where the group fits ≤2.4 MHz.

### 5. Tests
- Every bandplan loads + validates; total channel count printed in a test and ≥ 400.
- Round-trip: marine ch 16 → 156.800 MHz → ch 16.
- TX legality test as described above.
- `DaemonConfig` example from PLAN §3 parses.

## Done means
`cargo test -p hertz-types -p hertz-channels` green, `cargo check` green for the whole
workspace (stubs make that possible), status file updated per `plan/tasks/README.md`.
