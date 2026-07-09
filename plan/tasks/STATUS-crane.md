# STATUS — Crane 🏗️

## T6 — TUI client (`hertz tui`): ✅ DONE

### What's Done
- Refactored root `Cargo.toml` and `crates/hertz-tui/Cargo.toml` to declare all workspace dependencies including `ratatui`, `crossterm`, `cpal`, `clap` (with `env` features), `chrono`, `rtrb` etc.
- Implemented TUI library module under `crates/hertz-tui/src/lib.rs` defining the data contracts (`SpectrumFrame`, `UiCommand`), colormaps (Viridis, Inferno, Turbo, Mono), `Palette` abstraction for truecolor/256-color/NO_COLOR fallbacks (W4), `bins_to_columns` aggregation (W3), `History` ring (W2), `tuned_col` locator, and `WaterfallWidget`.
- Created robust test suite inside `crates/hertz-tui/src/tests.rs` containing all the spec §8 acceptance tests:
  - Bounded memory (W2) verification
  - Binned-to-columns (W3) verification
  - Ruler accuracy and caret placement verification
  - Headless rendering tracking the swept peak
  - Live resize safety (W3) verification
  - `NO_COLOR=1` monochrome block-shading fallback (W4) verification
  - Gutter TX overlay time-alignment (W6) verification
  - Channel non-blocking (W1) event queue verification
- Implemented binary entrypoint `hertz` in `crates/hertz-tui/src/main.rs`:
  - Handled all clap subcommands: `tui` (default), `status`, `doctor`, `records`, `tail`, `tune`.
  - Built out thin REST client wrapping axum backend endpoints.
  - Implemented async event processor reading live JSON events and decimation spectrum binary frames from `/stream`.
  - Added CPAL local speaker playback at 0.035 volume default, with `--no-audio` bypass.
  - Implemented fully interactive `ratatui` dashboard featuring:
    - Dongles Pane (F1)
    - Band Scope Pane (F2) with live spectrum trace and Waterfall widget
    - Channel Grid (F3) showing active channel carriers
    - Activity scrolling log (F4)
    - Transcript scrolling log (F5)
    - Manual tune dialog (`t`) and squelch threshold dialog (`s`)
    - TX console dialog (`x`) with compose, confirmation, and mock-transmission sequence.
- Cleaned up ALSA build requirements (installed `libasound2-dev` package inside the docker sandbox container).
- Ran all cargo tests and verified clean clippy (`cargo clippy --all-targets`) and clean formatting (`cargo fmt --check`).

### Bug Fixes
- **Bug #1 (? Help Keymap & F-Keys)**: Implemented full modal keyboard bindings help overlay on `?`. Pressed `Esc` or `?` closes it. Added direct pane focus hotkeys `F1`-`F5` mapping to panes (Dongles, Waterfall, Grid, Activity, Transcripts) as per spec. Added unit tests for help dialog toggle.
- **Bug #2 (Ruler Scope Initial Seeding)**: Seeded TUI scope `center_hz` and `tuned_hz` dynamically upon receiving WS `Hello` message containing the active dongle's actual current `freq_hz`, and whenever the active dongle is switched via arrow keys, ensuring correct initial ruler values. Added unit tests verifying Hello message seeding logic.
- **Bug #3 (Graceful Audio Failure & Stderr Pollution Prevention)**:
  - Gracefully catch CPAL/ALSA host/device/stream errors so the client does not crash, displaying `♪ audio: unavailable` in status bar instead of falling back to default/empty audio device playback.
  - Suppressed ALSA driver warnings by redirecting stderr (fd 2) to `/dev/null` on Unix platforms upon starting the TUI.
  - Moved audio initialization before raw mode and alternate screen entry to prevent probe outputs from corrupting display.

### Verification
- `cargo test -p hertz-tui` passes 100% cleanly.
- `cargo clippy --all-targets` is 100% warning-free.
- `cargo fmt --check` is 100% clean.
