//! MCP tool transport abstraction.
//!
//! `McpToolTransport` is the seam [`McpRawTool`](crate::tool::McpRawTool) calls
//! and that a fake stands in for under test. The concrete wire transports
//! (stdio, HTTP/SSE) that speak the protocol over the `mcp` SDK — plus the
//! notification, sampling, and progress channels — land in later phases; the
//! method set grows additively as they do.

use std::collections::HashMap;

use async_trait::async_trait;
use mcp::transport::McpTransportError;
use mcp::{CallToolResult, McpToolDefinition};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::progress::McpProgressUpdate;
use crate::types::{McpPromptDefinition, McpPromptResult, McpResourceDefinition};

/// Which catalog a `notifications/*/list_changed` referred to. Consumed by the
/// dynamic-refresh path (a change advances the server's live tool version).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListChangedKind {
    Tools,
    Prompts,
    Resources,
}

/// Raw MCP client transport: the wire operations `McpRawTool` needs to expose an
/// external server's tools as runtime tools, plus the prompt/resource surfaces a
/// host may consult. Tools are mandatory; prompts and resources default to
/// "unsupported" so a tools-only transport (or a test fake) need not implement
/// them.
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

    /// Invoke a tool while streaming its progress to `progress_tx`. Defaults to
    /// a plain [`call_tool`](Self::call_tool) (no progress) for transports that
    /// do not support server notifications.
    async fn call_tool_with_progress(
        &self,
        tool_name: &str,
        arguments: Value,
        _progress_tx: mpsc::Sender<McpProgressUpdate>,
    ) -> Result<CallToolResult, McpTransportError> {
        self.call_tool(tool_name, arguments).await
    }

    /// List the server's prompts (`prompts/list`). Defaults to none.
    async fn list_prompts(&self) -> Result<Vec<McpPromptDefinition>, McpTransportError> {
        Ok(Vec::new())
    }

    /// Render a prompt (`prompts/get`). Defaults to unsupported.
    async fn get_prompt(
        &self,
        _name: &str,
        _arguments: Option<HashMap<String, String>>,
    ) -> Result<McpPromptResult, McpTransportError> {
        Err(McpTransportError::NotSupported("prompts/get".to_string()))
    }

    /// List the server's resources (`resources/list`). Defaults to none.
    async fn list_resources(&self) -> Result<Vec<McpResourceDefinition>, McpTransportError> {
        Ok(Vec::new())
    }

    /// Read a resource by uri (`resources/read`). Defaults to unsupported.
    async fn read_resource(&self, _uri: &str) -> Result<Value, McpTransportError> {
        Err(McpTransportError::NotSupported(
            "resources/read".to_string(),
        ))
    }
}
