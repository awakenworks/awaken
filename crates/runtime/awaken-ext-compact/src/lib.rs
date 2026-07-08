//! `awaken-ext-compact` — within-session context compaction as a bounded context.
//!
//! Compaction summarizes the older part of a long conversation so future turns
//! carry a short summary instead of the full history. This crate owns the
//! compactor [`agent`]'s config and prompts and the pure [`fold`] policy (which
//! prefix to summarize); the host wires them onto its aux-agent substrate and
//! pairs the summary with a `ContextPolicy::KeepLast` window.
//!
//! Compaction is a *different* concern from memory (`awaken-ext-memory`): memory
//! is cross-session persistence, compaction is within-session window management.
//! They share only the aux-agent substrate.

pub mod agent;
pub mod config;
pub mod fold;
pub mod plugin;

pub use agent::{
    COMPACT_AGENT_ID, DEFAULT_COMPACT_INSTRUCTIONS, SUMMARIZE_PROMPT, default_compact_agent,
};
pub use config::CompactConfig;
pub use fold::fold_point;
pub use plugin::{
    COMPACT_PLUGIN_ID, CompactPlugin, Summarizer, compaction_count,
    config_schema as compact_config_schema,
};
