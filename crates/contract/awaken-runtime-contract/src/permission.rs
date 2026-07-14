//! Permission policy and the tool gate.
//!
//! Permission is the *only* authorization path (permission-policy-axis.md,
//! G21). Visibility, selection, capability compatibility, and health never
//! grant — their result types carry no decision (G9). The loop always calls the
//! gate before executing a tool; a permission-backed gate maps a
//! `PermissionDecision` onto a `GateOutcome`.

use async_trait::async_trait;
use awaken_agent_contract::agent::state::Store;
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
    /// Defer this call as a committed `ScheduledAction` (ADR-0020): the run parks
    /// and the action is performed later (in-process or by recovery), not decided
    /// by a human. `correlation_id` keys the committed request and its resume.
    /// `action_kind`, when set, names a plugin-owned scheduled-action kind that
    /// must be present in the resolved environment, else the run fails closed
    /// (ADR-0027); `None` is the ordinary tool-backed scheduled action.
    Schedule {
        correlation_id: String,
        action_kind: Option<String>,
    },
}

impl GateOutcome {
    /// The permission-relevant decision label for this outcome — the single
    /// authoritative vocabulary the audit trail (ADR-0030) records. Kept beside
    /// the type so a new outcome variant forces a label here, not in each caller.
    #[must_use]
    pub fn decision_label(&self) -> &'static str {
        match self {
            GateOutcome::Allow => "allow",
            GateOutcome::Block { .. } => "deny",
            GateOutcome::Suspend { .. } => "ask",
            GateOutcome::SetResult(_) => "set_result",
            GateOutcome::Schedule { .. } => "schedule",
        }
    }
}

/// The final invocation gate. The loop calls this for every tool call; only an
/// `Allow` reaches the executor. The loop consults the host gate first, then any
/// plugin-contributed gates in dependency order; a call runs only if every gate
/// allows it, and a permission `Deny` is absolute — a plugin gate can further
/// restrict but never widen what permission allows (G21).
#[async_trait]
pub trait ToolGateHook: Send + Sync {
    /// Stable id, used to bound a plugin-contributed gate under its
    /// `CapabilityBound` (G30). A host-wired gate that is not a plugin
    /// contribution keeps the default.
    fn id(&self) -> &str {
        "gate"
    }

    /// Decide one tool call against the run's read-only state. Most gates ignore
    /// `state`; a state machine gate reads it to enforce a precondition.
    async fn gate(&self, ctx: &PermissionContext, state: &Store) -> GateOutcome;
}
