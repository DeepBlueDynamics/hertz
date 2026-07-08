use std::io::{self, Write};
use std::net::TcpStream;

use serde::Serialize;

#[derive(Serialize)]
struct StreamFrame<'a> {
    source: &'a str,
    samples: Vec<f32>,
}

pub fn send_stream_samples(
    stream: &mut TcpStream,
    samples: &[f32],
    source: &str,
) -> io::Result<()> {
    if samples.is_empty() {
        return Ok(());
    }

    let max_abs = samples
        .iter()
        .fold(0.0f32, |acc, &v| acc.max(v.abs()))
        .max(1e-6);
    let scale = if max_abs <= 1.0 { 1.0 } else { 1.0 / max_abs };

    const CHUNK: usize = 512;
    let mut idx = 0;
    while idx < samples.len() {
        let end = (idx + CHUNK).min(samples.len());
        let frame_samples: Vec<f32> = samples[idx..end]
            .iter()
            .map(|&sample| (sample * scale).clamp(-1.0, 1.0))
            .collect();

        let frame = StreamFrame {
            source,
            samples: frame_samples,
        };

        let mut buf =
            serde_json::to_vec(&frame).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        buf.push(b'\n');
        stream.write_all(&buf)?;
        idx = end;
    }

    Ok(())
}
