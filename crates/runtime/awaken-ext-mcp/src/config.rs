//! MCP server connection configuration types.
//!
//! Re-exports [`McpServerConnectionConfig`] from the `mcp` crate, the
//! anti-corruption boundary for the wire protocol. Later phases add helpers for
//! building stdio and HTTP configurations.

pub use mcp::transport::{McpServerConnectionConfig, TransportTypeId};
