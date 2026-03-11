use crate::runtime::activity::ActivityManager;
use crate::runtime::behavior::AgentBehavior;
use crate::runtime::run::RunIdentity;
use crate::runtime::tool_call::lifecycle::SuspendedCall;
use crate::runtime::tool_call::{CallerContext, Tool, ToolDescriptor, ToolResult};
use crate::thread::{Message, ToolCall};
use crate::RunPolicy;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tirea_state::TrackedPatch;
use tokio_util::sync::CancellationToken;

/// Result of one tool call execution.
#[derive(Debug, Clone)]
pub struct ToolExecution {
    pub call: ToolCall,
    pub result: ToolResult,
    pub patch: Option<TrackedPatch>,
}

/// Canonical outcome for one tool call lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallOutcome {
    /// Tool execution was suspended and needs external resume/decision.
    Suspended,
    /// Tool execution succeeded.
    Succeeded,
    /// Tool execution failed.
    Failed,
}

impl ToolCallOutcome {
    /// Derive outcome from a concrete `ToolResult`.
    pub fn from_tool_result(result: &ToolResult) -> Self {
        match result.status {
            crate::runtime::tool_call::ToolStatus::Pending => Self::Suspended,
            crate::runtime::tool_call::ToolStatus::Error => Self::Failed,
            crate::runtime::tool_call::ToolStatus::Success
            | crate::runtime::tool_call::ToolStatus::Warning => Self::Succeeded,
        }
    }
}

/// Input envelope passed to tool execution strategies.
pub struct ToolExecutionRequest<'a> {
    pub tools: &'a HashMap<String, Arc<dyn Tool>>,
    pub calls: &'a [ToolCall],
    pub state: &'a Value,
    pub tool_descriptors: &'a [ToolDescriptor],
    /// Agent behavior for declarative phase dispatch.
    pub agent_behavior: Option<&'a dyn AgentBehavior>,
    pub activity_manager: Arc<dyn ActivityManager>,
    pub run_policy: &'a RunPolicy,
    pub run_identity: RunIdentity,
    pub caller_context: CallerContext,
    pub thread_id: &'a str,
    pub thread_messages: &'a [Arc<Message>],
    pub state_version: u64,
    pub cancellation_token: Option<&'a CancellationToken>,
}

/// Output item produced by tool execution strategies.
#[derive(Debug, Clone)]
pub struct ToolExecutionResult {
    pub execution: ToolExecution,
    pub outcome: ToolCallOutcome,
    /// Suspension payload for suspended outcomes.
    pub suspended_call: Option<SuspendedCall>,
    pub reminders: Vec<String>,
    /// User messages to append after tool execution.
    pub user_messages: Vec<String>,
    pub pending_patches: Vec<TrackedPatch>,
    /// Serialized state actions captured during this tool execution (intent log).
    pub serialized_state_actions: Vec<crate::runtime::state::SerializedStateAction>,
}

/// Error returned by custom tool executors.
#[derive(Debug, Clone, Error)]
pub enum ToolExecutorError {
    #[error("tool execution cancelled")]
    Cancelled { thread_id: String },
    #[error("tool execution failed: {message}")]
    Failed { message: String },
}

/// Policy controlling when resume decisions are replayed into tool execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionReplayPolicy {
    /// Replay each resolved suspended call as soon as its decision arrives.
    Immediate,
    /// Replay only when all currently suspended calls have decisions.
    BatchAllSuspended,
}

/// Strategy abstraction for tool execution.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(
        &self,
        request: ToolExecutionRequest<'_>,
    ) -> Result<Vec<ToolExecutionResult>, ToolExecutorError>;

    /// Stable strategy label for logs/debug output.
    fn name(&self) -> &'static str;

    /// Whether apply step should enforce parallel patch conflict checks.
    fn requires_parallel_patch_conflict_check(&self) -> bool {
        false
    }

    /// How runtime should replay resolved suspend decisions for this executor.
    fn decision_replay_policy(&self) -> DecisionReplayPolicy {
        DecisionReplayPolicy::Immediate
    }
}
