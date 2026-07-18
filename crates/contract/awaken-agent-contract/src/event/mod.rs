//! The one neutral event vocabulary and its routing (ADR-0058).
//!
//! [`AgentEvent`] is the single shape every producer emits and every protocol
//! projects from; [`classify`] is the single routing truth that says which channels
//! carry each event. The message log stays canonical truth — this is the read/emit
//! projection over it (Axis 1).

pub mod agent;
pub mod classify;
pub mod fold;

pub use agent::{AgentEvent, Delta, Fact, ToolDisposition};
pub use classify::{Routing, Tier, classify};
pub use fold::{
    HistorySink, ToolUseRef, Transcoder, fold_history, fold_messages, fold_step, terminal,
    terminal_awaiting,
};
