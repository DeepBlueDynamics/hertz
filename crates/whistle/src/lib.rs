//! Rust bindings for [Cactus Whistle](https://cactuscompute.com/blog/whistle),
//! a 16.9 MB on-device speech recognizer (en de fr es it nl pl) that runs on
//! the needle engine.
//!
//! ```no_run
//! let whistle = whistle::Whistle::load_default()?;
//! let pcm = whistle::wav::read_16k_mono("clip.wav")?;
//! let t = whistle.transcribe(&pcm, &whistle::Options::default())?;
//! println!("{} [{:?}]", t.text, t.language);
//! # Ok::<(), whistle::Error>(())
//! ```
//!
//! The engine holds one speech model per process and is not thread-safe, so
//! [`Whistle`] is a cheap, cloneable handle around a process-global, mutex-
//! guarded engine.

use std::ffi::{c_char, c_int, CString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use serde::{Deserialize, Serialize};

#[cfg(feature = "fetch")]
pub mod fetch;
mod resample;
mod sys;
#[cfg(feature = "wav")]
pub mod wav;

pub use resample::resample;

/// The model's input rate.
pub const SAMPLE_RATE: u32 = 16_000;
/// The longest clip one call accepts: 30 s.
pub const MAX_SAMPLES: usize = 30 * SAMPLE_RATE as usize;
/// The needle engine release these bindings were written against.
pub const ENGINE_VERSION: &str = "3.3.1";

const OUT_CAPACITY: usize = 1 << 18;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("engine library: {0}")]
    Library(String),
    #[error("engine: {0}")]
    Engine(String),
    #[error("{0} is not a speech model (.cact holds no Whistle weights)")]
    NotSpeech(PathBuf),
    #[error("a different model is already loaded in this process: {0}")]
    AlreadyLoaded(PathBuf),
    #[error("clip is {0} samples; at most {MAX_SAMPLES} (30 s at 16 kHz) per call")]
    TooLong(usize),
    #[error("no {0} found: set {1} or enable the `fetch` feature")]
    NotFound(&'static str, &'static str),
    #[error("unknown language {0:?}: one of en de fr es it nl pl")]
    Language(String),
    #[error("bad engine output: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[cfg(feature = "wav")]
    #[error("wav: {0}")]
    Wav(#[from] hound::Error),
    #[cfg(feature = "fetch")]
    #[error("download: {0}")]
    Download(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The seven languages Whistle transcribes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Language {
    En,
    De,
    Fr,
    Es,
    It,
    Nl,
    Pl,
}

impl Language {
    pub const ALL: [Language; 7] = [
        Self::En,
        Self::De,
        Self::Fr,
        Self::Es,
        Self::It,
        Self::Nl,
        Self::Pl,
    ];

    pub fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::De => "de",
            Self::Fr => "fr",
            Self::Es => "es",
            Self::It => "it",
            Self::Nl => "nl",
            Self::Pl => "pl",
        }
    }
}

impl FromStr for Language {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|l| l.code().eq_ignore_ascii_case(s))
            .ok_or_else(|| Error::Language(s.to_owned()))
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// Per-call decoding options.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Force a language; `None` detects it.
    pub language: Option<Language>,
    /// Words and phrases to bias toward (Aho-Corasick keyword biasing), e.g.
    /// vessel names, callsigns, "channel sixteen".
    pub keywords: Vec<String>,
    /// Return per-word start/end times and probabilities.
    pub word_timestamps: bool,
}

/// One transcribed word, times in seconds from the start of the clip.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub word: String,
    pub start: f32,
    pub end: f32,
    pub probability: f32,
}

/// A transcription. Silence and steady noise give empty `text` and no language.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    pub text: String,
    pub language: Option<String>,
    pub ttft_ms: f32,
    pub decode_tps: f32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<Word>,
}

impl Transcript {
    /// True when the engine heard no speech.
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

#[derive(Deserialize)]
struct RawTranscript {
    text: String,
    #[serde(default)]
    language: String,
    #[serde(default)]
    ttft_ms: f32,
    #[serde(default)]
    decode_tps: f32,
    #[serde(default)]
    words: Vec<Word>,
}

/// One incremental update from live streaming transcription.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamUpdate {
    pub text: String,
    pub words: Vec<Word>,
    pub pending: String,
    pub language: Option<String>,
    pub received: f32,
    pub pass_ms: f32,
}

