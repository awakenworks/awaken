//! The permission gate — the runtime's single authorization choke point.
//!
//! [`PermissionGate`] is the [`ToolGateHook`] the loop calls before every
//! protected tool call. It asks an injected [`PermissionPolicy`] for a decision
//! and maps it onto a [`GateOutcome`] (ADR-0030). It is the only path that can let
//! a protected operation proceed (G21); visibility, selection, capability, and
//! health never reach this decision. The concrete rule policy (patterns, modes)
//! is an extension; the runtime owns only this mapping.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::permission::{
    GateOutcome, PermissionContext, PermissionDecision, PermissionPolicy, ToolGateHook,
};

/// A [`ToolGateHook`] backed by a [`PermissionPolicy`].
pub struct PermissionGate {
    policy: Arc<dyn PermissionPolicy>,
}

impl PermissionGate {
    /// Gate every protected tool call through `policy`.
    pub fn new(policy: Arc<dyn PermissionPolicy>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl ToolGateHook for PermissionGate {
    async fn gate(&self, ctx: &PermissionContext) -> GateOutcome {
        // allow → execute; deny → a model-visible block; ask → park on a decision
        // ticket the operator resumes (ADR-0030 D1/D2).
        match self.policy.decide(ctx).await {
            PermissionDecision::Allow => GateOutcome::Allow,
            PermissionDecision::Deny { reason } => GateOutcome::Block { reason },
            PermissionDecision::Ask { ticket_id } => GateOutcome::Suspend { ticket_id },
        }
    }
}
