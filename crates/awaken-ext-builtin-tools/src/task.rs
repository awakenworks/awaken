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
use awaken_runtime_contract::tool::{RawTool, Tool, ToolError};
use serde::Deserialize;

use crate::erasure::erase;

/// Deliver a message into the multi-agent message lifecycle. The host backs this
/// with the runtime's message ingress.
#[async_trait]
pub trait MessageSender: Send + Sync {
    async fn send(&self, content: &str) -> Result<(), ToolError>;
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

#[derive(Deserialize)]
pub struct SendMessageArgs {
    pub content: String,
}

#[async_trait]
impl Tool for SendMessageTool {
    type Args = SendMessageArgs;
    type Output = String;
    fn id(&self) -> &str {
        "send_message"
    }
    async fn call(&self, args: SendMessageArgs) -> Result<String, ToolError> {
        self.0.send(&args.content).await?;
        Ok("message sent".to_string())
    }
}

/// `cancel_task` over an injected [`TaskCanceller`].
pub struct CancelTaskTool(Arc<dyn TaskCanceller>);

impl CancelTaskTool {
    pub fn new(canceller: Arc<dyn TaskCanceller>) -> Self {
        Self(canceller)
    }
}

#[derive(Deserialize)]
pub struct CancelTaskArgs {
    pub task_id: String,
}

#[async_trait]
impl Tool for CancelTaskTool {
    type Args = CancelTaskArgs;
    type Output = String;
    fn id(&self) -> &str {
        "cancel_task"
    }
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
#[derive(Deserialize)]
pub struct RecoverFailedMessagesArgs {}

#[async_trait]
impl Tool for RecoverFailedMessagesTool {
    type Args = RecoverFailedMessagesArgs;
    type Output = String;
    fn id(&self) -> &str {
        "recover_failed_messages"
    }
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
