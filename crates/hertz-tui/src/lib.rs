use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::Instant;

// ---------------------------------------------------------------------------
// Data Contract (Spec §3)
// ---------------------------------------------------------------------------

/// One time-slice of spectrum, produced by the DSP thread or WS.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpectrumFrame {
    /// Magnitude per FFT bin, in dBFS. Length == fft_size (e.g. 1024).
    pub bins_db: Vec<f32>,
    pub center_hz: f64,
    pub span_hz: f64,       // total bandwidth the bins cover
    pub squelch_open: bool, // carrier present on the tuned channel
    #[serde(skip, default = "Instant::now")]
    pub ts: Instant,
}

/// Commands the UI sends back to the DSP thread or daemon.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum UiCommand {
    NudgeTune(f64), // relative Hz
    SetTune(f64),   // absolute center Hz
    SetGain(f64),   // dB
}

// ---------------------------------------------------------------------------
// Configuration (Spec §4)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub fft_size: usize,
    pub max_rows: usize,
    pub target_fps: u32,
    pub db_floor: f32,
    pub db_ceil: f32,
    pub colormap: String,
    pub newest_on_top: bool,
    pub tuned_hz: f64,
    pub tune_step_hz: f64,
    pub tune_step_coarse_hz: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            fft_size: 1024,
            max_rows: 512,
            target_fps: 50,
            db_floor: -100.0,
            db_ceil: -20.0,
            colormap: "viridis".to_string(),
            newest_on_top: true,
            tuned_hz: 151940000.0,
            tune_step_hz: 12500.0,
            tune_step_coarse_hz: 1000000.0,
        }
    }
}

impl Config {
    pub fn load_from_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    pub fn load() -> Self {
        if let Ok(content) = std::fs::read_to_string("waterfall.toml") {
            if let Ok(cfg) = Self::load_from_str(&content) {
                return cfg;
            }
        }
        Self::default()
    }
}

// ---------------------------------------------------------------------------
// Colormap: dB → RGB (Spec §6.1)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Colormap {
    Viridis,
    Inferno,
    Turbo,
    Mono,
}

const VIRIDIS: [[f32; 3]; 9] = [
    [0.267, 0.005, 0.329],
    [0.283, 0.141, 0.458],
    [0.254, 0.265, 0.530],
    [0.207, 0.372, 0.553],
    [0.164, 0.471, 0.558],
    [0.128, 0.567, 0.551],
    [0.135, 0.659, 0.518],
    [0.267, 0.749, 0.441],
    [0.993, 0.906, 0.144],
];
#[allow(clippy::approx_constant)]
const INFERNO: [[f32; 3]; 9] = [
    [0.001, 0.000, 0.014],
    [0.129, 0.047, 0.291],
    [0.318, 0.070, 0.432],
    [0.500, 0.145, 0.404],
    [0.680, 0.222, 0.331],
    [0.844, 0.331, 0.227],
    [0.955, 0.485, 0.114],
    [0.988, 0.682, 0.135],
    [0.988, 0.998, 0.645],
];
const TURBO: [[f32; 3]; 9] = [
    [0.190, 0.072, 0.232],
    [0.219, 0.402, 0.884],
    [0.111, 0.664, 0.898],
    [0.170, 0.844, 0.640],
    [0.516, 0.945, 0.310],
    [0.826, 0.921, 0.196],
    [0.985, 0.755, 0.184],
    [0.949, 0.442, 0.114],
    [0.729, 0.113, 0.049],
];

pub fn next_colormap(cm: Colormap) -> Colormap {
    match cm {
        Colormap::Viridis => Colormap::Inferno,
        Colormap::Inferno => Colormap::Turbo,
        Colormap::Turbo => Colormap::Mono,
        Colormap::Mono => Colormap::Viridis,
    }
}

pub fn colormap_from_str(s: &str) -> Colormap {
    match s.to_lowercase().as_str() {
        "inferno" => Colormap::Inferno,
        "turbo" => Colormap::Turbo,
        "mono" => Colormap::Mono,
        _ => Colormap::Viridis,
    }
}

pub fn colormap_to_str(cm: Colormap) -> &'static str {
    match cm {
        Colormap::Viridis => "viridis",
        Colormap::Inferno => "inferno",
        Colormap::Turbo => "turbo",
        Colormap::Mono => "mono",
    }
}

fn ramp(t: f32, lut: &[[f32; 3]; 9]) -> Color {
    let t = t.clamp(0.0, 1.0) * 8.0;
    let i = t.floor() as usize;
    let f = t - i as f32;
    let a = lut[i];
    let b = lut[(i + 1).min(8)];
    let mix = |x: f32, y: f32| ((x + (y - x) * f) * 255.0).round() as u8;
    Color::Rgb(mix(a[0], b[0]), mix(a[1], b[1]), mix(a[2], b[2]))
}

