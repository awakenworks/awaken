//! Task tools (ADR-0007): `send_message`, `cancel_task`, and
//! `recover_failed_messages`.
//!
//! Unlike the hand tools, these act on runtime effects (message ingress, live
//! control, durable recovery) that a single tool call has no context for. Each
//! is a typed [`Tool`] over a narrow **service port** that the host injects at
//! registration: the runtime owns ingress/control, so the composition root wires
//! a concrete service backed by it, while the extension owns the model-visible
//! tool surface. This keeps the tool testable in isolation and free of runtime
//! internals.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::tool::{
    RawTool, Tool, ToolError, ToolRecoveryCapability, current_tool_operation_context,
};
use serde::Deserialize;

use crate::erase;

/// Deliver a message to another thread in the multi-agent message lifecycle. A
/// thread is the stable, addressable unit (a run is one ephemeral execution); the
/// host backs this with the runtime's message ingress, resolving the target
/// thread's pending boundary and staging a durable delivery.
#[async_trait]
pub trait MessageSender: Send + Sync {
    async fn send(&self, request: MessageSendRequest) -> Result<(), ToolError>;
}

/// One durable cross-thread send intent. The runtime-owned operation identity is
/// always present in production; an optional caller key lets separate tool calls
/// in the same Run intentionally name the same logical message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSendRequest {
    pub target_thread: String,
    pub content: String,
    pub idempotency_key: Option<String>,
    pub source_run_id: String,
    pub operation_id: String,
}

/// Cancel a task (run) by id. The host backs this with the runtime's live
/// control / active-run registry.
#[async_trait]
pub trait TaskCanceller: Send + Sync {
    async fn cancel(&self, task_id: &str) -> Result<(), ToolError>;
}

/// Inspect and replay failed durable messages. Operations-scoped; the host backs
/// this with the runtime's durable message store.
#[async_trait]
pub trait MessageRecovery: Send + Sync {
    /// Recover and return a human-readable summary of what was recovered.
    async fn recover(&self) -> Result<String, ToolError>;
}

/// `send_message` over an injected [`MessageSender`].
pub struct SendMessageTool(Arc<dyn MessageSender>);

impl SendMessageTool {
    pub fn new(sender: Arc<dyn MessageSender>) -> Self {
        Self(sender)
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendMessageArgs {
    /// ID of the thread to message.
    pub target_thread: String,
    /// Message body.
    pub content: String,
    /// Optional caller key scoped to the sending Run.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[async_trait]
impl Tool for SendMessageTool {
    type Args = SendMessageArgs;
    type Output = String;
    const ID: &'static str = "send_message";
    const DESCRIPTION: &'static str = "Send a message to another thread";

    fn recovery_capability(&self) -> ToolRecoveryCapability {
        ToolRecoveryCapability::DurableRequest
    }
    async fn call(&self, args: SendMessageArgs) -> Result<String, ToolError> {
        let context = current_tool_operation_context().ok_or_else(|| {
            ToolError::Execution(
                "send_message requires runtime-owned durable operation context".to_string(),
            )
        })?;
        let source_run_id = context.run_id.ok_or_else(|| {
            ToolError::Execution("send_message requires a runtime-owned source run".to_string())
        })?;
        let idempotency_key = args
            .idempotency_key
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty());
        self.0
            .send(MessageSendRequest {
                target_thread: args.target_thread.clone(),
                content: args.content,
                idempotency_key,
                source_run_id: source_run_id.0,
                operation_id: context.operation_id,
            })
            .await?;
        Ok(format!("message sent to {}", args.target_thread))
    }
}

/// `cancel_task` over an injected [`TaskCanceller`].
pub struct CancelTaskTool(Arc<dyn TaskCanceller>);

impl CancelTaskTool {
    pub fn new(canceller: Arc<dyn TaskCanceller>) -> Self {
        Self(canceller)
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelTaskArgs {
    /// Task identifier.
    pub task_id: String,
}

#[async_trait]
impl Tool for CancelTaskTool {
    type Args = CancelTaskArgs;
    type Output = String;
    const ID: &'static str = "cancel_task";
    const DESCRIPTION: &'static str = "Cancel a background task";

    async fn call(&self, args: CancelTaskArgs) -> Result<String, ToolError> {
        self.0.cancel(&args.task_id).await?;
        Ok(format!("cancelled {}", args.task_id))
    }
}

/// `recover_failed_messages` over an injected [`MessageRecovery`].
pub struct RecoverFailedMessagesTool(Arc<dyn MessageRecovery>);

impl RecoverFailedMessagesTool {
    pub fn new(recovery: Arc<dyn MessageRecovery>) -> Self {
        Self(recovery)
    }
}

/// `recover_failed_messages` takes no arguments.
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecoverFailedMessagesArgs {}

#[async_trait]
impl Tool for RecoverFailedMessagesTool {
    type Args = RecoverFailedMessagesArgs;
    type Output = String;
    const ID: &'static str = "recover_failed_messages";
    const DESCRIPTION: &'static str = "Recover failed messages (operator-scoped)";

    async fn call(&self, _args: RecoverFailedMessagesArgs) -> Result<String, ToolError> {
        self.0.recover().await
    }
}

/// The task tools, erased for `Runtime::with_tool` registration. The host
/// supplies the concrete services that back each tool.
pub fn task_tools(
    sender: Arc<dyn MessageSender>,
    canceller: Arc<dyn TaskCanceller>,
    recovery: Arc<dyn MessageRecovery>,
) -> Vec<Arc<dyn RawTool>> {
    vec![
        erase(SendMessageTool::new(sender)),
        erase(CancelTaskTool::new(canceller)),
        erase(RecoverFailedMessagesTool::new(recovery)),
    ]
}
