//! Unified umbrella crate for the tirea agent framework.
//!
//! Use feature flags to control which modules are included:
//!
//! | Feature    | What it enables                                  |
//! |------------|--------------------------------------------------|
//! | `core`     | AgentOS composition + runtime (**default**)       |
//! | `ag-ui`    | AG-UI protocol adapters                          |
//! | `ai-sdk-v6`| Vercel AI SDK v6 protocol adapters               |
//! | `mcp`      | MCP tool registry integration                    |
//! | `postgres` | PostgreSQL thread store backend                  |
//! | `nats`     | NATS JetStream thread store backend              |
//! | `full`     | All of the above                                 |
//!
//! # Quick start
//!
//! ```toml
//! [dependencies]
//! tirea = { version = "0.1", features = ["ag-ui"] }
//! ```
//!
//! ```ignore
//! use tirea::prelude::*;
//! ```

// ── Always available: contracts + state ─────────────────────────────────

/// Core agent contracts: traits, data models, events, and tool/plugin SPI.
pub use tirea_contract as contracts;

/// Typed JSON state engine (patches, ops, paths, state manager).
pub use tirea_state as state;

// ── Core (default): AgentOS composition + runtime ───────────────────────

/// AgentOS composition layer (builder, registries, agent definitions).
#[cfg(feature = "core")]
pub use tirea_agentos::composition;

/// AgentOS runtime layer (run preparation, execution, active-run coordination).
#[cfg(feature = "core")]
pub use tirea_agentos::runtime;

/// AgentOS extensions (permission, reminder, interaction, etc.).
#[cfg(feature = "core")]
pub use tirea_agentos::extensions;

// ── Protocols ───────────────────────────────────────────────────────────

/// AG-UI protocol types, adapters, and runtime wiring.
#[cfg(feature = "ag-ui")]
pub use tirea_protocol_ag_ui as ag_ui;

/// Vercel AI SDK v6 protocol types and adapters.
#[cfg(feature = "ai-sdk-v6")]
pub use tirea_protocol_ai_sdk_v6 as ai_sdk_v6;

// ── Extensions ────────────────────────────────────────────────────────

/// Skill subsystem (discovery, activation, resource loading, scripts).
#[cfg(feature = "skills")]
pub use tirea_extension_skills as skills;

// ── Storage backends ────────────────────────────────────────────────────

/// Thread store adapters (file, memory, postgres, nats).
#[cfg(any(feature = "postgres", feature = "nats"))]
pub use tirea_store_adapters as store;

// ── Prelude ─────────────────────────────────────────────────────────────

pub mod prelude;
