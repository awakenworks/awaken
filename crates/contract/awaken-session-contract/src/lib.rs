//! `awaken-session-contract` — the neutral session-runtime ports and vocabulary.
//!
//! The seam between the Managed Agents wire adapter (`awaken-protocol-managed`) and
//! the service layer that implements it (`awaken-runtime-host`): the ports a host
//! implements (session runtime, work queue, MCP probe, agent-config source, session
//! repository) plus the neutral vocabulary in their signatures. Dependencies point
//! inward — this is a `contract/` leaf, so the host and other implementors depend on
//! it instead of reverse-depending on a protocol adapter.
//!
//! Extraction is incremental: ports move here one family at a time, with
//! `awaken-protocol-managed` re-exporting each via a shim so consumers stay unchanged
//! until they are flipped to depend on this crate directly.

mod agent_config;
mod mcp_probe;

pub use agent_config::{AgentConfigSource, AgentConfigView};
pub use mcp_probe::{McpProbe, McpProbeStatus};
