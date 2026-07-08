# TUI Waterfall — Reference Specification

**Version:** 1.0
**Component:** operator console for the onboard box
**Language:** Rust (edition 2021), `ratatui` + `crossterm`
**Status:** implementable reference for the build agent

---

## 1. Overview & scope

A terminal waterfall + spectrum display that renders what the onboard SDR is hearing, so an operator (or you, over SSH to the boat's Pi) can *see* the RF environment around the monitored channel — and see, on the same waterfall, exactly when the voice-relay agent transmitted.

It is a **view**, not a DSP owner. It consumes FFT magnitude frames off a channel, renders them, and sends tuning commands back — it never touches the SDR directly. This keeps it drop-in compatible with the reader-thread + `crossbeam` command-bus pattern already used by the scanner stack and the voice-relay service.

**In scope (v1):** scrolling waterfall (2 rows/character cell via half-blocks, truecolor), a spectrum trace, a frequency ruler, a tuned-channel marker, squelch/carrier indication, a **TX overlay** driven by the voice-relay agent's keyed state, keyboard tuning, dB-range and colormap control.
**Out of scope:** demodulation, recording, multi-VFO, mouse. The waterfall observes; other modules act.

### Where it fits

```
 SDR reader thread ─► FFT ─► SpectrumFrame ──(crossbeam, bounded)──► ┌──────────────────────┐
        ▲                                                            │  Waterfall TUI       │
        │  UiCommand (tune/gain) ◄────────────(crossbeam)────────────┤  (this component)    │
        │                                                            └───────────┬──────────┘
 voice-relay TX worker ── keyed:AtomicBool ──────────────────────────────────────┘  (TX overlay)
```

The `SpectrumFrame` producer and the `UiCommand` consumer already exist in the scanner DSP thread; the `keyed` flag is the same `AtomicBool` the voice-relay TX worker sets in its spec (§7.1 there). This component only reads them.

---

## 2. Requirements (hard)

| ID | Requirement |
|----|-------------|
| **W1** | **UI never blocks DSP.** Frames arrive on a bounded channel; the UI drains with `try_recv` every tick. The producer uses `try_send` and drops on full. The draw loop must never `recv()` (block) on the frame channel. |
| **W2** | **Bounded memory.** History is a fixed-capacity ring (`max_rows`); appending past capacity evicts the oldest. |
| **W3** | **Resize-safe.** Rows are stored at full FFT resolution and binned-to-width **at draw time**, so a terminal resize needs no history rewrite. |
| **W4** | **Color fallback.** 24-bit truecolor by default; degrade to 256-color, then to monochrome block-shading (`░▒▓█`). Honor `NO_COLOR`. |
| **W5** | **Fixed frame budget.** Target FPS is configurable; the input poll timeout equals the frame budget so the UI stays responsive with or without incoming frames. |
| **W6** | **TX overlay.** Rows captured while the voice-relay agent is keyed are tagged and shown in a gutter, so agent transmissions are visible on the waterfall. |
| **W7** | **No direct hardware access.** Tuning/gain changes are sent as `UiCommand` back to the DSP thread. The UI thread owns no SDR handle. |

---

## 3. Data contract

Defined once, shared by the DSP producer and this consumer.

```rust
use std::time::Instant;

/// One time-slice of spectrum, produced by the DSP thread.
#[derive(Clone)]
pub struct SpectrumFrame {
    /// Magnitude per FFT bin, in dBFS. Length == fft_size (e.g. 1024).
    /// Bin 0 = center_hz - span_hz/2 ; last bin = center_hz + span_hz/2.
    pub bins_db: Vec<f32>,
    pub center_hz: f64,
    pub span_hz: f64,          // total bandwidth the bins cover (== sample rate)
    pub squelch_open: bool,    // carrier present on the tuned channel this slice
    pub ts: Instant,
}

/// Commands the UI sends back to the DSP thread (reuses the scanner command bus).
#[derive(Clone, Copy, Debug)]
pub enum UiCommand {
    NudgeTune(f64),   // relative Hz
    SetTune(f64),     // absolute center Hz
    SetGain(f64),     // dB
}
```

**Channel wiring (set up by whoever owns both threads):**
```rust
let (frame_tx, frame_rx) = crossbeam_channel::bounded::<SpectrumFrame>(8); // W1
let (cmd_tx,  cmd_rx)    = crossbeam_channel::unbounded::<UiCommand>();
// DSP thread: on each frame  ->  let _ = frame_tx.try_send(frame);  // drop on full
// DSP thread: drains cmd_rx and applies to the seify::Source message ports.
// voice-relay TX worker:      let keyed = Arc::new(AtomicBool::new(false));
```

---

## 4. Configuration (`waterfall.toml`)

```toml
fft_size       = 1024
max_rows       = 512      # history depth (W2); 1024 f32 * 512 ≈ 2 MB
target_fps     = 50       # W5
db_floor       = -100.0   # dB mapped to coldest color
db_ceil        = -20.0    # dB mapped to hottest color
colormap       = "viridis"  # "viridis" | "inferno" | "turbo" | "mono"
newest_on_top  = true     # false = classic scroll-up
tuned_hz       = 151940000.0   # marker: MURS ch 3 (or your 2m simplex freq)
tune_step_hz   = 12500.0       # arrow-key nudge
tune_step_coarse_hz = 1000000.0
```

---

## 5. Dependencies (`Cargo.toml`)

```toml
[dependencies]
ratatui = "0.29"
crossterm = "0.28"
crossbeam-channel = "0.5"
anyhow = "1"
serde = { version = "1", features = ["derive"] }
toml = "0.8"
# colormaps are hand-rolled below (no extra dep). Swap in `colorgrad = "0.7"` if preferred.
```

---

## 6. Module reference (Rust)

### 6.1 Colormap: dB → RGB

```rust
use ratatui::style::Color;

#[derive(Clone, Copy)]
pub enum Colormap { Viridis, Inferno, Turbo, Mono }

// 9-anchor viridis LUT (r,g,b 0..1). Compact + good perceptual ramp.
const VIRIDIS: [[f32; 3]; 9] = [
    [0.267,0.005,0.329],[0.283,0.141,0.458],[0.254,0.265,0.530],
    [0.207,0.372,0.553],[0.164,0.471,0.558],[0.128,0.567,0.551],
    [0.135,0.659,0.518],[0.267,0.749,0.441],[0.993,0.906,0.144],
];
const INFERNO: [[f32; 3]; 9] = [
    [0.001,0.000,0.014],[0.129,0.047,0.291],[0.318,0.070,0.432],
    [0.500,0.145,0.404],[0.680,0.222,0.331],[0.844,0.331,0.227],
    [0.955,0.485,0.114],[0.988,0.682,0.135],[0.988,0.998,0.645],
];
const TURBO: [[f32; 3]; 9] = [
    [0.190,0.072,0.232],[0.219,0.402,0.884],[0.111,0.664,0.898],
    [0.170,0.844,0.640],[0.516,0.945,0.310],[0.826,0.921,0.196],
    [0.985,0.755,0.184],[0.949,0.442,0.114],[0.729,0.113,0.049],
];

fn ramp(t: f32, lut: &[[f32; 3]; 9]) -> Color {
    let t = t.clamp(0.0, 1.0) * 8.0;
    let i = t.floor() as usize;
    let f = t - i as f32;
    let a = lut[i];
    let b = lut[(i + 1).min(8)];
    let mix = |x: f32, y: f32| ((x + (y - x) * f) * 255.0).round() as u8;
    Color::Rgb(mix(a[0], b[0]), mix(a[1], b[1]), mix(a[2], b[2]))
}

/// Map a dB value to a cell color given the current window and colormap.
pub fn db_to_color(db: f32, floor: f32, ceil: f32, cm: Colormap) -> Color {
    let t = ((db - floor) / (ceil - floor)).clamp(0.0, 1.0);
    match cm {
        Colormap::Viridis => ramp(t, &VIRIDIS),
        Colormap::Inferno => ramp(t, &INFERNO),
        Colormap::Turbo   => ramp(t, &TURBO),
        Colormap::Mono    => Color::Rgb((t*255.0) as u8, (t*255.0) as u8, (t*255.0) as u8),
    }
}
```

> **W4 fallback.** Detect truecolor via `COLORTERM=truecolor|24bit`. If absent, quantize `Color::Rgb` to the nearest xterm-256 (ratatui does not auto-downsample). If `NO_COLOR` is set or the colormap is `Mono` on a non-color terminal, render intensity with block glyphs `[' ','░','▒','▓','█']` indexed by `t` instead of color. Keep this behind a `Palette` abstraction so the widget code below is unchanged.

### 6.2 Bin → column aggregation (W3)

Terminal is ~120 columns; the FFT is 1024 bins. Aggregate with **max-hold per column** so a narrow carrier one bin wide still lights a full column (mean would wash it out).

```rust
/// Reduce `bins_db` to exactly `width` columns using max-hold.
pub fn bins_to_columns(bins: &[f32], width: usize, out: &mut Vec<f32>) {
    out.clear();
    if width == 0 || bins.is_empty() { return; }
    let n = bins.len();
    for x in 0..width {
        let lo = x * n / width;
        let hi = ((x + 1) * n / width).max(lo + 1).min(n);
        let mut m = f32::NEG_INFINITY;
        for &v in &bins[lo..hi] { if v > m { m = v; } }
        out.push(m);
    }
}
```

### 6.3 History ring

```rust
use std::collections::VecDeque;

pub struct Row {
    pub bins_db: Vec<f32>, // full FFT resolution (W3)
    pub squelch_open: bool,
    pub tx: bool,          // agent was keyed when this slice was captured (W6)
}

pub struct History {
    rows: VecDeque<Row>,   // front = newest
    cap: usize,
}
impl History {
    pub fn new(cap: usize) -> Self { Self { rows: VecDeque::with_capacity(cap), cap } }
    pub fn push(&mut self, row: Row) {
        self.rows.push_front(row);
        while self.rows.len() > self.cap { self.rows.pop_back(); } // W2
    }
    pub fn rows(&self) -> &VecDeque<Row> { &self.rows }
}
```

### 6.4 Waterfall widget (half-block, 2 rows per cell)

Each character cell renders **two** history rows: `▀` with foreground = top pixel, background = bottom pixel. This doubles vertical resolution.

```rust
use ratatui::{buffer::Buffer, layout::Rect, style::Color, widgets::Widget};

pub struct WaterfallWidget<'a> {
    pub hist: &'a History,
    pub floor: f32,
    pub ceil: f32,
    pub cm: Colormap,
    pub newest_on_top: bool,
    pub tuned_col: Option<u16>, // column of the tuned marker (for a faint overlay line)
}

impl<'a> Widget for &WaterfallWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let w = area.width as usize;
        if w == 0 || area.height == 0 { return; }

        // One char row = two waterfall rows.
        let pixel_rows_visible = (area.height as usize) * 2;
        let rows: Vec<&Row> = self.hist.rows().iter().take(pixel_rows_visible).collect();

        let mut top_cols: Vec<f32> = Vec::with_capacity(w);
        let mut bot_cols: Vec<f32> = Vec::with_capacity(w);

        for cy in 0..area.height {
            // index into `rows` for the two pixels stacked in this char row
            let (ti, bi) = if self.newest_on_top {
                (cy as usize * 2, cy as usize * 2 + 1)
            } else {
                let base = (area.height - 1 - cy) as usize * 2;
                (base, base + 1)
            };

            match rows.get(ti) { Some(r) => bins_to_columns(&r.bins_db, w, &mut top_cols),
                                 None => top_cols.clear() }
            match rows.get(bi) { Some(r) => bins_to_columns(&r.bins_db, w, &mut bot_cols),
                                 None => bot_cols.clear() }

            let y = area.y + cy;
            for cx in 0..area.width {
                let x = area.x + cx;
                let ci = cx as usize;
                let top = top_cols.get(ci).copied().unwrap_or(f32::NEG_INFINITY);
                let bot = bot_cols.get(ci).copied().unwrap_or(f32::NEG_INFINITY);

                let mut fg = shade(top, self.floor, self.ceil, self.cm);
                let mut bg = shade(bot, self.floor, self.ceil, self.cm);

                // faint tuned-marker overlay: brighten the marker column
                if Some(cx) == self.tuned_col {
                    fg = blend(fg, Color::Rgb(255, 255, 255), 0.35);
                    bg = blend(bg, Color::Rgb(255, 255, 255), 0.35);
                }

                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char('▀').set_fg(fg).set_bg(bg);
                }
            }
        }
    }
}

fn shade(db: f32, floor: f32, ceil: f32, cm: Colormap) -> Color {
    if db.is_finite() { db_to_color(db, floor, ceil, cm) } else { Color::Reset }
}
fn blend(a: Color, b: Color, t: f32) -> Color {
    if let (Color::Rgb(ar,ag,ab), Color::Rgb(br,bg,bb)) = (a, b) {
        let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
        Color::Rgb(m(ar,br), m(ag,bg), m(ab,bb))
    } else { a }
}
```

### 6.5 Ruler, TX gutter, spectrum trace, status

- **Frequency ruler** (one char row under/over the waterfall): left edge = `center - span/2`, right edge = `center + span/2`. Place ~5 evenly spaced tick labels in MHz (`{:.4}`), and a caret `▲` at `tuned_col`.
- **TX gutter** (1-col `Layout` on the left of the waterfall): for each char row, if either of its two rows has `tx == true`, draw `▐` in red; else blank. Result: agent transmissions appear as a red bar down the left edge, time-aligned with the waterfall (W6). A squelch-open slice can use a second gutter color (e.g. green) if you want carrier activity marked too.
- **Spectrum trace** (optional top panel): render the newest row as a line using a braille `Canvas`, or a simple per-column bar (`█` height ∝ normalized dB). Cheap and readable; keep it ≤ 6 rows tall.
- **Status line:** `center MHz · span kHz · dB[floor,ceil] · colormap · fps · SQL:open/idle · TX:on/off`.

Compute `tuned_col`:
```rust
fn tuned_col(tuned_hz: f64, center_hz: f64, span_hz: f64, width: u16) -> Option<u16> {
    let lo = center_hz - span_hz / 2.0;
    let frac = (tuned_hz - lo) / span_hz;
    if (0.0..=1.0).contains(&frac) { Some((frac * (width as f64 - 1.0)).round() as u16) }
    else { None }
}
```

### 6.6 Draw loop + input (W1/W5/W7)

```rust
use crossterm::event::{self, Event, KeyCode};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::{Duration, Instant};

pub struct AppState {
    pub hist: History,
    pub floor: f32,
    pub ceil: f32,
    pub cm: Colormap,
    pub newest_on_top: bool,
    pub center_hz: f64,
    pub span_hz: f64,
    pub tuned_hz: f64,
    pub squelch_open: bool,
    pub paused: bool,
}

pub fn run(
    mut state: AppState,
    frame_rx: crossbeam_channel::Receiver<SpectrumFrame>,
    cmd_tx: crossbeam_channel::Sender<UiCommand>,
    keyed: Arc<AtomicBool>,                 // shared with voice-relay TX worker (W6)
    cfg: &Config,
) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();     // raw mode + alt screen; ratatui::restore() on exit
    let budget = Duration::from_secs_f64(1.0 / cfg.target_fps as f64);

    let res = (|| -> anyhow::Result<()> {
        loop {
            // --- input, bounded by the frame budget (W5) ---
            if event::poll(budget)? {
                if let Event::Key(k) = event::read()? {
                    match k.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Char(' ') => state.paused = !state.paused,
                        KeyCode::Left  => { let _ = cmd_tx.send(UiCommand::NudgeTune(-cfg.tune_step_hz)); }
                        KeyCode::Right => { let _ = cmd_tx.send(UiCommand::NudgeTune( cfg.tune_step_hz)); }
                        KeyCode::Char('[') => state.floor -= 5.0,
                        KeyCode::Char(']') => state.floor += 5.0,
                        KeyCode::Char('{') => state.ceil  -= 5.0,
                        KeyCode::Char('}') => state.ceil  += 5.0,
                        KeyCode::Char('c') => state.cm = next_colormap(state.cm),
                        _ => {}
                    }
                }
            }

            // --- drain frames without blocking (W1) ---
            if !state.paused {
                while let Ok(f) = frame_rx.try_recv() {
                    state.center_hz = f.center_hz;
                    state.span_hz = f.span_hz;
                    state.squelch_open = f.squelch_open;
                    state.hist.push(Row {
                        bins_db: f.bins_db,
                        squelch_open: f.squelch_open,
                        tx: keyed.load(Ordering::Relaxed),   // W6
                    });
                }
            }

            // --- draw ---
            terminal.draw(|frame| render_ui(frame, &state, cfg))?;
        }
        Ok(())
    })();

    ratatui::restore();
    res
}
```

`render_ui` splits `frame.area()` with a `Layout` into: status line, optional spectrum panel, and the waterfall region; the waterfall region is further split into a 1-col TX gutter + the `WaterfallWidget` + a ruler row. Pass `tuned_col(...)` into the widget.

---

## 7. Controls

| Key | Action |
|-----|--------|
| `←` / `→` | tune down / up by `tune_step_hz` (sends `UiCommand` — W7) |
| `Shift+←/→` | coarse tune by `tune_step_coarse_hz` |
| `[` / `]` | lower / raise dB floor |
| `{` / `}` | lower / raise dB ceiling |
| `c` | cycle colormap (viridis → inferno → turbo → mono) |
| `space` | pause/resume the scroll (freezes history intake) |
| `q` | quit (restores terminal) |

---

## 8. Testing & acceptance

Ship a `--demo` synthetic source so the whole UI runs with no SDR: a thread pushes `SpectrumFrame`s at `target_fps` with a noise floor near `db_floor` plus a Gaussian peak whose center sweeps slowly across the span, and toggles the shared `keyed` flag every few seconds to exercise the TX overlay.

| Test | Method | Pass criteria |
|------|--------|---------------|
| Renders headless | `TestBackend`, run demo N frames | buffer non-empty; hottest cell tracks the swept peak's column |
| **Non-blocking (W1)** | stall the draw (sleep) while producer runs full-tilt | producer's `try_send` drops frames; no deadlock; UI resumes cleanly |
| Bounded memory (W2) | run 10 min | `history.len()` never exceeds `max_rows` |
| Resize-safe (W3) | shrink/grow terminal live | no panic; columns re-bin; peak still aligned to its frequency |
| Color fallback (W4) | run with `NO_COLOR=1` and a 256-color `TERM` | block-shading / 256-color path renders, still legible |
| **TX overlay (W6)** | demo toggles `keyed` | red gutter bar appears on exactly the rows captured while keyed, time-aligned |
| Tune command (W7) | press `→`, assert on `cmd_rx` | `UiCommand::NudgeTune(+step)` received by the DSP side; UI never touches hardware |
| Ruler accuracy | fixed center/span | tick labels match `center ± span/2`; caret sits on `tuned_hz` |

---

## 9. Deliverables checklist for the build agent

- [ ] `SpectrumFrame` / `UiCommand` types + channel wiring notes
- [ ] `Palette` abstraction: truecolor / 256-color / mono with `NO_COLOR` (W4)
- [ ] `db_to_color` + viridis/inferno/turbo/mono ramps
- [ ] `bins_to_columns` max-hold aggregation (W3)
- [ ] `History` ring (W2)
- [ ] `WaterfallWidget` (half-block, tuned-marker overlay)
- [ ] Ruler + TX gutter + status line; optional spectrum trace panel
- [ ] Draw loop: `try_recv` drain, budgeted input poll, pause, quit-restore (W1/W5)
- [ ] Keybinds sending `UiCommand` to the DSP bus (W7)
- [ ] `keyed: Arc<AtomicBool>` shared with the voice-relay TX worker (W6)
- [ ] `--demo` synthetic source + the §8 acceptance tests
- [ ] TOML config loader

---

## 10. Notes

- **Integration point.** The `keyed` `AtomicBool` is the *same* flag the voice-relay spec's TX worker sets around key-up. Share one `Arc<AtomicBool>` between the two components and the operator sees agent transmissions painted on the waterfall for free — no extra plumbing.
- **On a headless Pi over SSH,** truecolor usually works if `COLORTERM=truecolor` is exported through the SSH session; if the waterfall looks banded, that env var is missing and W4's 256-color path is what you're seeing.
- **Frame rate vs. FFT rate.** `target_fps` governs redraw; the DSP thread's FFT frame rate governs vertical scroll speed. If scroll is too fast to read, decimate frames in the producer (push every Nth) rather than slowing the UI.
- **Keep it a view.** Any feature that *acts* on the radio (record, retune-on-activity, TX) belongs in the scanner/voice-relay modules and is driven through `UiCommand` or their own control surfaces — the waterfall only shows and steers.