#[derive(Deserialize)]
struct RawStreamUpdate {
    #[serde(default)]
    text: String,
    #[serde(default)]
    words: Vec<Word>,
    #[serde(default)]
    pending: String,
    #[serde(default)]
    language: String,
    #[serde(default)]
    received: f32,
    #[serde(default)]
    pass_ms: f32,
}

struct Engine {
    lib: sys::Lib,
    weights: PathBuf,
    // Held for the life of the process in case the engine reads it in place.
    _cact: Vec<u8>,
    out: Vec<u8>,
}

/// Handle to the process-global Whistle model. `Clone`, `Send` and `Sync`;
/// calls are serialised on an internal mutex.
#[derive(Clone)]
pub struct Whistle {
    engine: Arc<Mutex<Engine>>,
}

static GLOBAL: OnceLock<Mutex<Option<Whistle>>> = OnceLock::new();

impl Whistle {
    /// Load the engine library at `lib` and the `.cact` weights at `weights`.
    /// The engine cannot unload, so a second call with the same weights
    /// returns the existing handle and one with different weights fails.
    pub fn load(lib: impl AsRef<Path>, weights: impl AsRef<Path>) -> Result<Self> {
        let weights = weights.as_ref().to_path_buf();
        let mut slot = GLOBAL
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = slot.as_ref() {
            let loaded = existing.weights();
            return if same_file(&loaded, &weights) {
                Ok(existing.clone())
            } else {
                Err(Error::AlreadyLoaded(loaded))
            };
        }
        let lib = sys::Lib::open(lib.as_ref())?;
        let cact = std::fs::read(&weights)?;
        // SAFETY: pointer/length describe `cact`, which outlives the engine.
        if unsafe { (lib.load)(cact.as_ptr(), cact.len() as u64) } < 0 {
            return Err(Error::Engine(lib.error("needle_load failed")));
        }
        if unsafe { (lib.models)() } & sys::NEEDLE_SPEECH == 0 {
            return Err(Error::NotSpeech(weights));
        }
        let whistle = Whistle {
            engine: Arc::new(Mutex::new(Engine {
                lib,
                weights,
                _cact: cact,
                out: vec![0; OUT_CAPACITY],
            })),
        };
        *slot = Some(whistle.clone());
        Ok(whistle)
    }

    /// Load from `WHISTLE_LIB_PATH` / `NEEDLE3_LIB_PATH` and `WHISTLE_WEIGHTS` /
    /// `NEEDLE_WHISTLE_WEIGHTS`, falling back to the local cache and, with the
    /// `fetch` feature, downloading from Hugging Face on first use.
    pub fn load_default() -> Result<Self> {
        Self::load(default_lib()?, default_weights()?)
    }

    /// Path of the loaded `.cact`.
    pub fn weights(&self) -> PathBuf {
        self.lock().weights.clone()
    }

    fn lock(&self) -> MutexGuard<'_, Engine> {
        self.engine.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Transcribe up to 30 s of 16 kHz mono PCM in [-1, 1].
    pub fn transcribe(&self, pcm: &[f32], opts: &Options) -> Result<Transcript> {
        if pcm.len() > MAX_SAMPLES {
            return Err(Error::TooLong(pcm.len()));
        }
        let language = opts.language.map(|l| CString::new(l.code()).unwrap());
        let keywords = (!opts.keywords.is_empty())
            .then(|| CString::new(opts.keywords.join("\n").replace('\0', "")).unwrap());
        let silence = [0.0f32; 1];
        let samples = if pcm.is_empty() { &silence[..] } else { pcm };

        let mut engine = self.lock();
        let Engine { lib, out, .. } = &mut *engine;
        out.fill(0);
        // SAFETY: every pointer is valid for the stated length for the call.
        let code = unsafe {
            (lib.transcribe)(
                samples.as_ptr(),
                pcm.len() as c_int,
                language.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                keywords.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                opts.word_timestamps as c_int,
                out.as_mut_ptr() as *mut c_char,
                out.len() as c_int,
            )
        };
        if code < 0 {
            return Err(Error::Engine(lib.error("needle_transcribe failed")));
        }
        let end = out.iter().position(|&b| b == 0).unwrap_or(out.len());
        let raw: RawTranscript = serde_json::from_slice(&out[..end])?;
        Ok(Transcript {
            text: raw.text,
            language: (!raw.language.is_empty()).then_some(raw.language),
            ttft_ms: raw.ttft_ms,
            decode_tps: raw.decode_tps,
            words: raw.words,
        })
    }

