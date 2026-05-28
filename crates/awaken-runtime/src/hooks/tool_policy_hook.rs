use std::sync::Arc;

use async_trait::async_trait;

use crate::PhaseContext;
use crate::phase::ToolGateHook;
use awaken_runtime_contract::StateError;
use awaken_runtime_contract::contract::tool_intercept::{
    ToolInterceptPayload, ToolPolicyContext, ToolPolicyDecision,
};

/// Typed policy hook layered on top of the existing ToolGate phase.
#[async_trait]
pub trait ToolPolicyHook: Send + Sync + 'static {
    async fn decide(&self, ctx: &ToolPolicyContext) -> Result<ToolPolicyDecision, StateError>;
}

pub(crate) type ToolPolicyHookArc = Arc<dyn ToolPolicyHook>;

pub(crate) struct ToolPolicyGateHook {
    hook: ToolPolicyHookArc,
}

impl ToolPolicyGateHook {
    pub(crate) fn new(hook: ToolPolicyHookArc) -> Self {
        Self { hook }
    }
}

#[async_trait]
impl ToolGateHook for ToolPolicyGateHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<Option<ToolInterceptPayload>, StateError> {
        let Some(policy_ctx) = ctx.tool_policy_context() else {
            return Ok(None);
        };
        let decision = self.hook.decide(&policy_ctx).await?;
        Ok(decision.into_intercept_payload())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::contract::tool_intercept::{RunMode, ToolKind};

    struct DenyScheduledExecute;

    #[async_trait]
    impl ToolPolicyHook for DenyScheduledExecute {
        async fn decide(&self, ctx: &ToolPolicyContext) -> Result<ToolPolicyDecision, StateError> {
            if ctx.run_mode == RunMode::Scheduled && ctx.tool_kind == ToolKind::Execute {
                return Ok(ToolPolicyDecision::Deny {
                    reason: "scheduled execute calls require explicit approval".into(),
                });
            }
            Ok(ToolPolicyDecision::Allow)
        }
    }

    #[tokio::test]
    async fn tool_policy_gate_hook_adapts_to_tool_gate_payload() {
        let hook = ToolPolicyGateHook::new(Arc::new(DenyScheduledExecute));
        let ctx = PhaseContext::new(
            awaken_runtime_contract::model::Phase::ToolGate,
            crate::state::Snapshot::new(0, Arc::new(crate::state::StateMap::default())),
        )
        .with_run_mode(RunMode::Scheduled)
        .with_tool_info("bash", "call-1", Some(serde_json::json!({"cmd": "echo"})));

        let payload = hook.run(&ctx).await.expect("policy should run");
        assert!(matches!(
            payload,
            Some(ToolInterceptPayload::Block { reason })
                if reason.contains("explicit approval")
        ));
    }
}
