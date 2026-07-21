//! `awaken-ext-memory` — cross-session memory as a bounded context.
//!
//! This crate owns the *local-filesystem* half of memory, independent of any host
//! and unaware of any store: a [`localfs`] directory of memory files (`<slug>.md`),
//! the [`tool`] the extractor uses to write one, the extractor [`agent`]'s config
//! and prompts, and bounded [`recall`] of saved memories into a new conversation's
//! context. It only ever touches a local directory — *durability* (surviving a
//! restart, id-keyed addressing) is a resources-plane concern (`awaken-memory-store`)
//! the host wires the directory to; the runtime stays store-unaware.
//!
//! It deliberately does NOT own the aux-agent substrate (running the extractor) or
//! the triggers — those are composition-root (host) concerns. The host wires this
//! crate's pieces onto its `run_configured_subrun` / background machinery.
//!
//! Memory is distinct from context compaction (`awaken-ext-compact`): memory is
//! cross-session persistence (extract → recall), compaction is within-session window
//! management. They share only the aux-agent substrate.

pub mod agent;
pub mod localfs;
pub mod plugin;
pub mod recall;
pub mod select;
pub mod tool;

pub use plugin::{
    MEMORY_PLUGIN_ID, MemoryConfig, MemoryPlugin, config_schema as memory_config_schema,
};

pub use agent::{
    DEFAULT_MEMORY_INSTRUCTIONS, DEFAULT_SELECTOR_INSTRUCTIONS, EXTRACT_PROMPT, MEMORY_AGENT_ID,
    SELECTOR_AGENT_ID, default_memory_agent, default_selector_agent,
};
pub use localfs::{Entry, MemoryDir, MemoryStoreHandle, sanitize_stem};
pub use recall::{RecallBounds, recall_block, recall_relevant};
pub use select::{RecallSelector, parse_indices, select_input, select_relevant};
pub use tool::{WriteMemoryTool, write_memory_descriptor};
