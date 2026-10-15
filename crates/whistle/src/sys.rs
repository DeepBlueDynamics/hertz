//! Raw needle engine C ABI (`needle.h`, engine 3.3.1), loaded at runtime.
//!
//! The engine ships as a self-contained shared library (`libneedle3.dll` /
//! `.so` / `.dylib`, UCRT-only on Windows) inside the `cactus-needle` wheels.
//! The per-platform `libneedle.a` archives on Hugging Face are built with a
//! clang/Itanium C++ ABI and do not link into `x86_64-pc-windows-msvc`, so we
//! load the shared library instead of linking statically.

use std::ffi::{c_char, c_int, CStr};
use std::path::Path;

use crate::Error;

#[allow(dead_code)]
pub const NEEDLE_TEXT: c_int = 1;
pub const NEEDLE_SPEECH: c_int = 2;

type LoadFn = unsafe extern "C" fn(*const u8, u64) -> c_int;
type ModelsFn = unsafe extern "C" fn() -> c_int;
type LastErrorFn = unsafe extern "C" fn() -> *const c_char;
type TranscribeFn = unsafe extern "C" fn(
    *const f32,
    c_int,
    *const c_char,
    *const c_char,
    c_int,
    *mut c_char,
    c_int,
) -> c_int;
type StreamProcessFn = unsafe extern "C" fn(
    *const f32,
    c_int,
    *const c_char,
    *const c_char,
    *mut c_char,
    c_int,
) -> c_int;
type StreamStopFn = unsafe extern "C" fn(*mut c_char, c_int) -> c_int;
type EmbedFn = unsafe extern "C" fn(*const c_char, *const f32, c_int, *mut f32, c_int) -> c_int;

/// Function table resolved from the engine library. The `Library` is kept
/// alive for as long as the pointers are.
pub struct Lib {
    _lib: libloading::Library,
    pub load: LoadFn,
    pub models: ModelsFn,
    pub last_error: LastErrorFn,
    pub transcribe: TranscribeFn,
    pub stream_process: StreamProcessFn,
    pub stream_stop: StreamStopFn,
    pub embed: EmbedFn,
}

impl Lib {
    pub fn open(path: &Path) -> Result<Self, Error> {
        let lib_err = |e: libloading::Error| Error::Library(format!("{}: {e}", path.display()));
        // SAFETY: loading a library runs its initialisers; the needle engine
        // has no global constructors with preconditions.
        let lib = unsafe { libloading::Library::new(path) }.map_err(lib_err)?;
        // SAFETY: the signatures match needle.h for engine 3.3.1.
        unsafe {
            let load = *lib.get::<LoadFn>(b"needle_load\0").map_err(lib_err)?;
            let models = *lib.get::<ModelsFn>(b"needle_models\0").map_err(lib_err)?;
            let last_error = *lib
                .get::<LastErrorFn>(b"needle_last_error\0")
                .map_err(lib_err)?;
            let transcribe = *lib
                .get::<TranscribeFn>(b"needle_transcribe\0")
                .map_err(lib_err)?;
            let stream_process = *lib
                .get::<StreamProcessFn>(b"needle_stream_transcribe_process\0")
                .map_err(lib_err)?;
            let stream_stop = *lib
                .get::<StreamStopFn>(b"needle_stream_transcribe_stop\0")
                .map_err(lib_err)?;
            let embed = *lib.get::<EmbedFn>(b"needle_embed\0").map_err(lib_err)?;
            Ok(Self {
                _lib: lib,
                load,
                models,
                last_error,
                transcribe,
                stream_process,
                stream_stop,
                embed,
            })
        }
    }

    /// The engine's last error message, or `fallback` when it has none.
    pub fn error(&self, fallback: &str) -> String {
        // SAFETY: returns a runtime-owned C string valid until the next call.
        let ptr = unsafe { (self.last_error)() };
        if ptr.is_null() {
            return fallback.to_owned();
        }
        let msg = unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned();
        if msg.is_empty() {
            fallback.to_owned()
        } else {
            msg
        }
    }
}
