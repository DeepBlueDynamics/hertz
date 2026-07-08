use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::broadcast::{AudioBroadcaster, AudioMessage};

const TRANSCRIBE_URL: &str = "http://localhost:8765";
const MODEL: &str = "large-v3";
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_POLL_ATTEMPTS: u32 = 120; // 4 minutes max

/// Spawn a background thread to transcribe a WAV file.
/// After transcription completes, broadcasts the result.
pub fn submit_transcription(
    wav_path: &Path,
    broadcaster: Arc<AudioBroadcaster>,
    channel: Option<u8>,
    freq: u32,
) {
    let path = wav_path.to_path_buf();
    thread::spawn(move || {
        if let Err(e) = transcribe_and_broadcast(&path, &broadcaster, channel, freq) {
            eprintln!("Transcription error: {}", e);
        }
    });
}

fn transcribe_and_broadcast(
    wav_path: &Path,
    broadcaster: &AudioBroadcaster,
    channel: Option<u8>,
    freq: u32,
) -> Result<(), String> {
    // Read the WAV file
    let wav_data =
        std::fs::read(wav_path).map_err(|e| format!("Failed to read WAV file: {}", e))?;

    // Generate job ID from timestamp + nanosecond bits
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let job_id = format!("{:x}{:04x}", now.as_millis(), now.subsec_nanos() & 0xFFFF);

    // Build multipart/form-data body
    let boundary = format!("----MeridianRadio{}", job_id);
    let filename = wav_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("recording.wav");

    let mut body = Vec::new();

    // File part
    write_part_header(&mut body, &boundary, "file", Some(filename), "audio/wav");
    body.extend_from_slice(&wav_data);
    body.extend_from_slice(b"\r\n");

    // job_id part
    write_part_header(&mut body, &boundary, "job_id", None, "");
    body.extend_from_slice(job_id.as_bytes());
    body.extend_from_slice(b"\r\n");

    // model part
    write_part_header(&mut body, &boundary, "model", None, "");
    body.extend_from_slice(MODEL.as_bytes());
    body.extend_from_slice(b"\r\n");

    // Close boundary
    body.extend_from_slice(format!("--{}--\r\n", boundary).as_bytes());

    // Submit transcription
    let submit_url = format!("{}/transcribe", TRANSCRIBE_URL);
    let content_type = format!("multipart/form-data; boundary={}", boundary);

    let resp = ureq::post(&submit_url)
        .set("Content-Type", &content_type)
        .set("User-Agent", "meridian-radio/1.0")
        .send_bytes(&body)
        .map_err(|e| format!("Failed to submit transcription: {}", e))?;

    if resp.status() != 200 {
        let status = resp.status();
        let body = resp
            .into_string()
            .unwrap_or_else(|_| "unknown".to_string());
        return Err(format!(
            "Transcription submit failed ({}): {}",
            status, body
        ));
    }

    println!(
        "Transcription submitted: job_id={} file={}",
        job_id,
        wav_path.display()
    );

    // Poll for completion
    let status_url = format!("{}/status/{}", TRANSCRIBE_URL, job_id);
    for attempt in 0..MAX_POLL_ATTEMPTS {
        thread::sleep(POLL_INTERVAL);

        let resp = ureq::get(&status_url)
            .call()
            .map_err(|e| format!("Failed to poll status: {}", e))?;

        let resp_text = resp
            .into_string()
            .map_err(|e| format!("Failed to read status response: {}", e))?;

        let status: serde_json::Value = serde_json::from_str(&resp_text)
            .map_err(|e| format!("Failed to parse status JSON: {}", e))?;

        match status["status"].as_str() {
            Some("completed") => {
                // Download transcript
                let download_url = format!("{}/download/{}", TRANSCRIBE_URL, job_id);
                let resp = ureq::get(&download_url)
                    .call()
                    .map_err(|e| format!("Failed to download transcript: {}", e))?;

                let transcript = resp
                    .into_string()
                    .map_err(|e| format!("Failed to read transcript: {}", e))?;

                let transcript = transcript.trim().to_string();
                if !transcript.is_empty() {
                    let preview = if transcript.len() > 80 {
                        &transcript[..80]
                    } else {
                        &transcript
                    };
                    println!("Transcription complete: \"{}\"", preview);

                    broadcaster.broadcast(AudioMessage::Transcription {
                        channel,
                        freq,
                        text: transcript,
                    });
                }
                return Ok(());
            }
            Some("failed") => {
                let err = status["error"].as_str().unwrap_or("unknown error");
                return Err(format!("Transcription failed: {}", err));
            }
            Some("queued") | Some("processing") => {
                if let Some(progress) = status["progress"].as_f64() {
                    if progress > 0.0 && attempt % 5 == 0 {
                        println!("Transcription progress: {:.0}%", progress * 100.0);
                    }
                }
            }
            other => {
                eprintln!("Unknown transcription status: {:?}", other);
            }
        }
    }

    Err("Transcription timed out after 4 minutes".to_string())
}

fn write_part_header(
    body: &mut Vec<u8>,
    boundary: &str,
    name: &str,
    filename: Option<&str>,
    content_type: &str,
) {
    body.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
    if let Some(fname) = filename {
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
                name, fname
            )
            .as_bytes(),
        );
        body.extend_from_slice(format!("Content-Type: {}\r\n", content_type).as_bytes());
    } else {
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{}\"\r\n", name).as_bytes(),
        );
    }
    body.extend_from_slice(b"\r\n");
}
