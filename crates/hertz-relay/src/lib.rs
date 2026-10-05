//! Hertz ↔ Hyperia ↔ ollaya.
//!
//! - [`hyperia`]: read the live window → tab → pane layout from a running Hyperia
//!   (its MCP endpoint, `terminal_status`).
//! - [`ollaya`]: ask the ollaya decision model multiple-choice questions about a
//!   state (e.g. which pane a radio call addresses, or which pane is active).

pub mod hyperia;
pub mod ollaya;