    /// Transcribe a clip of any length in back-to-back 30 s windows, joining
    /// the text and shifting word times onto the full clip's timeline.
    pub fn transcribe_long(&self, pcm: &[f32], opts: &Options) -> Result<Transcript> {
        if pcm.len() <= MAX_SAMPLES {
            return self.transcribe(pcm, opts);
        }
        let mut all = Transcript {
            text: String::new(),
            language: None,
            ttft_ms: 0.0,
            decode_tps: 0.0,
            words: Vec::new(),
        };
        let mut tps = Vec::new();
        for (i, chunk) in pcm.chunks(MAX_SAMPLES).enumerate() {
            let t = self.transcribe(chunk, opts)?;
            let offset = (i * MAX_SAMPLES) as f32 / SAMPLE_RATE as f32;
            if i == 0 {
                all.ttft_ms = t.ttft_ms;
            }
            if !t.is_empty() {
                if !all.text.is_empty() {
                    all.text.push(' ');
                }
                all.text.push_str(t.text.trim());
                all.language = all.language.or(t.language);
                tps.push(t.decode_tps);
            }
            all.words.extend(t.words.into_iter().map(|w| Word {
                start: w.start + offset,
                end: w.end + offset,
                ..w
            }));
        }
        all.decode_tps = if tps.is_empty() {
            0.0
        } else {
            tps.iter().sum::<f32>() / tps.len() as f32
        };
        Ok(all)
    }

    /// Feed a chunk of 16 kHz mono float PCM (typically ~1 s) to the live streaming
    /// recognizer. Returns words newly committed by this pass and the unconfirmed pending tail.
    pub fn stream_process(&self, pcm: &[f32], opts: &Options) -> Result<StreamUpdate> {
        let language = opts.language.map(|l| CString::new(l.code()).unwrap());
        let keywords = (!opts.keywords.is_empty())
            .then(|| CString::new(opts.keywords.join("\n").replace('\0', "")).unwrap());
        let silence = [0.0f32; 1];
        let samples = if pcm.is_empty() { &silence[..] } else { pcm };

        let mut engine = self.lock();
        let Engine { lib, out, .. } = &mut *engine;
        out.fill(0);
        let code = unsafe {
            (lib.stream_process)(
                samples.as_ptr(),
                pcm.len() as c_int,
                language.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                keywords.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                out.as_mut_ptr() as *mut c_char,
                out.len() as c_int,
            )
        };
        if code < 0 {
            return Err(Error::Engine(
                lib.error("needle_stream_transcribe_process failed"),
            ));
        }
        let end = out.iter().position(|&b| b == 0).unwrap_or(out.len());
        let raw: RawStreamUpdate = serde_json::from_slice(&out[..end])?;
        Ok(StreamUpdate {
            text: raw.text,
            words: raw.words,
            pending: raw.pending,
            language: (!raw.language.is_empty()).then_some(raw.language),
            received: raw.received,
            pass_ms: raw.pass_ms,
        })
    }

