use base64::Engine;
use serde::Deserialize;

use crate::agentic::{PaintRegion, SharedAgenticState, VoicePaintingData};
use crate::broadcast::{AudioBroadcaster, AudioMessage};
use std::sync::Arc;

#[derive(Deserialize)]
struct ClaudeResponse {
    content: Vec<ClaudeContent>,
}

#[derive(Deserialize)]
struct ClaudeContent {
    text: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeParsed {
    description: Option<String>,
    regions: Option<Vec<ParsedRegion>>,
}

#[derive(Deserialize)]
struct ParsedRegion {
    label: Option<String>,
    freq_lo: Option<f32>,
    freq_hi: Option<f32>,
    time_start: Option<f32>,
    time_end: Option<f32>,
    color: Option<String>,
    opacity: Option<f32>,
    style: Option<String>,
}

const CLAUDE_API_URL: &str = "https://api.anthropic.com/v1/messages";
const CLAUDE_MODEL: &str = "claude-sonnet-4-20250514";

const PROMPT: &str = r##"Analyze this voice spectrogram image. The spectrogram shows the vocal range (300-3400 Hz on the Y axis, time scrolling left to right). Identify formant regions, harmonic series, fricatives, voice onset/offset transitions, and noise bands visible in the image.

Return ONLY valid JSON (no markdown fences) with this exact structure:
{
  "description": "1-2 sentence human-readable analysis of what you see",
  "regions": [
    {
      "label": "F1 Formant",
      "freq_lo": 300.0,
      "freq_hi": 800.0,
      "time_start": 0.0,
      "time_end": 5.0,
      "color": "#ff6600",
      "opacity": 0.3,
      "style": "fill"
    }
  ]
}

For regions, use these guidelines:
- label: descriptive name (F1 Formant, F2 Formant, Sibilance, Harmonic Series, Noise Band, Voice Onset, etc.)
- freq_lo/freq_hi: Hz values in 300-3400 range
- time_start/time_end: seconds from left edge (0-5 range for visible window)
- color: hex color string
- opacity: 0.1-0.5 range for overlays
- style: "fill", "outline", "gradient", or "glow"

If no voice activity is visible (just noise), return a description noting that and an empty regions array."##;

/// Trigger a voice painting analysis using the latest screenshot from AgenticState.
/// Sends the screenshot to Claude's vision API, parses the response, stores result,
/// and broadcasts to WebSocket clients.
pub fn trigger_voice_paint(
    state: SharedAgenticState,
    broadcaster: Arc<AudioBroadcaster>,
) -> Result<VoicePaintingData, String> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .map_err(|_| "ANTHROPIC_API_KEY environment variable not set".to_string())?;

    let screenshot_png = {
        let s = state.lock().map_err(|e| format!("Lock error: {}", e))?;
        s.screenshot_png
            .clone()
            .ok_or_else(|| "No screenshot available".to_string())?
    };

    let b64 = base64::engine::general_purpose::STANDARD.encode(&screenshot_png);

    let body = serde_json::json!({
        "model": CLAUDE_MODEL,
        "max_tokens": 1024,
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": b64
                    }
                },
                {
                    "type": "text",
                    "text": PROMPT
                }
            ]
        }]
    });

    let resp = ureq::post(CLAUDE_API_URL)
        .set("x-api-key", &api_key)
        .set("anthropic-version", "2023-06-01")
        .set("content-type", "application/json")
        .send_string(&body.to_string())
        .map_err(|e| format!("Claude API request failed: {}", e))?;

    let resp_text = resp
        .into_string()
        .map_err(|e| format!("Failed to read response: {}", e))?;

    let claude_resp: ClaudeResponse =
        serde_json::from_str(&resp_text).map_err(|e| format!("Failed to parse response: {}", e))?;

    let text = claude_resp
        .content
        .first()
        .and_then(|c| c.text.as_ref())
        .ok_or_else(|| "No text in Claude response".to_string())?;

    // Try to extract JSON from the response (handle possible markdown fences)
    let json_str = if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            &text[start..=end]
        } else {
            text.as_str()
        }
    } else {
        text.as_str()
    };

    let parsed: ClaudeParsed =
        serde_json::from_str(json_str).map_err(|e| format!("Failed to parse painting JSON: {}", e))?;

    let regions: Vec<PaintRegion> = parsed
        .regions
        .unwrap_or_default()
        .into_iter()
        .map(|r| PaintRegion {
            label: r.label.unwrap_or_else(|| "Unknown".to_string()),
            freq_lo: r.freq_lo.unwrap_or(300.0),
            freq_hi: r.freq_hi.unwrap_or(3400.0),
            time_start: r.time_start.unwrap_or(0.0),
            time_end: r.time_end.unwrap_or(5.0),
            color: r.color.unwrap_or_else(|| "#ff6600".to_string()),
            opacity: r.opacity.unwrap_or(0.3),
            style: r.style.unwrap_or_else(|| "fill".to_string()),
        })
        .collect();

    let painting = VoicePaintingData {
        description: parsed
            .description
            .unwrap_or_else(|| "No description available".to_string()),
        regions,
        timestamp: chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
    };

    // Store in agentic state
    {
        let mut s = state.lock().map_err(|e| format!("Lock error: {}", e))?;
        s.voice_painting = Some(painting.clone());
    }

    // Broadcast to WebSocket clients
    let json = serde_json::to_string(&painting).unwrap_or_default();
    broadcaster.broadcast(AudioMessage::VoicePaint { painting: json });

    Ok(painting)
}
