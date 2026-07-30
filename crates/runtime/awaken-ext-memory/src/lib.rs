//! `awaken-ext-memory` — cross-session memory as a bounded context.
//!
//! This crate owns Memory extension semantics independent of any host: extractor
//! and selector [`agent`] configuration, the [`tool`] used to propose writes,
//! bounded [`recall`], and the durable [`extraction`] aggregate plus repository
//! port. Concrete content and intent stores remain injected adapters.
//!
//! It deliberately does not own a concrete Agent executor, content store, or
//! scheduler. Those are neutral ports composed by an embedding Runtime or service.
//!
//! Memory is distinct from context compaction (`awaken-ext-compact`): memory is
//! cross-session persistence (extract → recall), compaction is within-session window
//! management. They share only the aux-agent substrate.

pub mod agent;
pub mod consolidation;
pub mod extraction;
pub mod localfs;
pub mod plugin;
pub mod recall;
pub mod select;
pub mod tool;

/// Prefix reserved for request-only recalled context. Extraction excludes these
/// messages because they are not new Thread facts to memorize again.
pub const RECALL_MESSAGE_ID_PREFIX: &str = "mem-recall";

pub use plugin::{
    MEMORY_PLUGIN_ID, MemoryConfig, MemoryPlugin, MemoryRecall,
    config_schema as memory_config_schema,
};

pub use agent::{
    DEFAULT_MEMORY_INSTRUCTIONS, DEFAULT_SELECTOR_INSTRUCTIONS, EXTRACT_PROMPT, MEMORY_AGENT_ID,
    SELECTOR_AGENT_ID, default_memory_agent, default_selector_agent,
};
pub use consolidation::{
    MemoryConsolidationJobRecord, MemoryConsolidationRepository,
    MemoryConsolidationRepositoryError, WorkspaceMemoryConsolidatorOverride,
};
pub use extraction::{
    MemoryExtractionController, MemoryExtractionDriver, MemoryExtractionError,
    MemoryExtractionIntent, MemoryExtractionMutation, MemoryExtractionPolicy,
    MemoryExtractionReceipt, MemoryExtractionRepository, MemoryExtractionStatus,
    MemoryExtractorSnapshot, MemoryMutationReceipt, MemoryTerminalExtraction,
    MemoryTerminalExtractionRequest, MemoryTerminalObserver, PutMemoryExtractionOutcome,
};
pub use localfs::{Entry, MemoryDir, MemoryStoreHandle, sanitize_stem};
pub use recall::{RecallBounds, recall_block, recall_relevant};
pub use select::{RecallSelector, parse_indices, select_input, select_relevant};
pub use tool::{WriteMemoryTool, accepts_memory_content, write_memory_descriptor};
