# whistle

Rust bindings for [Cactus Whistle](https://cactuscompute.com/blog/whistle), a 16.9 MB on-device speech recognizer (en, de, fr, es, it, nl, pl). It runs on the needle engine. Forest uses it to transcribe recorded transmissions in-process instead of POSTing them to a Whisper service.

## How it runs

Whistle has no engine of its own. It is a `.cact` weights file (`Cactus-Compute/whistle`) loaded into the needle 3 engine (`Cactus-Compute/needle3`, v3.3.1, Apache-2.0) through a plain C ABI (`needle.h`):

| C function | Rust |
|---|---|
| `needle_load(bytes, n)` | `Whistle::load` / `load_default` |
| `needle_models()` | checks that the `.cact` holds a speech model |
| `needle_transcribe(pcm, n, lang, keywords, word_ts, out, cap)` | `Whistle::transcribe` |
| `needle_stream_transcribe_process(pcm, n, lang, keywords, out, cap)` | `Whistle::stream_process` |
| `needle_stream_transcribe_stop(out, cap)` | `Whistle::stream_stop` |
| `needle_embed(NULL, pcm, n, out, cap)` | `Whistle::embed` |
| `needle_last_error()` | folded into `Error::Engine` |

**Why it loads at runtime instead of linking statically.** Hugging Face also ships `libneedle.a` per platform. The Windows archive is built with clang and the Itanium C++ ABI (`__gxx_personality`, `_Znwy`), so it won't link into `x86_64-pc-windows-msvc`. The shared library inside the `cactus-needle` wheel (`needle/libneedle3.dll`) imports only UCRT and kernel32, so the crate opens it with `libloading`. Linux and macOS use the same approach with `.so` and `.dylib`.

On first use, the `fetch` feature downloads the engine (pulled out of the wheel for the build target) and `whistle.cact` into `%LOCALAPPDATA%\whistle` (`~/.cache/whistle` on Unix). You can override any of these:

- `WHISTLE_LIB_PATH` or `NEEDLE3_LIB_PATH`: the engine library
- `WHISTLE_WEIGHTS` or `NEEDLE_WHISTLE_WEIGHTS`: the `.cact` file (for example, a fine-tuned one)
- `WHISTLE_CACHE`: the cache directory

For air-gapped boxes, build with `--no-default-features --features wav` and set the two paths.

## Use

```rust
let whistle = whistle::Whistle::load_default()?;          // once per process
let pcm = whistle::wav::read_16k_mono("tx.wav")?;          // any rate/channels -> 16 kHz mono f32
let opts = whistle::Options {
    language: Some(whistle::Language::En),                 // None = detect
    keywords: vec!["Sea Wolf".into(), "Point Bonita".into()], // callsigns, vessel names
    word_timestamps: true,
};
let t = whistle.transcribe_long(&pcm, &opts)?;             // transcribe() caps at 30 s
if !t.is_empty() { println!("{} ({:?})", t.text, t.language); }
```

The demod output already lives in memory, so skip the WAV. Pass `&[f32]` at 16 kHz, or convert with `whistle::resample(&audio_48k, 48_000, 16_000)`.

```
cargo run --release --example transcribe -- clip.wav --words --keywords "Sea Wolf,channel sixteen"
cargo test --release -- --ignored     # engine tests (download ~19 MB once)
```

## Live transcription

For streaming audio (e.g. live SDR audio feeds in ~1 s chunks), Whistle provides incremental decoding:

```rust
let whistle = whistle::Whistle::load_default()?;
let opts = whistle::Options {
    language: Some(whistle::Language::En),
    keywords: vec!["Sea Wolf".into()],
    ..Default::default()
};

// Feed 16 kHz mono float PCM in chunks of ~1 s:
for chunk in pcm_stream {
    let update = whistle.stream_process(&chunk, &opts)?;
    if !update.text.is_empty() {
        print!("{} ", update.text); // words committed by this pass
    }
    // update.pending has the unconfirmed trailing hypothesis
}

// Finalize when transmission ends (commits any remaining tail):
let final_update = whistle.stream_stop()?;
println!("{}", final_update.text);
```

```
cargo run --release --example stream -- clip.wav
```

## Contract and limits

- **One model per process. The engine is not thread-safe.** `Whistle` is `Clone + Send + Sync`, and calls are serialized on a mutex. Run ASR on one worker thread fed by a channel, not inline in the DSP loop.
- **Input:** 16 kHz mono f32 in [-1, 1], at most 30 s per `transcribe`. `transcribe_long` splits longer audio into back-to-back 30 s windows (no overlap, so a word on a boundary can be split).
- **Silence and steady noise** return empty `text` and `language: None`. That makes it a second-stage voice check after squelch: drop recordings that come back empty.
- **Keywords** turn on Aho-Corasick biasing. Feed it the bandplan's channel names, known vessel names and callsigns.
- **Output:** `text`, `language`, `ttft_ms` and `decode_tps`, plus `words` (`word`, `start`, `end`, `probability`) when `word_timestamps` is set. The struct derives `Serialize` and maps directly onto an `ASR_TRANSCRIBED` event.
- Weights stay loaded for the life of the process (the engine can't unload them). Loading a different `.cact` returns `Error::AlreadyLoaded`.

## Measured here

Windows 11 x86_64, CPU, release build, clean TTS mayday call at 48 kHz:

- Load: 3.4 s on the first run (includes the ~19 MB download), 18 ms once cached
- 11.1 s of audio: transcribed in 0.6–0.84 s (TTFT about 600 ms, about 260 tok/s)
- Transcript: exact, including "Sea Wolf", "Channel 16" and "Point Bonita"

Cactus's published figures (11 ms TTFT, 1,319 tok/s) are from an Apple M4 Pro. Accuracy on real NFM audio, which is band-limited to 300–3400 Hz and has static, is not measured yet.