    /// End the live streaming transcription. The pending tail becomes the committed
    /// words of the returned update. The next `stream_process` call starts a new stream.
    pub fn stream_stop(&self) -> Result<StreamUpdate> {
        let mut engine = self.lock();
        let Engine { lib, out, .. } = &mut *engine;
        out.fill(0);
        let code =
            unsafe { (lib.stream_stop)(out.as_mut_ptr() as *mut c_char, out.len() as c_int) };
        if code < 0 {
            return Err(Error::Engine(
                lib.error("needle_stream_transcribe_stop failed"),
            ));
        }
        let end = out.iter().position(|&b| b == 0).unwrap_or(out.len());
        let raw: RawStreamUpdate = serde_json::from_slice(&out[..end])?;
        Ok(StreamUpdate {
            text: raw.text,
            words: raw.words,
            pending: raw.pending,
            language: (!raw.language.is_empty()).then_some(raw.language),
            received: raw.received,
            pass_ms: raw.pass_ms,
        })
    }

    /// Encoder embedding: one row per 80 ms frame, flattened row-major.
    /// Use [`Whistle::embed_dim`] to split rows.
    pub fn embed(&self, pcm: &[f32]) -> Result<Vec<f32>> {
        if pcm.len() > MAX_SAMPLES {
            return Err(Error::TooLong(pcm.len()));
        }
        let engine = self.lock();
        let lib = &engine.lib;
        // SAFETY: a null output asks only for the float count.
        let size = unsafe {
            (lib.embed)(
                std::ptr::null(),
                pcm.as_ptr(),
                pcm.len() as c_int,
                std::ptr::null_mut(),
                0,
            )
        };
        if size < 0 {
            return Err(Error::Engine(lib.error("needle_embed failed")));
        }
        let mut out = vec![0.0f32; size as usize];
        let got = unsafe {
            (lib.embed)(
                std::ptr::null(),
                pcm.as_ptr(),
                pcm.len() as c_int,
                out.as_mut_ptr(),
                size,
            )
        };
        if got != size {
            return Err(Error::Engine(lib.error("needle_embed failed")));
        }
        Ok(out)
    }

    /// Floats per 80 ms frame in [`Whistle::embed`] output.
    pub fn embed_dim(&self) -> Result<usize> {
        // 80 ms of silence is exactly one frame.
        Ok(self.embed(&[0.0; 1280])?.len())
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn env_path(names: &[&str]) -> Option<PathBuf> {
    names
        .iter()
        .find_map(std::env::var_os)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Directory for the engine library and weights: `WHISTLE_CACHE`, else
/// `%LOCALAPPDATA%\whistle`, `$XDG_CACHE_HOME/whistle` or `~/.cache/whistle`.
pub fn cache_dir() -> PathBuf {
    if let Some(dir) = env_path(&["WHISTLE_CACHE"]) {
        return dir;
    }
    let base = if cfg!(windows) {
        env_path(&["LOCALAPPDATA"])
    } else {
        env_path(&["XDG_CACHE_HOME"]).or_else(|| env_path(&["HOME"]).map(|h| h.join(".cache")))
    };
    base.unwrap_or_else(std::env::temp_dir).join("whistle")
}

/// File name of the engine shared library on this platform.
pub fn lib_file_name() -> &'static str {
    if cfg!(windows) {
        "libneedle3.dll"
    } else if cfg!(target_os = "macos") {
        "libneedle3.dylib"
    } else {
        "libneedle3.so"
    }
}

fn default_lib() -> Result<PathBuf> {
    if let Some(p) = env_path(&["WHISTLE_LIB_PATH", "NEEDLE3_LIB_PATH"]) {
        return Ok(p);
    }
    let cached = cache_dir().join(ENGINE_VERSION).join(lib_file_name());
    if cached.exists() {
        return Ok(cached);
    }
    #[cfg(feature = "fetch")]
    return fetch::engine(&cache_dir());
    #[cfg(not(feature = "fetch"))]
    Err(Error::NotFound("engine library", "WHISTLE_LIB_PATH"))
}

fn default_weights() -> Result<PathBuf> {
    if let Some(p) = env_path(&["WHISTLE_WEIGHTS", "NEEDLE_WHISTLE_WEIGHTS"]) {
        return Ok(p);
    }
    let cached = cache_dir().join("whistle.cact");
    if cached.exists() {
        return Ok(cached);
    }
    #[cfg(feature = "fetch")]
    return fetch::weights(&cache_dir());
    #[cfg(not(feature = "fetch"))]
    Err(Error::NotFound("whistle.cact", "WHISTLE_WEIGHTS"))
}
