# Status - Cognitive Haddock 🌀

## What's Done
- **Workspace root configuration:**
  - Created `Cargo.toml` with `[workspace.dependencies]` pinning all required packages.
  - Created `.gitignore` ignoring build, data, and audio artifacts, except fixtures.
  - Created `rust-toolchain.toml` pointing to the stable channel.
- **Created stub crates:**
  - `crates/hertz-sdr`, `crates/hertz-dsp`, `crates/hertz-daemon`, `crates/hertz-tui`, `crates/hertz-tx` (created stubs immediately to avoid blocking other workers).
- **Implemented `hertz-types`:**
  - `Event` enum: Tagged serde representation of all 11 event types carrying `dongle_id: String`.
  - `Mode` enum: Nfm, Am, Usb, Lsb.
  - `Channel` struct matching the schema from PLAN §4.
  - `TxPolicy` and `LicenseReq` custom serialization/deserialization logic. Parses strings like `"certified-radio+gmrs-license"`, `"sdr-or-radio+ham"`, or `"never"`.
  - `DongleRole` and configuration structures (`DongleConfig`, `DaemonSettings`, `DaemonConfig`, etc.).
  - Config `load()` logic resolving the configuration path from arguments, environment variables, or standard paths.
- **Implemented `hertz-channels`:**
  - `load_channels()`: Reads and merges all `*.toml` bandplans from a main directory and an optional user directory.
  - Validation: Ensures unique channel IDs across all files, validates frequency range constraints for channelized groups, and enforces restricted TX policies (marine/air/rail/NOAA channels are restricted to `Never`).
  - Indexing: Provides structured access via `get_by_id`, `get_by_group`, and `get_by_freq` with frequency tolerance.
- **Generated `bandplans/` TOML database:**
  - Wrote 12 database files containing a total of **423 channels** (real frequency assignments):
    - `marine-vhf-us.toml` (48 communication channels + 7 WX channels, preserving gnosis channel numbering and labels)
    - `marine-vhf-intl.toml` (33 duplex/simplex variants)
    - `marine-ais.toml` (2 channels)
    - `noaa-wx.toml` (7 channels with `continuous_carrier = true`)
    - `frs-gmrs.toml` (30 channels including shared and repeater input channels)
    - `murs.toml` (5 channels)
    - `cb.toml` (40 AM channels + 5 USB variants)
    - `ham-2m.toml` (50 simplex and repeater channels)
    - `ham-70cm.toml` (35 channels)
    - `airband.toml` (40 aviation AM channels)
    - `railroad-aar.toml` (91 AAR channels 7-97 in 15 kHz steps)
    - `public-safety-interop.toml` (30 mutual aid channels)
- **Host Tools Integration:**
  - Installed `build-essential` inside the container since Cargo dependency build scripts failed due to a missing C compiler/linker (`cc`).

## In Progress
- None. T1 is fully complete.

## Blocked
- None.

## Verification
- **Formatting:** `cargo fmt` executed successfully and formatted all source code.
- **Linting:** `cargo clippy --all-targets` verified warning-free and error-free.
- **Testing:** All unit tests executed and passed successfully:
  - `test_bandplans_load_and_validate`: Confirms that all 12 bandplans parse successfully and loaded channel count is 423 (>= 400).
  - `test_marine_roundtrip`: Confirms channel 16 maps to 156.800 MHz and frequency queries within 1 kHz tolerance resolve back to channel 16.
  - `test_tx_legality`: Confirms validation returns an error if any marine/air/rail channel is configured with a non-Never TX policy.
  - `test_daemon_config_parses`: Verifies that the sample TOML config from PLAN §3 is successfully parsed.
