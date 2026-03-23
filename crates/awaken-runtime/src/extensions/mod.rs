//! Bundled extensions for the awaken runtime.
//!
//! - [`a2a`]: Agent-to-Agent delegation via A2A protocol
//! - [`a2ui`]: Agent-to-UI declarative UI messages
//! - [`background`]: Background task management
//! - [`handoff`]: Dynamic same-thread agent switching

pub mod a2a;
pub mod a2ui;
pub mod background;
pub mod handoff;
