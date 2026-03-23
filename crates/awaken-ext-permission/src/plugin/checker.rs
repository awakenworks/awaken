use async_trait::async_trait;

use awaken_contract::StateError;
use awaken_contract::contract::tool_intercept::{ToolInterceptAction, ToolInterceptPayload};
use awaken_runtime::state::StateCommand;
use awaken_runtime::{PhaseContext, PhaseHook};

use crate::rules::{ToolPermissionBehavior, evaluate_tool_permission};
use crate::state::{PermissionOverridesKey, PermissionPolicyKey, permission_rules_from_state};

/// BeforeToolExecute hook that evaluates permission rules and schedules
/// `ToolInterceptAction` to block or suspend tool calls.
///
/// - `Allow` → no intercept (tool proceeds normally)
/// - `Deny` → schedules `Block` intercept
/// - `Ask` → schedules `Suspend` intercept (awaits external approval)
///
/// On resume after `Ask`, checks `resume_input` — if approved, proceeds.
pub(super) struct PermissionInterceptHook;

#[async_trait]
impl PhaseHook for PermissionInterceptHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let tool_name = match &ctx.tool_name {
            Some(name) => name.as_str(),
            None => return Ok(StateCommand::new()),
        };
        let tool_args = ctx.tool_args.clone().unwrap_or_default();

        // If resuming after permission approval, proceed (no intercept)
        if ctx.resume_input.as_ref().is_some_and(|r| {
            r.action == awaken_contract::contract::suspension::ResumeDecisionAction::Resume
        }) {
            return Ok(StateCommand::new());
        }

        let policy = ctx.state::<PermissionPolicyKey>();
        let overrides = ctx.state::<PermissionOverridesKey>();
        let ruleset = permission_rules_from_state(policy, overrides);
        let evaluation = evaluate_tool_permission(&ruleset, tool_name, &tool_args);

        let mut cmd = StateCommand::new();
        match evaluation.behavior {
            ToolPermissionBehavior::Allow => {} // No intercept = proceed
            ToolPermissionBehavior::Deny => {
                cmd.schedule_action::<ToolInterceptAction>(ToolInterceptPayload::Block {
                    reason: format!("Tool '{}' denied by permission rules", tool_name),
                })?;
            }
            ToolPermissionBehavior::Ask => {
                // For now, suspend without a detailed ticket.
                // Future: build SuspendTicket with permission confirmation UI schema.
                use awaken_contract::contract::suspension::{
                    PendingToolCall, SuspendTicket, Suspension, ToolCallResumeMode,
                };
                let call_id = ctx.tool_call_id.as_deref().unwrap_or("");
                let ticket = SuspendTicket::new(
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
                );
                cmd.schedule_action::<ToolInterceptAction>(ToolInterceptPayload::Suspend(ticket))?;
            }
        }
        Ok(cmd)
    }
}
