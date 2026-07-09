#![allow(warnings)]
//! `hertzd` — the Hertz daemon: owns dongles, DSP, storage, and the network surface.
//!
//! Scope of this crate (PLAN §11 Phase 3 / T4): monitor + channelized roles on axum
//! (REST + WS + SSE + `/audio` on one port). Hopscan, MCP, and the transcription
//! engines are later phases; only seams are provided here.

pub mod audio;
pub mod bus;
pub mod control;
pub mod factory;
pub mod history;
pub mod pipeline_bridge;
pub mod recorder;
pub mod runtime;
pub mod server;
pub mod spectrum;
pub mod transcribe;

pub use audio::AudioFrame;
pub use bus::EventBus;
pub use control::{DongleControl, DongleRuntimeHandle};
pub use factory::{MockFactory, RealFactory, SdrFactory};
pub use recorder::RecorderHandle;
pub use runtime::Daemon;
pub use transcribe::{DisabledTranscriber, Transcriber};
