//! Transcription seam (PLAN §6 / T4: leave the seam, no real engine yet).
//!
//! `trait Transcriber` + a no-op [`DisabledTranscriber`] selected by config. On
//! `RecordingSaved` the daemon calls the transcriber; Phase 6 fills in
//! whisper-internal / whisper-http / cloud. The trait is sync on purpose (kept
//! object-safe and dependency-free); the daemon runs it off the hot path via
//! `spawn_blocking`, and real engines do their blocking I/O inside.

use std::path::Path;

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
