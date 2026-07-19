//! The permission gate — the runtime's single authorization choke point.
//!
//! [`PermissionGate`] is the [`ToolGateHook`] the loop calls before every
//! protected tool call. It asks an injected [`ToolPermissionPolicy`] for a decision
//! and maps it onto a [`GateOutcome`] (ADR-0030). It is the only path that can let
//! a protected operation proceed (G21); visibility, selection, capability, and
//! health never reach this decision. The concrete rule policy (patterns, modes)
//! is an extension; the runtime owns only this mapping.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::state::Store;
use awaken_runtime_contract::permission::{
    GateOutcome, ToolCall, ToolGateHook, ToolPermissionPolicy, ToolPermissionVerdict,
};

/// A [`ToolGateHook`] backed by a [`ToolPermissionPolicy`].
pub struct PermissionGate {
    policy: Arc<dyn ToolPermissionPolicy>,
}

impl PermissionGate {
    /// Gate every protected tool call through `policy`.
    pub fn new(policy: Arc<dyn ToolPermissionPolicy>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl ToolGateHook for PermissionGate {
    fn id(&self) -> &str {
        "permission"
    }

    async fn gate(&self, call: &ToolCall, _state: &Store) -> GateOutcome {
        // allow → execute; deny → a model-visible block; ask → await on a decision
        // ticket the operator resumes (ADR-0030 D1/D2).
        match self.policy.evaluate(call).await {
            ToolPermissionVerdict::Allow => GateOutcome::Allow,
            ToolPermissionVerdict::Deny { reason } => GateOutcome::Block { reason },
            ToolPermissionVerdict::RequireConfirmation { correlation_id } => {
                GateOutcome::RequireConfirmation { correlation_id }
            }
        }
    }
}
