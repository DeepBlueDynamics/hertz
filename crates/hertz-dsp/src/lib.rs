//! Hertz DSP library — a pure, hardware-free port of the gnosis-radio signal chain.
//!
//! This crate contains the channelizer, NFM/AM demodulators, entropy (spectral-flatness)
//! squelch with hang/prebuffer/fades, AFC, a WAV recorder, and an RF-noise entropy pool.
//! It performs **no I/O and no logging** in its hot path. The top-level [`Pipeline`]
//! consumes complex baseband IQ and returns a stream of [`PipelineEvent`] values; the
//! daemon (a separate crate) wires those events to its event bus, recorder, and logs.
//!
//! See `plan/PLAN.md` §11 (Phase 2) and `plan/tasks/T2-sloth-dsp.md`.

pub mod analysis;
pub mod channelizer;
pub mod classify;
pub mod demod;
pub mod entropy;
pub mod pipeline;
pub mod recorder;
pub mod testutil;
pub mod voice_out;

pub use demod::Mode;
pub use pipeline::{AudioKind, Pipeline, PipelineConfig, PipelineEvent, TransmissionSummary};

/// Re-exported for convenience; the complex sample type used throughout the crate.
pub use num_complex::Complex32;
