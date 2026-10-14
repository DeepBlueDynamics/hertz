//! cargo run --release --example stream -- clip.wav [--lang en] [--keywords "Sea Wolf,channel sixteen"]

use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut path = None;
    let mut opts = whistle::Options::default();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--lang" => opts.language = Some(args.next().ok_or("--lang needs a value")?.parse()?),
            "--keywords" => {
                let list = args.next().ok_or("--keywords needs a value")?;
                opts.keywords = list
                    .split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            _ => path = Some(a),
        }
    }
    let path = path.ok_or("usage: stream <clip.wav> [--lang xx] [--keywords a,b]")?;

    let t0 = Instant::now();
    let whistle = whistle::Whistle::load_default()?;
    eprintln!(
        "loaded {} in {:?}",
        whistle.weights().display(),
        t0.elapsed()
    );

    let pcm = whistle::wav::read_16k_mono(&path)?;
    eprintln!(
        "read {:.2} s of audio",
        pcm.len() as f32 / whistle::SAMPLE_RATE as f32
    );

    let chunk_size = whistle::SAMPLE_RATE as usize; // 1 s chunks at 16 kHz
    let mut stream_text = String::new();

    println!("--- Streaming transcription (1 s chunks) ---");
    let t_stream = Instant::now();
    for (i, chunk) in pcm.chunks(chunk_size).enumerate() {
        let update = whistle.stream_process(chunk, &opts)?;
        if !update.text.is_empty() {
            if !stream_text.is_empty() {
                stream_text.push(' ');
            }
            stream_text.push_str(update.text.trim());
        }
        println!(
            "chunk {:2}: committed=\"{}\" pending=\"{}\" (received={:.2}s pass={:.1}ms)",
            i + 1,
            update.text,
            update.pending,
            update.received,
            update.pass_ms,
        );
    }

    let final_update = whistle.stream_stop()?;
    if !final_update.text.is_empty() {
        if !stream_text.is_empty() {
            stream_text.push(' ');
        }
        stream_text.push_str(final_update.text.trim());
    }
    println!(
        "stop    : committed=\"{}\" (pass={:.1}ms)",
        final_update.text, final_update.pass_ms,
    );
    eprintln!("streaming finished in {:?}", t_stream.elapsed());
    println!("Full stream transcript: \"{}\"", stream_text);

    println!("\n--- Batch transcribe() comparison ---");
    let t_batch = Instant::now();
    let batch = whistle.transcribe_long(&pcm, &opts)?;
    eprintln!("batch finished in {:?}", t_batch.elapsed());
    println!("Full batch transcript : \"{}\"", batch.text);

    Ok(())
}
