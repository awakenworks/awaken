//! Unified agent delegation tool -- dispatches to local or remote backend.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use awaken_contract::contract::event_sink::{EventSink, NullEventSink};
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult,
};

use crate::registry::AgentResolver;

use super::a2a_backend::{A2aBackend, A2aConfig};
use super::backend::AgentBackend;
use super::local_backend::LocalBackend;
use super::progress_sink::ProgressForwardingSink;

/// Unified tool for agent delegation.
///
/// The LLM calls this tool to delegate work to a sub-agent. Routing to
/// local or remote backend is transparent -- determined at construction time.
pub struct AgentTool {
    /// Target agent ID.
    agent_id: String,
    /// Human-readable description for the LLM.
    description: String,
    /// Backend that performs the actual execution.
    backend: Arc<dyn AgentBackend>,
}

impl AgentTool {
    /// Create a tool that delegates to a local sub-agent.
    pub fn local(
        agent_id: impl Into<String>,
        description: impl Into<String>,
        resolver: Arc<dyn AgentResolver>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            description: description.into(),
            backend: Arc::new(LocalBackend::new(resolver)),
        }
    }

    /// Create a tool that delegates to a remote agent via A2A protocol.
    pub fn remote(
        agent_id: impl Into<String>,
        description: impl Into<String>,
        config: A2aConfig,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            description: description.into(),
            backend: Arc::new(A2aBackend::new(config)),
        }
    }

    /// Create a tool with a custom backend (for testing).
    pub fn with_backend(
        agent_id: impl Into<String>,
        description: impl Into<String>,
        backend: Arc<dyn AgentBackend>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            description: description.into(),
            backend,
        }
    }

    /// Returns the target agent ID.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }
}

#[async_trait]
impl Tool for AgentTool {
    fn descriptor(&self) -> ToolDescriptor {
        let tool_id = format!("agent_run_{}", self.agent_id);
        ToolDescriptor::new(&tool_id, &tool_id, &self.description).with_parameters(json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Task to delegate to the sub-agent"
                }
            },
            "required": ["prompt"]
        }))
    }

    fn validate_args(&self, args: &Value) -> Result<(), ToolError> {
        if args.get("prompt").and_then(Value::as_str).is_none() {
            return Err(ToolError::InvalidArguments(
                "missing required field \"prompt\"".into(),
            ));
        }
        Ok(())
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();

        if prompt.is_empty() {
            return Err(ToolError::InvalidArguments(
                "prompt must not be empty".into(),
            ));
        }

        let tool_id = format!("agent_run_{}", self.agent_id);
        let messages = vec![awaken_contract::contract::message::Message::user(&prompt)];

        // Build a forwarding sink: if parent has a sink, filter through ProgressForwardingSink;
        // otherwise use NullEventSink
        let sink: Arc<dyn EventSink> = match &ctx.activity_sink {
            Some(parent_sink) => Arc::new(ProgressForwardingSink::new(parent_sink.clone())),
            None => Arc::new(NullEventSink),
        };

        match self
            .backend
            .execute(
                &self.agent_id,
                messages,
                sink,
                Some(ctx.run_identity.run_id.clone()),
                Some(ctx.call_id.clone()),
            )
            .await
        {
            Ok(result) => {
                let status_str = result.status.to_string();
                Ok(ToolResult::success(
                    &tool_id,
                    json!({
                        "agent_id": result.agent_id,
                        "status": status_str,
                        "response": result.response,
                        "steps": result.steps,
                    }),
                ))
            }
            Err(e) => Ok(ToolResult::error(&tool_id, e.to_string())),
        }
    }
}
