/// Generalized Visualization Module
/// Provides ASCII art visualization for debugging and analysis
/// Can be extended to generate image files later
use std::fs::File;
use std::io::Write;

/// Visualization configuration
pub struct VizConfig {
    pub enabled: bool,
    pub output_dir: String,
    pub width: usize,
    pub height: usize,
}

impl Default for VizConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            output_dir: "viz".to_string(),
            width: 80,
            height: 24,
        }
    }
}

/// Render a small grid/matrix (e.g., 3x3 floats)
#[allow(dead_code)]
pub fn render_grid(data: &[Vec<f32>], label: &str, config: &VizConfig) -> String {
    if !config.enabled {
        return String::new();
    }

    let mut output = format!("\n=== {} ===\n", label);

    // Find min/max for normalization
    let mut min_val = f32::MAX;
    let mut max_val = f32::MIN;
    for row in data {
        for &val in row {
            min_val = min_val.min(val);
            max_val = max_val.max(val);
        }
    }

    // Render grid with ASCII intensity
    let chars = " .:-=+*#%@";
    for row in data {
        output.push_str("│");
        for &val in row {
            // Normalize to 0-1
            let normalized = if max_val > min_val {
                (val - min_val) / (max_val - min_val)
            } else {
                0.5
            };
            let idx = ((normalized * (chars.len() - 1) as f32) as usize).min(chars.len() - 1);
            let ch = chars.chars().nth(idx).unwrap();
            output.push_str(&format!(" {:7.3}{} ", val, ch));
        }
        output.push_str("│\n");
    }

    output.push_str(&format!("Range: [{:.3}, {:.3}]\n", min_val, max_val));
    output
}

