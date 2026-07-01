//! Adapt an MCP server tool into an awaken [`RawTool`].
//!
//! [`McpRawTool`] erases one MCP tool behind the runtime's `RawTool` port, keyed
//! by the namespaced id `mcp__<server>__<tool>`. It maps the MCP three-state
//! result onto the neutral one:
//!
//! - a transport failure is a [`ToolError`] (aborts the call);
//! - an MCP `isError` result is a model-visible [`ToolOutput::error`];
//! - a normal result is [`ToolOutput::ok`].

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};
use mcp::{McpToolDefinition, ToolContent};
use serde_json::Value;

use crate::error::McpError;
use crate::id_mapping::to_tool_id;
use crate::transport::McpToolTransport;

/// One MCP server tool presented as a runtime [`RawTool`].
pub struct McpRawTool {
    /// Namespaced runtime id (`mcp__<server>__<tool>`); the descriptor shares it.
    id: String,
    /// The tool's name on the wire (unsanitized), passed to `tools/call`.
    tool_name: String,
    transport: Arc<dyn McpToolTransport>,
}

impl McpRawTool {
    /// Build a raw tool for `tool_name` on `server_name`, invoked through
    /// `transport`. Fails if either name sanitizes to an empty id component.
    pub fn new(
        server_name: &str,
        tool_name: &str,
        transport: Arc<dyn McpToolTransport>,
    ) -> Result<Self, McpError> {
        let id = to_tool_id(server_name, tool_name)?;
        Ok(Self {
            id,
            tool_name: tool_name.to_string(),
            transport,
        })
    }
}

#[async_trait]
impl RawTool for McpRawTool {
    fn id(&self) -> &str {
        &self.id
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        // A null argument payload is an empty object, matching the SDK's
        // expectation and the erasure convention for no-argument tools.
        let arguments = if call.arguments.is_null() {
            Value::Object(serde_json::Map::new())
        } else {
            call.arguments
        };
        match self.transport.call_tool(&self.tool_name, arguments).await {
            // Transport failure: the call could not complete — a runtime error
            // the loop surfaces to the model without a tool result.
            Err(err) => Err(ToolError::Execution(err.to_string())),
            Ok(result) => {
                let content = flatten_content(&result.content);
                if result.is_error.unwrap_or(false) {
                    // Tool-level error: model-visible, the run continues.
                    Ok(ToolOutput::error(call.call_id, content))
                } else {
                    Ok(ToolOutput::ok(call.call_id, content))
                }
            }
        }
    }
}

