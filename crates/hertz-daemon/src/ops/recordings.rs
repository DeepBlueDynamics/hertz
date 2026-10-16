//! Tools: recordings, recording (2 MB chunks). (Phase 1, Confidential Turkey.)

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::{args, ready, OpError, OpResult, Tool, CHUNK_BYTES, CONTENT_KEY};
use crate::runtime::DaemonState;

pub const TOOLS: &[Tool] = &[
    Tool {
        name: "recordings",
        description: "Recording files, newest first, filtered by `channel`, \
            `since_minutes` and `limit`. Each has its size and `chunks` (how \
            many 2 MB audio pieces it spans).",
        read_only: true,
        input_schema: recordings_schema,
        call: |s, a| ready(recordings(&s, a)),
        agent: None,
    },
    Tool {
        name: "recording",
        description: "One recording by file name: metadata, transcript and \
            `chunks`. With `chunk` N, the result also carries that 2 MB piece \
            of the WAV as a base64 audio content block.",
        read_only: true,
        input_schema: recording_schema,
        call: |s, a| ready(recording(&s, a)),
        agent: None,
    },
];

fn recordings_schema(_: &DaemonState) -> Value {
    json!({
        "type": "object",
        "properties": {
            "channel": { "type": "string", "description": "Only recordings on this channel id." },
            "since_minutes": { "type": "number", "description": "Only recordings this recent." },
            "limit": { "type": "integer", "minimum": 1, "description": "How many, newest first." }
        }
    })
}

fn recording_schema(_: &DaemonState) -> Value {
    json!({
        "type": "object",
        "properties": {
            "file": { "type": "string", "description": "A recording file name from `recordings`." },
            "chunk": { "type": "integer", "minimum": 0, "description": "Which 2 MB audio piece to include (0-based)." }
        },
        "required": ["file"]
    })
}

fn recordings_dir(state: &DaemonState) -> PathBuf {
    Path::new(&state.config.daemon.data_dir).join("recordings")
}

fn transcripts_dir(state: &DaemonState) -> PathBuf {
    Path::new(&state.config.daemon.data_dir).join("transcriptions")
}

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn wav_duration(path: &Path) -> Option<f32> {
    let reader = hound::WavReader::open(path).ok()?;
    let spec = reader.spec();
    let n = reader.into_samples::<i16>().count() as f32;
    Some(n / spec.sample_rate as f32)
}

/// Parse the transcript sidecar `transcriptions/<stem>.txt` into
/// (channel, freq_hz). Absent or unparseable -> (None, None).
fn transcript_meta(state: &DaemonState, stem: &str) -> (Option<String>, Option<u64>) {
    let path = transcripts_dir(state).join(format!("{stem}.txt"));
    let Ok(content) = std::fs::read_to_string(&path) else {
        return (None, None);
    };
    let mut channel = None;
    let mut freq_hz = None;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("CHANNEL: ") {
            let v = v.trim();
            if v != "-" && !v.is_empty() {
                channel = Some(v.to_string());
            }
        } else if let Some(v) = line.strip_prefix("FREQUENCY: ") {
            let mhz = v.trim().trim_end_matches(" MHz").parse::<f64>().ok();
            freq_hz = mhz.map(|m| (m * 1e6).round() as u64);
        }
    }
    (channel, freq_hz)
}

/// The transcript text for a recording (everything after the `---` separator),
/// or None when there is no transcript yet.
fn transcript_text(state: &DaemonState, stem: &str) -> Option<String> {
    let path = transcripts_dir(state).join(format!("{stem}.txt"));
    let content = std::fs::read_to_string(&path).ok()?;
    let text = content.split("\n---\n").nth(1)?;
    let text = text.trim().to_string();
    Some(text)
}

fn chunks_for(size: u64) -> u64 {
    size.div_ceil(CHUNK_BYTES)
}

