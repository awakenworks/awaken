//! Adapter: wraps Awaken agents as MCP tools.
//!
//! Each registered agent becomes one `McpTool` whose `call()` runs the agent
//! loop, collects the final assistant text, and returns it as `ToolContent`.
//!
//! Logging notifications are forwarded via the MCP server's outbound channel
//! so that clients can observe long-running agent runs.

use std::sync::Arc;

use mcp::protocol::{JsonRpcNotification, McpToolDefinition, ServerOutbound, ToolContent};
use mcp::tool::{BoxFuture, McpTool, ToolCallResult};
use serde_json::Value;
use tokio::sync::mpsc;

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::message::Message;
use awaken_runtime::{AgentRuntime, RunRequest};

use crate::transport::channel_sink::ChannelEventSink;

/// An MCP tool backed by an Awaken agent.
///
/// Calling this tool sends a user message to the agent, runs the full agent
/// loop, and returns the final assistant text as `ToolContent::text`.
///
/// If `outbound_tx` is set, structured logging notifications are sent during
/// execution.
pub struct AgentMcpTool {
    agent_id: String,
    description: String,
    runtime: Arc<AgentRuntime>,
    outbound_tx: Option<mpsc::Sender<ServerOutbound>>,
}

impl AgentMcpTool {
    pub fn new(agent_id: String, description: String, runtime: Arc<AgentRuntime>) -> Self {
        Self {
            agent_id,
            description,
            runtime,
            outbound_tx: None,
        }
    }

    /// Attach the MCP server's outbound channel for sending logging notifications.
    pub fn with_outbound(mut self, tx: mpsc::Sender<ServerOutbound>) -> Self {
        self.outbound_tx = Some(tx);
        self
    }

    /// Send a log notification via the MCP server's outbound channel.
    async fn send_log(&self, level: &str, message: &str) {
        if let Some(tx) = &self.outbound_tx {
            let params = serde_json::json!({
                "level": level,
                "logger": format!("agent/{}", self.agent_id),
                "data": message,
            });
            let notification = JsonRpcNotification {
                jsonrpc: "2.0".to_string(),
                method: "notifications/message".to_string(),
                params: Some(params),
            };
            let _ = tx.send(ServerOutbound::Notification(notification)).await;
        }
    }
}

impl McpTool for AgentMcpTool {
    fn definition(&self) -> McpToolDefinition {
        McpToolDefinition::new(&self.agent_id)
            .with_description(&self.description)
            .with_schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "The user message to send to the agent"
                    }
                },
                "required": ["message"]
            }))
    }

    fn call<'a>(&'a self, args: Value) -> BoxFuture<'a, ToolCallResult> {
        Box::pin(async move {
            let text = args
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            if text.is_empty() {
                return Err("'message' parameter is required and must be non-empty".to_string());
            }

            let thread_id = format!("mcp-{}", uuid::Uuid::now_v7());
            let messages = vec![Message::user(&text)];
            let request = RunRequest::new(thread_id, messages).with_agent_id(self.agent_id.clone());

            let (event_tx, mut event_rx) = mpsc::unbounded_channel();
            let sink = Arc::new(ChannelEventSink::new(event_tx));

            self.send_log("info", "starting agent run").await;

            let runtime = Arc::clone(&self.runtime);
            let run_handle = tokio::spawn(async move { runtime.run(request, sink).await });

            // Collect text deltas and emit logs from agent events.
            let mut assistant_text = String::new();
            let mut step_count: u32 = 0;
            while let Some(event) = event_rx.recv().await {
                match &event {
                    AgentEvent::TextDelta { delta } => {
                        assistant_text.push_str(delta);
                    }
                    AgentEvent::StepStart { .. } => {
                        step_count += 1;
                        self.send_log("info", &format!("step {step_count}")).await;
                    }
                    AgentEvent::ToolCallStart { name, .. } => {
                        self.send_log("info", &format!("calling tool: {name}"))
                            .await;
                    }
                    AgentEvent::ToolCallDone { result, .. } => {
                        let status = if result.is_success() {
                            "success"
                        } else {
                            "error"
                        };
                        self.send_log(
                            "info",
                            &format!("tool {} completed: {status}", result.tool_name),
                        )
                        .await;
                    }
                    AgentEvent::InferenceComplete {
                        model, duration_ms, ..
                    } => {
                        self.send_log(
                            "debug",
                            &format!("inference complete: model={model} duration={duration_ms}ms"),
                        )
                        .await;
                    }
                    _ => {}
                }
            }

            // Await the run to propagate panics.
            match run_handle.await {
                Ok(Ok(_)) => {
                    self.send_log("notice", "completed").await;
                }
                Ok(Err(e)) => {
                    self.send_log("error", &format!("agent run failed: {e}"))
                        .await;
                    if assistant_text.is_empty() {
                        return Err(format!("agent run failed: {e}"));
                    }
                }
                Err(e) => {
                    return Err(format!("agent task panicked: {e}"));
                }
            }

            if assistant_text.is_empty() {
                assistant_text = "(no response)".to_string();
            }

            Ok(vec![ToolContent::text(assistant_text)])
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime::{AgentResolver, AgentRuntime, ResolvedAgent, RuntimeError};
    use serde_json::json;

    struct StubResolver;
    impl AgentResolver for StubResolver {
        fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
            Err(RuntimeError::AgentNotFound {
                agent_id: agent_id.to_string(),
            })
        }

        fn agent_ids(&self) -> Vec<String> {
            vec!["test-agent".to_string()]
        }
    }

    fn test_runtime() -> Arc<AgentRuntime> {
        Arc::new(AgentRuntime::new(Arc::new(StubResolver)))
    }

    #[test]
    fn definition_has_correct_name_and_schema() {
        let tool = AgentMcpTool::new(
            "my-agent".to_string(),
            "A test agent".to_string(),
            test_runtime(),
        );
        let def = tool.definition();
        assert_eq!(def.name, "my-agent");
        assert_eq!(def.description.as_deref(), Some("A test agent"));
        let schema = &def.input_schema;
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["message"].is_object());
    }

    #[tokio::test]
    async fn call_rejects_empty_message() {
        let tool = AgentMcpTool::new("my-agent".to_string(), "test".to_string(), test_runtime());
        let result = tool.call(json!({"message": ""})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("non-empty"));
    }

    #[tokio::test]
    async fn call_rejects_missing_message() {
        let tool = AgentMcpTool::new("my-agent".to_string(), "test".to_string(), test_runtime());
        let result = tool.call(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn call_with_unresolvable_agent_returns_error() {
        let tool = AgentMcpTool::new(
            "nonexistent".to_string(),
            "test".to_string(),
            test_runtime(),
        );
        let result = tool.call(json!({"message": "hello"})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("failed"));
    }

    #[tokio::test]
    async fn logging_notifications_are_sent() {
        let (tx, mut rx) = mpsc::channel(64);
        let tool = AgentMcpTool::new("test-agent".to_string(), "test".to_string(), test_runtime())
            .with_outbound(tx);

        // Call will fail (stub resolver), but logs should be sent first.
        let _ = tool.call(json!({"message": "hello"})).await;

        // Collect all notifications.
        let mut notifications = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let ServerOutbound::Notification(n) = msg {
                notifications.push(n);
            }
        }

        // Should have at least one structured logging notification.
        assert!(
            notifications
                .iter()
                .any(|n| n.method == "notifications/message"),
            "expected at least one logging notification, got: {notifications:?}"
        );
    }
}