pub fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    let thresh = 8;
    if (r as i16 - g as i16).abs() < thresh && (g as i16 - b as i16).abs() < thresh {
        let avg = (r as u16 + g as u16 + b as u16) / 3;
        if avg < 3 {
            16
        } else if avg > 248 {
            231
        } else {
            232 + ((avg - 8) * 24 / 247).min(23) as u8
        }
    } else {
        let qr = (r as u16 * 5 / 255) as u8;
        let qg = (g as u16 * 5 / 255) as u8;
        let qb = (b as u16 * 5 / 255) as u8;
        16 + 36 * qr + 6 * qg + qb
    }
}

pub fn db_to_color_raw(db: f32, floor: f32, ceil: f32, cm: Colormap) -> Color {
    let t = ((db - floor) / (ceil - floor)).clamp(0.0, 1.0);
    match cm {
        Colormap::Viridis => ramp(t, &VIRIDIS),
        Colormap::Inferno => ramp(t, &INFERNO),
        Colormap::Turbo => ramp(t, &TURBO),
        Colormap::Mono => Color::Rgb((t * 255.0) as u8, (t * 255.0) as u8, (t * 255.0) as u8),
    }
}

// ---------------------------------------------------------------------------
// Palette Abstraction (W4 fallback)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Palette {
    pub is_truecolor: bool,
    pub no_color: bool,
}

impl Default for Palette {
    fn default() -> Self {
        Self::new()
    }
}

impl Palette {
    pub fn new() -> Self {
        let is_truecolor = std::env::var("COLORTERM")
            .map(|v| v == "truecolor" || v == "24bit")
            .unwrap_or(false);
        let no_color = std::env::var("NO_COLOR").is_ok();
        Self {
            is_truecolor,
            no_color,
        }
    }