/// `recordings`: the WAV files, newest first.
pub fn recordings(state: &DaemonState, a: Value) -> OpResult {
    #[derive(serde::Deserialize, Default)]
    struct Q {
        channel: Option<String>,
        since_minutes: Option<f64>,
        limit: Option<usize>,
    }
    let q: Q = args(a)?;
    let now = unix_now();
    let since = q.since_minutes.map(|m| now - m * 60.0);

    let mut out = Vec::new();
    // A missing recordings directory is an empty listing, not an error.
    let Ok(rd) = std::fs::read_dir(recordings_dir(state)) else {
        return Ok(json!({ "recordings": out, "count": 0 }));
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("wav") {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let filename = p.file_name().unwrap().to_string_lossy().to_string();
        let stem = p.file_stem().unwrap().to_string_lossy().to_string();
        let (channel, freq_hz) = transcript_meta(state, &stem);
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        if q.channel
            .as_ref()
            .is_some_and(|c| channel.as_deref() != Some(c.as_str()))
        {
            continue;
        }
        if since.is_some_and(|s| modified < s) {
            continue;
        }
        let size = meta.len();
        out.push(json!({
            "filename": filename,
            "channel": channel,
            "freq_hz": freq_hz.unwrap_or(0),
            "size_bytes": size,
            "modified_ts_sec": modified,
            "duration_sec": wav_duration(&p).unwrap_or(0.0),
            "chunks": chunks_for(size),
        }));
    }
    out.sort_by(|a, b| {
        b["modified_ts_sec"]
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&a["modified_ts_sec"].as_f64().unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if let Some(n) = q.limit {
        out.truncate(n);
    }
    Ok(json!({ "recordings": out, "count": out.len() }))
}

/// `recording`: metadata, transcript, chunk count, and optionally one audio piece.
pub fn recording(state: &DaemonState, a: Value) -> OpResult {
    #[derive(serde::Deserialize)]
    struct Q {
        file: String,
        chunk: Option<u64>,
    }
    let q: Q = args(a)?;

    // Same sanitize as the REST get_recording: a bare filename only.
    if q.file.contains('/') || q.file.contains("..") || q.file.contains('\\') {
        return Err(OpError::BadArgs(
            "file must be a bare recording file name, not a path".into(),
        ));
    }
    let path = recordings_dir(state).join(&q.file);
    if !path.is_file() {
        return Err(OpError::NotFound(format!(
            "no recording named {:?}; list them with recordings",
            q.file
        )));
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let (channel, freq_hz) = transcript_meta(state, &stem);
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let chunks = chunks_for(size);

    let mut out = json!({
        "file": q.file.clone(),
        "channel": channel,
        "freq_hz": freq_hz.unwrap_or(0),
        "size_bytes": size,
        "duration_sec": wav_duration(&path).unwrap_or(0.0),
        "transcript": transcript_text(state, &stem),
        "chunks": chunks,
    });

    if let Some(n) = q.chunk {
        if n >= chunks {
            return Err(OpError::BadArgs(format!(
                "chunk must be 0..{chunks} (this recording has {chunks} chunk(s))"
            )));
        }
        let start = (n * CHUNK_BYTES) as u64;
        let mut file = std::fs::File::open(&path)
            .map_err(|e| OpError::Unavailable(format!("can't read {}: {e}", q.file)))?;
        let mut bytes = vec![0u8; (CHUNK_BYTES as usize).min((size - start) as usize)];
        if !bytes.is_empty() {
            use std::io::Seek;
            file.seek(std::io::SeekFrom::Start(start))
                .map_err(|e| OpError::Unavailable(format!("can't seek {}: {e}", q.file)))?;
            file.read_exact(&mut bytes)
                .map_err(|e| OpError::Unavailable(format!("can't read {}: {e}", q.file)))?;
        }
        out[CONTENT_KEY] = json!([{
            "type": "audio",
            "mimeType": "audio/wav",
            "data": base64_encode(&bytes),
        }]);
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// base64 (no dependency; standard alphabet, padded)
// ---------------------------------------------------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}