/// Render a waveform (time series data)
pub fn render_waveform(data: &[f32], label: &str, config: &VizConfig) -> String {
    if !config.enabled {
        return String::new();
    }

    let mut output = format!("\n=== {} ===\n", label);

    if data.is_empty() {
        output.push_str("(empty)\n");
        return output;
    }

    // Downsample to fit width
    let samples_per_col = (data.len() as f32 / config.width as f32).ceil() as usize;
    let mut column_values: Vec<f32> = Vec::new();
    for chunk in data.chunks(samples_per_col.max(1)) {
        let center_idx = chunk.len() / 2;
        column_values.push(
            chunk
                .get(center_idx)
                .copied()
                .unwrap_or(*chunk.last().unwrap_or(&0.0)),
        );
    }

    // Determine global scale using 95th percentile of absolute column values
    let mut sorted = column_values.clone();
    sorted.sort_by(|a, b| {
        a.abs()
            .partial_cmp(&b.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let scale_idx = ((sorted.len() as f32) * 0.95).floor() as usize;
    let scale = sorted
        .get(scale_idx)
        .map(|v| v.abs())
        .unwrap_or(1.0)
        .max(1e-6);
    let normalized_cols: Vec<f32> = column_values
        .iter()
        .map(|v| (v / scale).clamp(-1.0, 1.0))
        .collect();

    // Draw waveform
    for row_idx in (0..config.height).rev() {
        let threshold = -1.0 + 2.0 * (row_idx as f32 / config.height as f32);
        let half_step = 1.0 / config.height as f32;
        output.push('│');

        for &norm in &normalized_cols {
            let row_bottom = threshold - half_step;
            let row_top = threshold + half_step;
            if norm > row_bottom && norm <= row_top {
                output.push('█');
            } else {
                output.push(' ');
            }
        }

        output.push('│');
        output.push(' ');
        if threshold.abs() < 1e-3 {
            output.push('0');
        } else {
            let scaled = threshold * 100.0;
            let rounded = scaled.round() / 100.0;
            output.push_str(&format!("{:+.2}", rounded));
        }
        output.push('\n');
    }

    output.push_str("└");
    output.push_str(&"─".repeat(normalized_cols.len()));
    output.push_str("┘\n");
    output.push_str(&format!(
        "Samples: {} | Normalized (95th pct)\n",
        data.len()
    ));

    output
}

/// Render a spectrum (frequency domain)
pub fn render_spectrum(
    freqs: &[f32],
    magnitudes: &[f32],
    label: &str,
    config: &VizConfig,
) -> String {
    if !config.enabled {
        return String::new();
    }

    let mut output = format!("\n=== {} ===\n", label);

    if magnitudes.is_empty() {
        output.push_str("(empty)\n");
        return output;
    }

    // Find max
    let max_val = magnitudes.iter().copied().fold(f32::MIN, f32::max);

    // Downsample to fit width
    let bins_per_col = (magnitudes.len() as f32 / config.width as f32).ceil() as usize;
    let mut display_data = Vec::new();
    let mut display_freqs = Vec::new();

    for (idx, chunk) in magnitudes.chunks(bins_per_col.max(1)).enumerate() {
        // Take max of chunk (show peaks)
        let max = chunk.iter().copied().fold(f32::MIN, f32::max);
        display_data.push(max);

        // Get corresponding frequency
        if idx * bins_per_col < freqs.len() {
            display_freqs.push(freqs[idx * bins_per_col]);
        }
    }

    // Draw spectrum bars
    for row_idx in (0..config.height).rev() {
        let threshold = max_val * (row_idx as f32 / config.height as f32);
        output.push_str("│");

        for &val in &display_data {
            if val >= threshold {
                output.push('█');
            } else {
                output.push(' ');
            }
        }

        output.push_str(&format!("│ {:6.1} dB\n", threshold));
    }

    output.push_str("└");
    output.push_str(&"─".repeat(display_data.len()));
    output.push_str("┘\n");

    // Show frequency labels
    output.push_str(" ");
    if !display_freqs.is_empty() {
        let first_freq = display_freqs[0];
        let last_freq = *display_freqs.last().unwrap();
        output.push_str(&format!("{:.2} kHz", first_freq / 1000.0));
        let padding = display_data.len().saturating_sub(20);
        output.push_str(&" ".repeat(padding));
        output.push_str(&format!("{:.2} kHz", last_freq / 1000.0));
    }
    output.push('\n');

    output
}

/// Save visualization to file
#[allow(dead_code)]
pub fn save_viz(content: &str, filename: &str, config: &VizConfig) -> std::io::Result<()> {
    if !config.enabled {
        return Ok(());
    }

    std::fs::create_dir_all(&config.output_dir)?;
    let path = format!("{}/{}", config.output_dir, filename);
    let mut file = File::create(path)?;
    file.write_all(content.as_bytes())?;
    Ok(())
}

/// Print visualization to console
pub fn print_viz(content: &str, config: &VizConfig) {
    if config.enabled && !content.is_empty() {
        print!("{}", content);
    }
}

/// Render a spectrogram (frequency over time) with noise gating
/// history: Vec of (frequencies, magnitudes) tuples from past FFTs
pub fn render_spectrogram(
    history: &Vec<(Vec<f32>, Vec<f32>)>,
    label: &str,
    config: &VizConfig,
) -> String {
    if !config.enabled || history.is_empty() {
        return String::new();
    }

    let mut output = format!("\n=== {} ===\n", label);

    // Each column is a time slice, each row is a frequency bin
    let time_slices = history.len().min(config.width);
    if time_slices == 0 {
        output.push_str("(no data)\n");
        return output;
    }

    // Get frequency bins from first slice (assume all same)
    let freq_bins = history[0].1.len();
    let bins_per_row = (freq_bins as f32 / config.height as f32).ceil() as usize;

    // NOISE GATING: Find noise floor (lowest 10th percentile)
    let mut all_mags: Vec<f32> = Vec::new();
    for (_freqs, mags) in history.iter() {
        all_mags.extend(mags);
    }
    all_mags.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let noise_floor = if all_mags.len() > 10 {
        all_mags[all_mags.len() / 10] // 10th percentile
    } else {
        -80.0
    };

    // Find max above noise floor for scaling
    let mut max_above_floor = noise_floor;
    for &mag in &all_mags {
        if mag > noise_floor {
            max_above_floor = max_above_floor.max(mag);
        }
    }
    let scale_den = (max_above_floor - noise_floor).max(1e-3);

    // Render spectrogram: rows = frequency bins, columns = time
    let shade_ramp: [char; 10] = [' ', '.', ':', '-', '=', '+', '*', '#', '%', '@'];
    for row_idx in (0..config.height).rev() {
        output.push('│');

        // Take last N time slices
        let start_idx = if history.len() > time_slices {
            history.len() - time_slices
        } else {
            0
        };
        for slice_idx in start_idx..history.len() {
            let (_freqs, mags) = &history[slice_idx];

            // Average bins for this row
            let bin_start = row_idx * bins_per_row;
            let bin_end = (bin_start + bins_per_row).min(mags.len());
            if bin_start >= mags.len() {
                output.push(' ');
                continue;
            }

            let avg_mag =
                mags[bin_start..bin_end].iter().sum::<f32>() / (bin_end - bin_start) as f32;

            // NOISE GATE: Only show if above noise floor
            if avg_mag <= noise_floor + 2.0 {
                output.push(' '); // Below noise floor = blank
                continue;
            }

            // Normalize relative to noise floor and max
            let normalized = ((avg_mag - noise_floor) / scale_den).clamp(0.0, 1.0);
            let idx = ((normalized * (shade_ramp.len() as f32 - 1.0)).round() as usize)
                .min(shade_ramp.len() - 1);
            output.push(shade_ramp[idx]);
        }

        // Show frequency label for this row
        if row_idx == config.height - 1 && !history[0].0.is_empty() {
            let bin_idx = row_idx * bins_per_row;
            if bin_idx < history[0].0.len() {
                output.push_str(&format!(" │ {:.1} kHz\n", history[0].0[bin_idx] / 1000.0));
            } else {
                output.push_str(" │\n");
            }
        } else {
            output.push_str(" │\n");
        }
    }

    output.push('└');
    output.push_str(&"─".repeat(time_slices));
    output.push_str("┘\n");
    output.push_str(&format!(
        " ← {} time slices (noise floor: {:.1} dB) →\n",
        time_slices, noise_floor
    ));

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grid_viz() {
        let config = VizConfig {
            enabled: true,
            ..Default::default()
        };
        let data = vec![
            vec![1.0, 2.0, 3.0],
            vec![4.0, 5.0, 6.0],
            vec![7.0, 8.0, 9.0],
        ];
        let output = render_grid(&data, "Test Grid", &config);
        assert!(output.contains("Test Grid"));
        assert!(output.contains("1.000"));
        assert!(output.contains("9.000"));
    }

    #[test]
    fn test_waveform_viz() {
        let config = VizConfig {
            enabled: true,
            width: 40,
            height: 10,
            ..Default::default()
        };
        let data: Vec<f32> = (0..100).map(|i| (i as f32 * 0.1).sin()).collect();
        let output = render_waveform(&data, "Sine Wave", &config);
        assert!(output.contains("Sine Wave"));
        assert!(output.contains("Samples: 100"));
    }

    #[test]
    fn test_spectrum_viz() {
        let config = VizConfig {
            enabled: true,
            width: 40,
            height: 15,
            ..Default::default()
        };
        let freqs: Vec<f32> = (0..100).map(|i| i as f32 * 1000.0).collect();
        let mags: Vec<f32> = (0..100)
            .map(|i| (50.0 - (i as f32 - 50.0).abs()) * 0.5)
            .collect();
        let output = render_spectrum(&freqs, &mags, "Test Spectrum", &config);
        assert!(output.contains("Test Spectrum"));
        assert!(output.contains("kHz"));
    }
}
