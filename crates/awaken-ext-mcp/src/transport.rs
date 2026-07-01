//! MCP tool transport abstraction.
//!
//! `McpToolTransport` is the seam [`McpRawTool`](crate::tool::McpRawTool) calls
//! and that a fake stands in for under test. The concrete wire transports
//! (stdio, HTTP/SSE) that speak the protocol over the `mcp` SDK — plus the
//! notification, sampling, and progress channels — land in later phases; the
//! method set grows additively as they do.

use async_trait::async_trait;
use mcp::transport::McpTransportError;
use mcp::{CallToolResult, McpToolDefinition};
use serde_json::Value;

/// Raw MCP client transport: the wire operations `McpRawTool` needs to expose an
/// external server's tools as runtime tools.
#[async_trait]
pub trait McpToolTransport: Send + Sync {
    /// Discover the server's tools (`tools/list`).
    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError>;

    /// Invoke one tool (`tools/call`). A returned [`CallToolResult`] with
    /// `is_error` set is a *tool* error (model-visible, run continues); an `Err`
    /// is a *transport* error (aborts the call). This three-state distinction is
    /// mapped to the neutral result in [`McpRawTool`](crate::tool::McpRawTool).
    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<CallToolResult, McpTransportError>;
}
