//! Unified agent delegation tool -- dispatches to local or remote backend.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::backend::{
    BackendControl, BackendParentContext, BackendRunRequest, ExecutionBackend, LocalBackend,
};
use crate::registry::{
    AgentResolver, ExecutionResolver, LocalExecutionResolver, ResolvedBackendAgent,
    ResolvedExecution,
};
use awaken_contract::contract::event_sink::{EventSink, NullEventSink};
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};

use super::a2a_backend::{A2aBackend, A2aConfig};
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
    /// Execution resolver used to build the canonical execution plan at call time.
    resolver: Arc<dyn ExecutionResolver>,
}

impl AgentTool {
    /// Create a tool that delegates to a local sub-agent.
    pub fn local(
        agent_id: impl Into<String>,
        description: impl Into<String>,
        resolver: Arc<dyn AgentResolver>,
    ) -> Self {
        Self::with_execution_resolver(
            agent_id,
            description,
            Arc::new(LocalExecutionResolver::new(resolver)),
        )
    }

    /// Create a tool that delegates to a remote agent via A2A protocol.
    pub fn remote(
        agent_id: impl Into<String>,
        description: impl Into<String>,
        config: A2aConfig,
    ) -> Self {
        Self::with_backend(agent_id, description, Arc::new(A2aBackend::new(config)))
    }

    /// Create a tool with a custom backend (for testing).
    pub fn with_backend(
        agent_id: impl Into<String>,
        description: impl Into<String>,
        backend: Arc<dyn ExecutionBackend>,
    ) -> Self {
        let agent_id = agent_id.into();
        let description = description.into();
        Self {
            agent_id: agent_id.clone(),
            description: description.clone(),
            resolver: Arc::new(StaticExecutionResolver::non_local(
                &agent_id,
                &description,
                backend,
            )),
        }
    }

    pub fn with_execution_resolver(
        agent_id: impl Into<String>,
        description: impl Into<String>,
        resolver: Arc<dyn ExecutionResolver>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            description: description.into(),
            resolver,
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

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
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

        let resolved = self
            .resolver
            .resolve_execution(&self.agent_id)
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;

        let request = BackendRunRequest {
            agent_id: &self.agent_id,
            messages,
            sink,
            resolver: self.resolver.as_ref(),
            run_identity: None,
            parent: Some(BackendParentContext {
                parent_run_id: Some(ctx.run_identity.run_id.clone()),
                parent_thread_id: Some(ctx.run_identity.thread_id.clone()),
                parent_tool_call_id: Some(ctx.call_id.clone()),
            }),
            phase_runtime: None,
            checkpoint_store: None,
            control: BackendControl::default(),
            overrides: None,
            frontend_tools: Vec::new(),
            inbox: None,
            is_continuation: false,
        };

        match execute_resolved(&resolved, request).await {
            Ok(result) => {
                let status_str = result.status.to_string();
                let mut tool_result = ToolResult::success(
                    &tool_id,
                    json!({
                        "agent_id": result.agent_id,
                        "status": status_str,
                        "response": result.response,
                        "steps": result.steps,
                    }),
                );
                if let Some(ref child_run_id) = result.run_id {
                    tool_result = tool_result.with_metadata(
                        "child_run_id",
                        serde_json::Value::String(child_run_id.clone()),
                    );
                }
                Ok(tool_result.into())
            }
            Err(e) => Ok(ToolResult::error(&tool_id, e.to_string()).into()),
        }
    }
}

async fn execute_resolved(
    resolved: &ResolvedExecution,
    request: BackendRunRequest<'_>,
) -> Result<crate::backend::BackendRunResult, crate::backend::ExecutionBackendError> {
    match resolved {
        ResolvedExecution::Local(_) => LocalBackend::new().execute(request).await,
        ResolvedExecution::NonLocal(agent) => agent.backend.execute(request).await,
    }
}

struct StaticExecutionResolver {
    local: Option<crate::registry::ResolvedAgent>,
    execution: ResolvedExecution,
}

impl StaticExecutionResolver {
    fn non_local(agent_id: &str, description: &str, backend: Arc<dyn ExecutionBackend>) -> Self {
        let spec = Arc::new(awaken_contract::registry_spec::AgentSpec {
            id: agent_id.to_string(),
            model: String::new(),
            system_prompt: description.to_string(),
            ..Default::default()
        });
        Self {
            local: None,
            execution: ResolvedExecution::NonLocal(ResolvedBackendAgent { spec, backend }),
        }
    }
}

impl AgentResolver for StaticExecutionResolver {
    fn resolve(
        &self,
        agent_id: &str,
    ) -> Result<crate::registry::ResolvedAgent, crate::RuntimeError> {
        if let Some(agent) = &self.local {
            if agent.id() == agent_id {
                return Ok(agent.clone());
            }
        }
        Err(crate::RuntimeError::ResolveFailed {
            message: format!("agent '{agent_id}' cannot be resolved locally"),
        })
    }

    fn agent_ids(&self) -> Vec<String> {
        vec![self.execution.spec().id.clone()]
    }
}

impl ExecutionResolver for StaticExecutionResolver {
    fn resolve_execution(&self, agent_id: &str) -> Result<ResolvedExecution, crate::RuntimeError> {
        if self.execution.spec().id == agent_id {
            Ok(self.execution.clone())
        } else {
            Err(crate::RuntimeError::ResolveFailed {
                message: format!("agent not found: {agent_id}"),
            })
        }
    }
}
