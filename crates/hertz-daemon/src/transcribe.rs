//! Transcription engines behind `trait Transcriber`, selected by config: a no-op
//! [`DisabledTranscriber`] or [`WhistleTranscriber`] (Cactus Whistle, on-device). On
//! `RecordingSaved` the recorder calls the transcriber via `spawn_blocking`; the
//! trait is sync on purpose (object-safe), and engines do their blocking work inside.

use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};

use tracing::{info, warn};

/// One transcription backend, selectable by config. Stored as `Box<dyn Transcriber>`.
pub trait Transcriber: Send + Sync {
    /// Transcribe `wav_path`; return the text, or empty string on no-op.
    fn transcribe(&self, wav_path: &Path) -> anyhow::Result<String>;

    /// Human-readable engine name (for `/api/status`-style reporting).
    fn name(&self) -> &'static str;
}

/// No-op transcriber used when transcription is disabled in config.
pub struct DisabledTranscriber;

impl Transcriber for DisabledTranscriber {
    fn transcribe(&self, _wav_path: &Path) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn name(&self) -> &'static str {
        "disabled"
    }
}

/// Cactus Whistle (`engine = "whistle"`), on-device via the `whistle` crate. The
/// engine and ~17 MB weights are fetched on first use; a background thread loads
/// them at boot so the first recording doesn't wait. The engine serializes calls.
pub struct WhistleTranscriber {
    options: whistle::Options,
    engine: OnceLock<whistle::Whistle>,
}

impl WhistleTranscriber {
    /// `language`: "auto" (detect) or a code Whistle knows ("en", "de", ...).
    /// `keywords`: call signs, vessel names and channel names to bias toward.
    pub fn spawn(language: &str, keywords: Vec<String>) -> Arc<Self> {
        let language = match language {
            "" | "auto" => None,
            code => match whistle::Language::from_str(code) {
                Ok(l) => Some(l),
                Err(e) => {
                    warn!("whistle: {e}; detecting language instead");
                    None
                }
            },
        };
        let t = Arc::new(Self {
            options: whistle::Options {
                language,
                keywords,
                word_timestamps: false,
            },
            engine: OnceLock::new(),
        });
        let preload = Arc::clone(&t);
        std::thread::Builder::new()
            .name("whistle-load".into())
            .spawn(move || {
                if let Err(e) = preload.engine() {
                    warn!("whistle preload failed: {e:#}");
                }
            })
            .expect("spawn whistle loader");
        t
    }

    fn engine(&self) -> anyhow::Result<&whistle::Whistle> {
        if let Some(w) = self.engine.get() {
            return Ok(w);
        }
        info!("whistle: loading (first run downloads the engine and weights)");
        let w = whistle::Whistle::load_default()?;
        info!("whistle: ready ({})", w.weights().display());
        // A concurrent loader may have won; load_default hands back the same engine.
        Ok(self.engine.get_or_init(|| w))
    }
}

impl Transcriber for WhistleTranscriber {
    fn transcribe(&self, wav_path: &Path) -> anyhow::Result<String> {
        let pcm = whistle::wav::read_16k_mono(wav_path)?;
        let t = self.engine()?.transcribe_long(&pcm, &self.options)?;
        Ok(t.text.trim().to_string())
    }
    fn name(&self) -> &'static str {
        "whistle"
    }
}
