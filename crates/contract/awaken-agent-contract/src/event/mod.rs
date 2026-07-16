//! The one neutral event vocabulary and its routing (ADR-0058).
//!
//! [`AgentEvent`] is the single shape every producer emits and every protocol
//! projects from; [`classify`] is the single routing truth that says which channels
//! carry each event. The message log stays canonical truth — this is the read/emit
//! projection over it (Axis 1).

pub mod agent;
pub mod classify;

pub use agent::{AgentEvent, Committed, Live, ToolDisposition};
pub use classify::{Routing, Tier, classify};
