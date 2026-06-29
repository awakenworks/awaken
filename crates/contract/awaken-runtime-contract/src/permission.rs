//! Permission policy and the tool gate.
//!
//! Permission is the *only* authorization path (permission-policy-axis.md,
//! G21). Visibility, selection, capability compatibility, and health never
//! grant — their result types carry no decision (G9). The loop always calls the
//! gate before executing a tool; a permission-backed gate maps a
//! `PermissionDecision` onto a `GateOutcome`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::tool::ToolOutput;

/// Normalized data for one authorization decision. Carries no live handle, so a
/// decision can be logged, replayed, and audited as plain data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionContext {
    pub tool_id: String,
    pub call_id: String,
    pub arguments: serde_json::Value,
}

/// Typed authorization decision. `Ask` parks the call for a human/out-of-band
/// approval correlated by `ticket_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionDecision {
    Allow,
    Deny { reason: String },
    Ask { ticket_id: String },
}

/// The authorization policy. Async because a real policy may consult an
/// external system; it returns only a decision, never executes the tool.
#[async_trait]
pub trait PermissionPolicy: Send + Sync {
    async fn decide(&self, ctx: &PermissionContext) -> PermissionDecision;
}

/// What the gate tells the loop to do with one tool call.
#[derive(Debug, Clone, PartialEq)]
pub enum GateOutcome {
    /// Execute the tool.
    Allow,
    /// Skip execution; feed a model-visible block result back instead.
    Block { reason: String },
    /// Skip execution; the gate supplied the result directly.
    SetResult(ToolOutput),
    /// Suspend the run pending an out-of-band decision (ticket correlation).
    Suspend { ticket_id: String },
}

/// The final invocation gate. The loop calls this for every tool call; only an
/// `Allow` reaches the executor.
#[async_trait]
pub trait ToolGateHook: Send + Sync {
    async fn gate(&self, ctx: &PermissionContext) -> GateOutcome;
}