/// Render MCP tool content into the model-visible string. Text blocks pass
/// through, joined by newlines; a non-text block (image/audio/resource) becomes
/// a compact JSON marker so the model still sees that content came back.
pub(crate) fn flatten_content(content: &[ToolContent]) -> String {
    content
        .iter()
        .map(|block| match block {
            ToolContent::Text { text, .. } => text.clone(),
            other => serde_json::to_string(other)
                .unwrap_or_else(|_| "[unrenderable mcp content]".to_string()),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the model-visible descriptor for an MCP tool: namespaced id, the
/// server-supplied description, and its input schema (normalized to an object
/// schema, since a server may omit it). The descriptor's content hash is pinned
/// over that surface (G3/G8).
pub fn mcp_tool_descriptor(
    server_name: &str,
    def: &McpToolDefinition,
) -> Result<ToolDescriptor, McpError> {
    let id = to_tool_id(server_name, &def.name)?;
    let description = def.description.clone().unwrap_or_default();
    let parameters = normalize_schema(def.input_schema.clone());
    Ok(ToolDescriptor::pinned("mcp", id, description, parameters))
}

/// Ensure the schema the model sees is an object schema. MCP servers may send a
/// non-object or omit the schema entirely; the runtime descriptor expects an
/// object shape.
fn normalize_schema(schema: Value) -> Value {
    match schema {
        Value::Object(_) => schema,
        _ => serde_json::json!({ "type": "object" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp::CallToolResult;
    use mcp::transport::McpTransportError;

    /// A transport whose single `call_tool` outcome is fixed, so each of the
    /// three result states can be driven deterministically.
    struct FakeTransport {
        tools: Vec<McpToolDefinition>,
        outcome: Outcome,
    }

    enum Outcome {
        /// A successful call returning `text`, with the given `is_error` flag.
        Result { text: String, is_error: bool },
        /// The transport itself failed.
        TransportErr(String),
    }

    #[async_trait]
    impl McpToolTransport for FakeTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(self.tools.clone())
        }

        async fn call_tool(
            &self,
            _tool_name: &str,
            _arguments: Value,
        ) -> Result<CallToolResult, McpTransportError> {
            match &self.outcome {
                Outcome::Result { text, is_error } => Ok(CallToolResult {
                    content: vec![ToolContent::Text {
                        text: text.clone(),
                        annotations: None,
                        meta: None,
                    }],
                    structured_content: None,
                    is_error: Some(*is_error),
                }),
                Outcome::TransportErr(msg) => Err(McpTransportError::TransportError(msg.clone())),
            }
        }
    }

    fn tool_def(name: &str) -> McpToolDefinition {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "description": format!("the {name} tool"),
            "inputSchema": { "type": "object", "properties": { "q": { "type": "string" } } },
        }))
        .expect("valid tool definition")
    }

    fn transport(outcome: Outcome) -> Arc<dyn McpToolTransport> {
        Arc::new(FakeTransport {
            tools: vec![tool_def("echo")],
            outcome,
        })
    }

    fn call() -> ToolCall {
        ToolCall {
            call_id: "c1".to_string(),
            tool_id: "mcp__srv__echo".to_string(),
            arguments: serde_json::json!({ "q": "hi" }),
        }
    }

    #[tokio::test]
    async fn success_result_maps_to_ok_output() {
        let tool = McpRawTool::new(
            "srv",
            "echo",
            transport(Outcome::Result {
                text: "pong".to_string(),
                is_error: false,
            }),
        )
        .expect("builds");
        assert_eq!(tool.id(), "mcp__srv__echo");
        let out = tool.invoke(call()).await.expect("invokes");
        assert!(!out.is_error);
        assert_eq!(out.content, "pong");
        assert_eq!(out.call_id, "c1");
    }

    #[tokio::test]
    async fn tool_error_result_maps_to_model_visible_error() {
        let tool = McpRawTool::new(
            "srv",
            "echo",
            transport(Outcome::Result {
                text: "boom".to_string(),
                is_error: true,
            }),
        )
        .expect("builds");
        // A tool-level error does not abort the run: it is a model-visible
        // error result, not a `ToolError`.
        let out = tool.invoke(call()).await.expect("invokes");
        assert!(out.is_error);
        assert_eq!(out.content, "boom");
    }

    #[tokio::test]
    async fn transport_failure_maps_to_tool_error() {
        let tool = McpRawTool::new(
            "srv",
            "echo",
            transport(Outcome::TransportErr("connection reset".to_string())),
        )
        .expect("builds");
        let err = tool.invoke(call()).await.expect_err("transport fails");
        assert!(matches!(err, ToolError::Execution(msg) if msg.contains("connection reset")));
    }

    #[tokio::test]
    async fn null_arguments_become_an_empty_object() {
        // A no-argument call must still reach the transport (as `{}`), not be
        // rejected. The fake ignores args, so a successful result proves the
        // null path did not panic or short-circuit.
        let tool = McpRawTool::new(
            "srv",
            "echo",
            transport(Outcome::Result {
                text: "ok".to_string(),
                is_error: false,
            }),
        )
        .expect("builds");
        let mut c = call();
        c.arguments = Value::Null;
        let out = tool.invoke(c).await.expect("invokes");
        assert_eq!(out.content, "ok");
    }

    #[test]
    fn descriptor_is_namespaced_and_carries_the_schema() {
        let def = tool_def("echo");
        let descriptor = mcp_tool_descriptor("srv", &def).expect("builds");
        assert_eq!(descriptor.id, "mcp__srv__echo");
        assert_eq!(descriptor.description, "the echo tool");
        assert_eq!(descriptor.parameters["properties"]["q"]["type"], "string");
        // The id feeds the content hash, so it is present in the pinned hash.
        assert!(descriptor.content_hash.contains("mcp__srv__echo"));
    }

    #[test]
    fn descriptor_normalizes_a_missing_schema_to_an_object() {
        let def: McpToolDefinition = serde_json::from_value(serde_json::json!({
            "name": "bare",
        }))
        .expect("valid tool definition");
        let descriptor = mcp_tool_descriptor("srv", &def).expect("builds");
        assert_eq!(descriptor.parameters["type"], "object");
    }

    #[test]
    fn multiple_text_blocks_join_with_newlines() {
        let content = vec![
            ToolContent::Text {
                text: "line one".to_string(),
                annotations: None,
                meta: None,
            },
            ToolContent::Text {
                text: "line two".to_string(),
                annotations: None,
                meta: None,
            },
        ];
        assert_eq!(flatten_content(&content), "line one\nline two");
    }
}
