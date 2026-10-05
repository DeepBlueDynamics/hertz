//! Hertz ↔ Hyperia ↔ ollaya.
//!
//! - [`hyperia`]: read the live window → tab → pane layout from a running Hyperia
//!   (its MCP endpoint, `terminal_status`).
//! - [`ollaya`]: ask the ollaya decision model multiple-choice questions about a
//!   state (e.g. which pane a radio call addresses, or which pane is active).
//! - [`route`]: radio transcript → the Hyperia pane ollaya says it addresses.
//! - [`verify`]: second-opinion transcription and an ollaya "did it add anything" check.

pub mod hyperia;
pub mod ollaya;
pub mod route;
pub mod verify;
