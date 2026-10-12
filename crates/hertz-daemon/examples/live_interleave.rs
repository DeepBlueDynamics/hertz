//! Does a batch transcription in the middle of a live stream disturb it?
//! `cargo run --release -p hertz-daemon --example live_interleave -- long.wav other.wav`
fn run(w: &whistle::Whistle, pcm: &[f32], other: Option<&[f32]>) -> anyhow::Result<String> {
    let opts = whistle::Options::default();
    let mut text = Vec::new();
    for (i, chunk) in pcm.chunks(16_000).enumerate() {
        if i == 3 {
            if let Some(o) = other {
                let t = w.transcribe_long(o, &opts)?;
                println!("  (batch in the middle: {:?})", t.text);
            }
        }
        let u = w.stream_process(chunk, &opts)?;
        if !u.text.trim().is_empty() {
            text.push(u.text.trim().to_string());
        }
    }
    let u = w.stream_stop()?;
    text.push(u.text.trim().to_string());
    Ok(text.join(" "))
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let w = whistle::Whistle::load_default()?;
    let a = whistle::wav::read_16k_mono(&args[0])?;
    let b = whistle::wav::read_16k_mono(&args[1])?;
    println!("clean:       {}", run(&w, &a, None)?);
    println!("interleaved: {}", run(&w, &a, Some(&b))?);
    Ok(())
}