    pub fn shade(&self, db: f32, floor: f32, ceil: f32, cm: Colormap) -> Color {
        if !db.is_finite() {
            return Color::Reset;
        }
        if self.no_color {
            return Color::Reset;
        }
        let raw = db_to_color_raw(db, floor, ceil, cm);
        if self.is_truecolor {
            raw
        } else {
            match raw {
                Color::Rgb(r, g, b) => Color::Indexed(rgb_to_ansi256(r, g, b)),
                other => other,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Bin → column aggregation (W3)
// ---------------------------------------------------------------------------

pub fn bins_to_columns(bins: &[f32], width: usize, out: &mut Vec<f32>) {
    out.clear();
    if width == 0 || bins.is_empty() {
        return;
    }
    let n = bins.len();
    for x in 0..width {
        let lo = x * n / width;
        let hi = ((x + 1) * n / width).max(lo + 1).min(n);
        let mut m = f32::NEG_INFINITY;
        for &v in &bins[lo..hi] {
            if v > m {
                m = v;
            }
        }
        out.push(m);
    }
}

// ---------------------------------------------------------------------------
// History ring (W2)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Row {
    pub bins_db: Vec<f32>, // full FFT resolution (W3)
    pub squelch_open: bool,
    pub tx: bool, // agent was keyed when this slice was captured (W6)
}

#[derive(Clone, Debug)]
pub struct History {
    rows: VecDeque<Row>, // front = newest
    cap: usize,
}

impl History {
    pub fn new(cap: usize) -> Self {
        Self {
            rows: VecDeque::with_capacity(cap),
            cap,
        }
    }
    pub fn push(&mut self, row: Row) {
        self.rows.push_front(row);
        while self.rows.len() > self.cap {
            self.rows.pop_back();
        } // W2
    }
    pub fn rows(&self) -> &VecDeque<Row> {
        &self.rows
    }
    pub fn len(&self) -> usize {
        self.rows.len()
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
    pub fn clear(&mut self) {
        self.rows.clear();
    }
}

// ---------------------------------------------------------------------------
// Tuned Column computation
// ---------------------------------------------------------------------------

pub fn tuned_col(tuned_hz: f64, center_hz: f64, span_hz: f64, width: u16) -> Option<u16> {
    let lo = center_hz - span_hz / 2.0;
    let frac = (tuned_hz - lo) / span_hz;
    if (0.0..=1.0).contains(&frac) {
        Some((frac * (width as f64 - 1.0)).round() as u16)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Waterfall Widget (half-block, 2 rows per cell)
// ---------------------------------------------------------------------------

pub struct WaterfallWidget<'a> {
    pub hist: &'a History,
    pub floor: f32,
    pub ceil: f32,
    pub cm: Colormap,
    pub newest_on_top: bool,
    pub tuned_col: Option<u16>,
    pub palette: &'a Palette,
}

impl<'a> Widget for &WaterfallWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let w = area.width as usize;
        if w == 0 || area.height == 0 {
            return;
        }

        let pixel_rows_visible = (area.height as usize) * 2;
        let rows: Vec<&Row> = self.hist.rows().iter().take(pixel_rows_visible).collect();

        let mut top_cols: Vec<f32> = Vec::with_capacity(w);
        let mut bot_cols: Vec<f32> = Vec::with_capacity(w);

        for cy in 0..area.height {
            let (ti, bi) = if self.newest_on_top {
                (cy as usize * 2, cy as usize * 2 + 1)
            } else {
                let base = (area.height - 1 - cy) as usize * 2;
                (base, base + 1)
            };

            match rows.get(ti) {
                Some(r) => bins_to_columns(&r.bins_db, w, &mut top_cols),
                None => top_cols.clear(),
            }
            match rows.get(bi) {
                Some(r) => bins_to_columns(&r.bins_db, w, &mut bot_cols),
                None => bot_cols.clear(),
            }

            let y = area.y + cy;
            for cx in 0..area.width {
                let x = area.x + cx;
                let ci = cx as usize;
                let top = top_cols.get(ci).copied().unwrap_or(f32::NEG_INFINITY);
                let bot = bot_cols.get(ci).copied().unwrap_or(f32::NEG_INFINITY);

                if let Some(cell) = buf.cell_mut((x, y)) {
                    if self.palette.no_color
                        || (self.cm == Colormap::Mono && !self.palette.is_truecolor)
                    {
                        // Monochrome block shading fallback
                        let t = ((top - self.floor) / (self.ceil - self.floor)).clamp(0.0, 1.0);
                        let glyphs = [' ', '░', '▒', '▓', '█'];
                        let idx = (t * 4.0).round() as usize;
                        cell.set_char(glyphs[idx])
                            .set_fg(Color::Reset)
                            .set_bg(Color::Reset);
                    } else {
                        let mut fg = self.palette.shade(top, self.floor, self.ceil, self.cm);
                        let mut bg = self.palette.shade(bot, self.floor, self.ceil, self.cm);

                        if Some(cx) == self.tuned_col {
                            fg = blend(fg, Color::Rgb(255, 255, 255), 0.35);
                            bg = blend(bg, Color::Rgb(255, 255, 255), 0.35);
                        }
                        cell.set_char('▀').set_fg(fg).set_bg(bg);
                    }
                }
            }
        }
    }
}

pub fn blend(a: Color, b: Color, t: f32) -> Color {
    if let (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) = (a, b) {
        let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
        Color::Rgb(m(ar, br), m(ag, bg), m(ab, bb))
    } else {
        a
    }
}

// ---------------------------------------------------------------------------
// Wire Protocol Spectrum Frame Decoder (T4.1 adaptation)
// ---------------------------------------------------------------------------

pub fn decode_spectrum_frame(buf: &[u8]) -> Option<SpectrumFrame> {
    if buf.len() < 21 {
        return None;
    }
    if buf[0] != 0x02 {
        return None;
    }
    let _dongle_idx = buf[1];
    let center_hz = f64::from_le_bytes(buf[2..10].try_into().ok()?);
    let span_hz = f64::from_le_bytes(buf[10..18].try_into().ok()?);
    let squelch_open = buf[18] != 0;
    let n = u16::from_le_bytes(buf[19..21].try_into().ok()?) as usize;
    if buf.len() < 21 + n * 4 {
        return None;
    }
    let mut bins_db = Vec::with_capacity(n);
    for idx in 0..n {
        let offset = 21 + idx * 4;
        let val = f32::from_le_bytes(buf[offset..offset + 4].try_into().ok()?);
        bins_db.push(val);
    }
    Some(SpectrumFrame {
        bins_db,
        center_hz,
        span_hz,
        squelch_open,
        ts: Instant::now(),
    })
}

// ---------------------------------------------------------------------------
// Demo swept-peak generator
// ---------------------------------------------------------------------------

pub fn make_swept_peak_frame(
    t: f64,
    fft_size: usize,
    center_hz: f64,
    span_hz: f64,
    tuned_hz: f64,
) -> SpectrumFrame {
    let sweep_freq = 0.05; // Hz, slow sweep
    let sweep_frac = 0.5 + 0.35 * (2.0 * std::f64::consts::PI * sweep_freq * t).sin();
    let peak_bin = (sweep_frac * fft_size as f64) as usize;

    let floor = -90.0f32;
    let peak_val = -30.0f32;
    let mut bins_db = Vec::with_capacity(fft_size);

    for i in 0..fft_size {
        // pseudo-random noise floor
        let rand_noise = (((i as f64 * 123.45).sin() * 1000.0).fract() * 2.0) as f32;
        let noise = floor + rand_noise;

        let dist = i as f32 - peak_bin as f32;
        let sigma = 8.0f32;
        let gaussian = ((dist * dist) / (-2.0 * sigma * sigma)).exp();

        bins_db.push(noise + (peak_val - floor) * gaussian);
    }

    let peak_hz = center_hz - span_hz / 2.0 + sweep_frac * span_hz;
    let dist_hz = (peak_hz - tuned_hz).abs();
    let squelch_open = dist_hz < (span_hz * 0.05);

    SpectrumFrame {
        bins_db,
        center_hz,
        span_hz,
        squelch_open,
        ts: Instant::now(),
    }
}

#[cfg(test)]
pub mod tests;
