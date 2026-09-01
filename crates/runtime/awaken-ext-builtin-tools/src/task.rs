//! Task tool (ADR-0007): `cancel_task`.
//!
//! Unlike the hand tools, this acts on live runtime control that a single tool
//! call has no context for. It is a typed [`Tool`] over a narrow **service port** that the host injects at
//! registration: the runtime owns ingress/control, so the composition root wires
//! a concrete service backed by it, while the extension owns the model-visible
//! tool surface. This keeps the tool testable in isolation and free of runtime
//! internals.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::tool::{RawTool, Tool, ToolError};
use serde::Deserialize;

use crate::erase;

/// Cancel a task (run) by id. The host backs this with the runtime's live
/// control / active-run registry.
#[async_trait]
pub trait TaskCanceller: Send + Sync {
    async fn cancel(&self, task_id: &str) -> Result<(), ToolError>;
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

/// The task tools, erased for `Runtime::with_tool` registration. The host
/// supplies the concrete services that back each tool.
pub fn task_tools(canceller: Arc<dyn TaskCanceller>) -> Vec<Arc<dyn RawTool>> {
    vec![erase(CancelTaskTool::new(canceller))]
}
