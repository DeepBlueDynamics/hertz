//! Second-opinion transcription. After a call has been relayed, re-transcribe the
//! recording on the Whisper transcription service (when it is up) and ask ollaya
//! whether the result says something the on-device transcript missed.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use crate::ollaya::OllayaClient;

pub const DEFAULT_URL: &str = "http://localhost:8765";
pub const DEFAULT_MODEL: &str = "medium";

/// How long to wait for the service to finish one recording.
const JOB_TIMEOUT: Duration = Duration::from_secs(120);
const POLL_EVERY: Duration = Duration::from_millis(500);

/// Client for the transcription service (`/health`, `/transcribe`, `/status/{id}`,
/// `/download/{id}`).
pub struct TranscriptionService {
    url: String,
    model: String,
}

impl TranscriptionService {
    pub fn new(url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            model: model.into(),
        }
    }

    /// True when the service answers `/health` with a loaded model.
    pub fn is_up(&self) -> bool {
        ureq::get(&format!("{}/health", self.url))
            .timeout(Duration::from_secs(2))
            .call()
            .ok()
            .and_then(|r| r.into_json::<serde_json::Value>().ok())
            .is_some_and(|v| v["model_loaded"].as_bool() == Some(true))
    }

    /// Transcribe one WAV; returns the text (empty if the service heard no speech).
    pub fn transcribe(&self, wav: &Path) -> Result<String> {
        let audio = std::fs::read(wav).with_context(|| format!("read {wav:?}"))?;
        let job_id = format!(
            "hertz{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let filename = wav
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "recording.wav".into());
        let boundary = format!("----hertz{job_id}");
        let mut body = Vec::with_capacity(audio.len() + 512);
        let mut part = |name: &str, file: Option<&str>, data: &[u8]| {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match file {
                Some(f) => body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\nContent-Type: audio/wav\r\n\r\n"
                    )
                    .as_bytes(),
                ),
                None => body.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
                ),
            }
            body.extend_from_slice(data);
            body.extend_from_slice(b"\r\n");
        };
        part("file", Some(&filename), &audio);
        part("job_id", None, job_id.as_bytes());
        part("model", None, self.model.as_bytes());
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

        ureq::post(&format!("{}/transcribe", self.url))
            .set(
                "content-type",
                &format!("multipart/form-data; boundary={boundary}"),
            )
            .send_bytes(&body)
            .map_err(|e| anyhow!("submit to {}: {e}", self.url))?;

        let started = Instant::now();
        loop {
            let status: serde_json::Value = ureq::get(&format!("{}/status/{job_id}", self.url))
                .call()
                .map_err(|e| anyhow!("status {job_id}: {e}"))?
                .into_json()?;
            match status["status"].as_str() {
                Some("completed") => break,
                Some("failed") => {
                    return Err(anyhow!(
                        "transcription failed: {}",
                        status["error"].as_str().unwrap_or("unknown")
                    ))
                }
                _ if started.elapsed() > JOB_TIMEOUT => {
                    return Err(anyhow!("transcription timed out after {JOB_TIMEOUT:?}"))
                }
                _ => std::thread::sleep(POLL_EVERY),
            }
        }
        let report = ureq::get(&format!("{}/download/{job_id}", self.url))
            .call()
            .map_err(|e| anyhow!("download {job_id}: {e}"))?
            .into_string()?;
        Ok(report_text(&report))
    }
}

/// Pull the transcript out of the service's report: the `NNNN [t0 - t1] text`
/// lines under "TELEGRAPH COPY FOLLOWS". No such lines means no speech.
pub fn report_text(report: &str) -> String {
    report
        .lines()
        .filter_map(|l| {
            let (num, rest) = l.split_once(' ')?;
            if num.len() != 4 || !num.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let rest = rest.trim_start().strip_prefix('[')?;
            Some(rest.split_once(']')?.1.trim())
        })
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Does `second` say something `first` doesn't? Identical wording (ignoring case
/// and punctuation) short-circuits to "no". Otherwise ollaya judges the word
/// difference: shown the full sentences it reads both as "the same message", so it
/// is given only the words each side heard that the other didn't (6/6 on a labeled
/// set of real radio calls vs 4/6 with the sentences). Returns the verdict and
/// ollaya's probability for its choice.
pub fn adds_information(
    ollaya: &OllayaClient,
    first: &str,
    second: &str,
    min_probability: f64,
) -> Result<(bool, f64)> {
    let a = words(first);
    let b = words(second);
    if b.is_empty() || a == b {
        return Ok((false, 1.0));
    }
    let only = |x: &[String], y: &[String]| {
        let v: Vec<&str> = x
            .iter()
            .filter(|w| !y.contains(w))
            .map(String::as_str)
            .collect();
        if v.is_empty() {
            "(none)".to_string()
        } else {
            v.join(", ")
        }
    };
    let mut criteria = BTreeMap::new();
    criteria.insert(
        "new".to_string(),
        "The server heard real words that the on-device transcript got wrong or missed: a call sign, name, place, number, or instruction.".to_string(),
    );
    criteria.insert(
        "same".to_string(),
        "The differences are trivial: the same sounds spelled differently, repeated words, or filler words.".to_string(),
    );
    let mut questions = BTreeMap::new();
    questions.insert("compare".to_string(), criteria);
    let state = serde_json::Value::String(format!(
        "Words only the server heard: {}. Words only the on-device transcript had: {}.",
        only(&b, &a),
        only(&a, &b)
    ));
    let answer = ollaya
        .choose(&state, &questions)?
        .remove("compare")
        .expect("choose returns every asked question");
    let p = answer
        .probabilities
        .get(&answer.choice)
        .copied()
        .unwrap_or(answer.confidence);
    Ok((answer.choice == "new" && p >= min_probability, p))
}

/// Lowercased alphanumeric words (apostrophes kept: "you're").
fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_telegraph_report() {
        let report = "========== TRANSMISSION STATUS REPORT ==========\nSTATUS: TRANSCRIPTION COMPLETE\n------------------------------------------------------------\nTELEGRAPH COPY FOLLOWS\n0001 [0000.00s - 3.00s] Alpha India, this is Alpha Tango.\n0002 [0003.00s - 4.50s] Come in, over.\nEND OF TRANSMISSION STOP\n";
        assert_eq!(
            report_text(report),
            "Alpha India, this is Alpha Tango. Come in, over."
        );
        assert_eq!(
            report_text("TELEGRAPH COPY FOLLOWS\nEND OF TRANSMISSION STOP"),
            ""
        );
    }
}
