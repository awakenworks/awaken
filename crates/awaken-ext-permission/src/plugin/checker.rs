use async_trait::async_trait;

use awaken_contract::StateError;
use awaken_contract::contract::tool_intercept::ToolInterceptPayload;
use awaken_runtime::{PhaseContext, ToolGateHook};

use crate::rules::{ToolPermissionBehavior, evaluate_tool_permission};
use crate::state::{PermissionOverridesKey, PermissionPolicyKey, permission_rules_from_state};

/// Tool gate hook that evaluates permission rules before a tool call executes.
///
/// - `Allow` → no intercept (tool proceeds normally)
/// - `Deny` → returns `Block`
/// - `Ask` → returns `Suspend` (awaits external approval)
///
/// On resume after `Ask`, checks `resume_input` — if approved, proceeds.
pub(super) struct PermissionToolGateHook;

#[async_trait]
impl ToolGateHook for PermissionToolGateHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<Option<ToolInterceptPayload>, StateError> {
        let tool_name = match &ctx.tool_name {
            Some(name) => name.as_str(),
            None => return Ok(None),
        };
        let tool_args = ctx.tool_args.clone().unwrap_or_default();

        let is_resume = ctx.resume_input.as_ref().is_some_and(|r| {
            r.action == awaken_contract::contract::suspension::ResumeDecisionAction::Resume
        });

        let policy = ctx.state::<PermissionPolicyKey>();
        let overrides = ctx.state::<PermissionOverridesKey>();
        let ruleset = permission_rules_from_state(policy, overrides);
        let evaluation = evaluate_tool_permission(&ruleset, tool_name, &tool_args);

        let intercept = match evaluation.behavior {
            ToolPermissionBehavior::Allow => None,
            ToolPermissionBehavior::Deny => Some(ToolInterceptPayload::Block {
                reason: format!("Tool '{}' denied by permission rules", tool_name),
            }),
            ToolPermissionBehavior::Ask => {
                if is_resume {
                    None
                } else {
                    use awaken_contract::contract::suspension::{
                        PendingToolCall, SuspendTicket, Suspension, ToolCallResumeMode,
                    };
                    let call_id = ctx.tool_call_id.as_deref().unwrap_or("");
                    Some(ToolInterceptPayload::Suspend(SuspendTicket::new(
                        Suspension {
                            id: format!("perm_{call_id}"),
                            action: "tool:PermissionConfirm".into(),
                            message: format!("Permission required for tool '{tool_name}'"),
                            parameters: tool_args.clone(),
                            ..Default::default()
                        },
                        PendingToolCall::new(
                            format!("perm_{call_id}"),
                            "permission_confirm",
                            tool_args,
                        ),
                        ToolCallResumeMode::ReplayToolCall,
                    )))
                }
            }
        };

        Ok(intercept)
    }
}
