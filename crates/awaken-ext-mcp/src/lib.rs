//! Model Context Protocol (MCP) client extension.
//!
//! Connects to external MCP servers and exposes their tools as awaken runtime
//! tools ([`RawTool`](awaken_runtime_contract::tool::RawTool)), namespaced
//! `mcp__<server>__<tool>`. The wire protocol is spoken through the `mcp` SDK,
//! which stays the anti-corruption boundary: only neutral runtime values
//! (`ToolOutput`, `ToolDescriptor`) cross out of this crate.
//!
//! This is an extension (never the kernel): like `awaken-ext-builtin-tools` it
//! depends only on `awaken-runtime-contract` and the host wires it in.
//!
//! # Phasing
//!
//! Full parity with the reference MCP crates (goal / awaken-next) is built in
//! phases: P0 (this) covers tool discovery/invocation and the error three-state;
//! later phases add the stdio/HTTP transports, the connection manager,
//! `tools/list_changed` refresh, sampling, progress, resources, prompts, and
//! credentials.

pub mod config;
pub mod error;
pub mod id_mapping;
pub mod tool;
pub mod transport;

pub use config::{McpServerConnectionConfig, TransportTypeId};
pub use error::McpError;
pub use id_mapping::to_tool_id;
pub use tool::{McpRawTool, mcp_tool_descriptor};
pub use transport::McpToolTransport;
