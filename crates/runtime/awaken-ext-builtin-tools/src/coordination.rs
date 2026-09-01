//! Asynchronous Agent coordination tools.
//!
//! These are ordinary first-party tools over one injected orchestration port.
//! The extension owns their stable model-visible contract; the host owns roster
//! resolution, Thread/Run creation, durable delivery, and recovery. Keeping
//! those effects behind [`AgentCoordinator`] prevents this package from growing
//! a second Thread registry or message transport.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::tool::{
    RawTool, Tool, ToolError, ToolRecoveryCapability, current_tool_operation_context,
};
use serde::{Deserialize, Serialize};

use crate::erase;

/// Stable model-facing identities owned with their concrete coordination tools.
pub const LIST_AGENTS: &str = "list_agents";
pub const SEND_MESSAGE: &str = "send_message";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRosterEntry {
    pub agent_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentListRequest {
    pub source_run_id: String,
    pub source_thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentMessageTarget {
    Spawn { agent_id: String },
    ExistingThread { session_thread_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMessageRequest {
    pub target: AgentMessageTarget,
    pub message: String,
    pub source_run_id: String,
    pub source_thread_id: String,
    pub source_call_id: String,
    pub operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessageReceipt {
    pub session_thread_id: String,
    pub accepted: bool,
}

#[async_trait]
pub trait AgentCoordinator: Send + Sync {
    async fn list_agents(
        &self,
        request: AgentListRequest,
    ) -> Result<Vec<AgentRosterEntry>, ToolError>;

    async fn send_message(
        &self,
        request: AgentMessageRequest,
    ) -> Result<AgentMessageReceipt, ToolError>;
}

pub struct ListAgentsTool(Arc<dyn AgentCoordinator>);

impl ListAgentsTool {
    pub fn new(coordinator: Arc<dyn AgentCoordinator>) -> Self {
        Self(coordinator)
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListAgentsArgs {}

#[async_trait]
impl Tool for ListAgentsTool {
    type Args = ListAgentsArgs;
    type Output = Vec<AgentRosterEntry>;
    const ID: &'static str = LIST_AGENTS;
    const DESCRIPTION: &'static str = "List the Agents available for asynchronous coordination";

    async fn call(&self, _args: Self::Args) -> Result<Self::Output, ToolError> {
        let context = current_tool_operation_context().ok_or_else(|| {
            ToolError::Execution("list_agents requires runtime-owned operation context".to_string())
        })?;
        let source_run_id = context.run_id.ok_or_else(|| {
            ToolError::Execution("list_agents requires a runtime-owned source run".to_string())
        })?;
        let source_thread_id = context.thread_id.ok_or_else(|| {
            ToolError::Execution("list_agents requires a runtime-owned source thread".to_string())
        })?;
        self.0
            .list_agents(AgentListRequest {
                source_run_id: source_run_id.0,
                source_thread_id: source_thread_id.0,
            })
            .await
    }
}

pub struct SendMessageTool(Arc<dyn AgentCoordinator>);

impl SendMessageTool {
    pub fn new(coordinator: Arc<dyn AgentCoordinator>) -> Self {
        Self(coordinator)
    }
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendMessageArgs {
    /// Roster Agent id; starts a new persistent Agent thread.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Existing Agent thread id; retains that thread's history.
    #[serde(default)]
    pub session_thread_id: Option<String>,
    /// Task or follow-up message delivered to the selected Agent thread.
    pub message: String,
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[async_trait]
impl Tool for SendMessageTool {
    type Args = SendMessageArgs;
    type Output = AgentMessageReceipt;
    const ID: &'static str = SEND_MESSAGE;
    const DESCRIPTION: &'static str =
        "Start an Agent thread or send a follow-up to an existing Agent thread";

    fn recovery_capability(&self) -> ToolRecoveryCapability {
        ToolRecoveryCapability::DurableRequest
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, ToolError> {
        let target = match (non_empty(args.agent_id), non_empty(args.session_thread_id)) {
            (Some(agent_id), None) => AgentMessageTarget::Spawn { agent_id },
            (None, Some(session_thread_id)) => {
                AgentMessageTarget::ExistingThread { session_thread_id }
            }
            (Some(_), Some(_)) | (None, None) => {
                return Err(ToolError::InvalidArguments(
                    "exactly one of `agent_id` or `session_thread_id` is required".to_string(),
                ));
            }
        };
        if args.message.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "`message` must not be empty".to_string(),
            ));
        }
        let context = current_tool_operation_context().ok_or_else(|| {
            ToolError::Execution(
                "send_message requires runtime-owned operation context".to_string(),
            )
        })?;
        let source_run_id = context.run_id.ok_or_else(|| {
            ToolError::Execution("send_message requires a runtime-owned source run".to_string())
        })?;
        let source_thread_id = context.thread_id.ok_or_else(|| {
            ToolError::Execution("send_message requires a runtime-owned source thread".to_string())
        })?;
        let source_call_id = context.call_id.ok_or_else(|| {
            ToolError::Execution(
                "send_message requires a runtime-owned tool call identity".to_string(),
            )
        })?;
        self.0
            .send_message(AgentMessageRequest {
                target,
                message: args.message,
                source_run_id: source_run_id.0,
                source_thread_id: source_thread_id.0,
                source_call_id,
                operation_id: context.operation_id,
            })
            .await
    }
}

pub fn coordination_tools(coordinator: Arc<dyn AgentCoordinator>) -> Vec<Arc<dyn RawTool>> {
    vec![
        erase(ListAgentsTool::new(coordinator.clone())),
        erase(SendMessageTool::new(coordinator)),
    ]
}
