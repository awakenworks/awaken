//! `awaken-ext-memory` — cross-session memory as a bounded context.
//!
//! This crate owns the *persistence* half of memory, independent of any host: a
//! file-backed [`store`] of durable memories, the [`tool`] the extractor uses to
//! write them, the extractor [`agent`]'s config and prompts, and bounded [`recall`]
//! of saved memories into a new conversation's context.
//!
//! It deliberately does NOT own the aux-agent substrate (running the extractor) or
//! the triggers — those are composition-root (host) concerns. The host wires this
//! crate's pieces onto its `run_configured_subrun` / background machinery.
//!
//! Memory is distinct from context compaction (`awaken-ext-compact`): memory is
//! cross-session persistence (extract → store → recall), compaction is
//! within-session window management. They share only the aux-agent substrate.

pub mod agent;
pub mod recall;
pub mod select;
pub mod store;
pub mod tool;

pub use agent::{
    DEFAULT_MEMORY_INSTRUCTIONS, EXTRACT_PROMPT, MEMORY_AGENT_ID, default_memory_agent,
};
pub use recall::{RecallBounds, recall_block, recall_relevant};
pub use select::select_relevant;
pub use store::{MemoryStore, sanitize_stem};
pub use tool::{WriteMemoryTool, write_memory_descriptor};
