# T6 — TUI client (`hertz tui`) — unassigned, ready for dispatch

Phase 4 of `plan/PLAN.md` §11. The ratatui client. **Normative reference for the waterfall
pane: `plan/tui-waterfall-spec.md` (v1.0)** — implement it as written, with the adaptations
in §Adaptations below. Overall pane layout: PLAN §8. Wire protocol: `hertz-types/src/wire.rs`
(already implemented and tested; do not change it — request changes via STATUS file).

## Your paths
- `crates/hertz-tui/**` only. Workspace root Cargo.toml only for new
  `[workspace.dependencies]` (ratatui, crossterm, tokio-tungstenite, cpal, clap).

## Adaptations of the waterfall spec (network client, not in-process)
1. **`SpectrumFrame` source**: the spec's crossbeam producer is replaced by the daemon WS.
   A client task deserializes incoming spectrum frames into the spec's `SpectrumFrame` and
   `try_send`s into the same bounded(8) channel — W1 semantics preserved end-to-end.
   NOTE: the daemon does not yet emit spectrum frames on the wire (queued as T4.1 for the
   daemon owner). Until it does, the spec's REQUIRED `--demo` synthetic source (spec §8) is
   the frame producer — build the whole waterfall against `--demo` and the seam makes the
   WS source a drop-in later.
2. **`UiCommand` sink**: spec's `cmd_tx` maps to daemon calls — `NudgeTune`/`SetTune` →
   `POST /api/dongles/{id}/tune`, `SetGain` → the tune endpoint's gain field. Same enum,
   HTTP behind it.
3. **TX overlay `keyed` flag**: set a local `Arc<AtomicBool>` from `TxEvent` wire events
   (keyed/unkeyed) instead of sharing memory with the TX worker. Same gutter rendering.
4. Everything else in the spec verbatim: half-block rendering, max-hold `bins_to_columns`,
   `History` ring, colormaps + `Palette` fallback (W4), frame-budget loop (W5), keybinds
   (§7), and ALL §8 acceptance tests (TestBackend, non-blocking, resize, NO_COLOR, TX
   overlay, tune command, ruler accuracy).

## Beyond the waterfall (PLAN §8)
- `hertz` binary, clap subcommands: `tui` (default), `status`, `doctor`, `records`, `tail`
  — the non-tui ones are thin REST printers. Flags: `--connect URL` (default
  http://localhost:9080), `--token` (or HERTZ_TOKEN).
- Panes besides the waterfall: dongle list, channel activity grid (from ChannelActivity
  events), scrolling event log, transcript pane, status bar (spec §6.5 status line +
  REC/LISTEN/MCP indicators). Function keys / tab to switch focus; waterfall keybinds
  active when waterfall focused.
- Audio: subscribe binary audio frames for the selected channel; cpal playback at 0.035
  volume; `--no-audio` flag.
- Reconnect: WS drop → status bar shows offline, retry with backoff; UI never exits on
  connection loss.

## Done means
`cargo test -p hertz-tui` green including all spec §8 acceptance tests; `hertz tui --demo`
renders the full layout with the swept-peak demo and TX gutter exercise; clippy clean;
STATUS file updated.
