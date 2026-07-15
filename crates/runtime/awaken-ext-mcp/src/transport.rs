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

    /// Whether the connection is still usable. Defaults to `true` for stateless
    /// transports; a process-backed transport reports its child's liveness.
    fn is_alive(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp::{CallToolResult, ToolContent};

    /// A tools-only transport: it implements only the two mandatory methods, so the
    /// prompt/resource/progress/liveness surfaces exercise the trait defaults.
    struct ToolsOnly;

    #[async_trait]
    impl McpToolTransport for ToolsOnly {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(Vec::new())
        }
        async fn call_tool(
            &self,
            tool_name: &str,
            _arguments: Value,
        ) -> Result<CallToolResult, McpTransportError> {
            Ok(CallToolResult {
                content: vec![ToolContent::Text {
                    text: format!("ran {tool_name}"),
                    annotations: None,
                    meta: None,
                }],
                structured_content: None,
                is_error: Some(false),
            })
        }
    }

    #[tokio::test]
    async fn a_tools_only_transport_defaults_the_optional_surfaces_fail_soft() {
        let t = ToolsOnly;
        // List surfaces default to empty (a tools-only server has none), never an error.
        assert!(t.list_prompts().await.expect("prompts default").is_empty());
        assert!(
            t.list_resources()
                .await
                .expect("resources default")
                .is_empty()
        );
        // Get/read surfaces default to an explicit NotSupported, not a panic.
        assert!(matches!(
            t.get_prompt("greet", None).await,
            Err(McpTransportError::NotSupported(m)) if m == "prompts/get"
        ));
        assert!(matches!(
            t.read_resource("file:///x").await,
            Err(McpTransportError::NotSupported(m)) if m == "resources/read"
        ));
        // A stateless transport reports alive by default.
        assert!(t.is_alive());
    }

    #[tokio::test]
    async fn call_tool_with_progress_defaults_to_a_plain_call() {
        // A transport with no server-notification support falls back to `call_tool`,
        // so a progress-aware caller still gets the result (just no progress events).
        let t = ToolsOnly;
        let (tx, mut rx) = mpsc::channel(1);
        let result = t
            .call_tool_with_progress("echo", Value::Null, tx)
            .await
            .expect("delegates to call_tool");
        assert!(matches!(result.is_error, Some(false)));
        // No progress was emitted on the fallback path.
        assert!(rx.try_recv().is_err());
    }
}
