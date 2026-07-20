//! MCP server connection configuration types.
//!
//! Re-exports the shared wire configuration so callers do not depend on a
//! third-party MCP client SDK.

pub use awaken_mcp_wire::{McpServerConnectionConfig, TransportTypeId};
