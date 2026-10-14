//! cargo run --release --example transcribe -- clip.wav [--lang en] [--keywords "Sea Wolf,channel sixteen"] [--words]

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
            "--words" => opts.word_timestamps = true,
            _ => path = Some(a),
        }
    }
    let path = path.ok_or("usage: transcribe <clip.wav> [--lang xx] [--keywords a,b] [--words]")?;

    let t0 = Instant::now();
    let whistle = whistle::Whistle::load_default()?;
    eprintln!(
        "loaded {} in {:?}",
        whistle.weights().display(),
        t0.elapsed()
    );

    let pcm = whistle::wav::read_16k_mono(&path)?;
    let t1 = Instant::now();
    let t = whistle.transcribe_long(&pcm, &opts)?;
    eprintln!(
        "{:.1} s of audio in {:?}",
        pcm.len() as f32 / 16_000.0,
        t1.elapsed()
    );
    println!("{}", serde_json::to_string_pretty(&t)?);
    Ok(())
}
