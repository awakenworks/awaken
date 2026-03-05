use super::actions::{
    apply_tool_policy, deny_missing_call_id, deny_tool, reject_out_of_scope, request_permission,
};
use super::scope;
use super::state::{resolve_permission_behavior, PermissionPolicy, ToolPermissionBehavior};
use async_trait::async_trait;
use serde_json::json;
use tirea_contract::io::ResumeDecisionAction;
use tirea_contract::runtime::behavior::{AgentBehavior, ReadOnlyContext};
use tirea_contract::runtime::phase::SuspendTicket;
use tirea_contract::runtime::phase::{ActionSet, BeforeInferenceAction, BeforeToolExecuteAction};
use tirea_contract::runtime::{PendingToolCall, ToolCallResumeMode};

/// Stable plugin id for permission actions.
pub const PERMISSION_PLUGIN_ID: &str = "permission";

/// Frontend tool name for permission confirmation prompts.
pub const PERMISSION_CONFIRM_TOOL_NAME: &str = "PermissionConfirm";

/// Permission strategy plugin.
///
/// Checks permissions in `before_tool_execute`.
/// - `Allow`: no-op
/// - `Deny`: block tool
/// - `Ask`: suspend the tool call and emit a confirmation ticket
pub struct PermissionPlugin;

#[async_trait]
impl AgentBehavior for PermissionPlugin {
    fn id(&self) -> &str {
        PERMISSION_PLUGIN_ID
    }

    tirea_contract::declare_plugin_states!(PermissionPolicy, super::state::PermissionOverrides);

    async fn before_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeToolExecuteAction> {
        let Some(tool_id) = ctx.tool_name() else {
            return ActionSet::empty();
        };

        let call_id = ctx.tool_call_id().unwrap_or_default().to_string();
        if !call_id.is_empty() {
            let has_resume_grant = ctx
                .resume_input()
                .is_some_and(|resume| matches!(resume.action, ResumeDecisionAction::Resume));
            if has_resume_grant {
                return ActionSet::empty();
            }
        }

        let snapshot = ctx.snapshot();
        let permission = resolve_permission_behavior(&snapshot, tool_id);

        match permission {
            ToolPermissionBehavior::Allow => ActionSet::empty(),
            ToolPermissionBehavior::Deny => ActionSet::single(deny_tool(tool_id)),
            ToolPermissionBehavior::Ask => {
                if call_id.is_empty() {
                    return ActionSet::single(deny_missing_call_id());
                }
                let tool_args = ctx.tool_args().cloned().unwrap_or_default();
                let arguments = json!({
                    "tool_name": tool_id,
                    "tool_args": tool_args.clone(),
                });
                let pending_call_id = format!("fc_{call_id}");
                let suspension =
                    tirea_contract::Suspension::new(&pending_call_id, "tool:PermissionConfirm")
                        .with_parameters(arguments.clone());
                ActionSet::single(request_permission(SuspendTicket::new(
                    suspension,
                    PendingToolCall::new(pending_call_id, PERMISSION_CONFIRM_TOOL_NAME, arguments),
                    ToolCallResumeMode::ReplayToolCall,
                )))
            }
        }
    }
}

/// Tool scope policy plugin.
///
/// Enforces allow/deny list filtering via `RunConfig` scope keys.
/// Install before [`PermissionPlugin`] so out-of-scope tools are blocked first.
pub struct ToolPolicyPlugin;

#[async_trait]
impl AgentBehavior for ToolPolicyPlugin {
    fn id(&self) -> &str {
        "tool_policy"
    }

    async fn before_inference(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        let run_config = ctx.run_config();
        let allowed = scope::parse_scope_filter(run_config.value(scope::SCOPE_ALLOWED_TOOLS_KEY));
        let excluded = scope::parse_scope_filter(run_config.value(scope::SCOPE_EXCLUDED_TOOLS_KEY));

        if allowed.is_none() && excluded.is_none() {
            return ActionSet::empty();
        }
        apply_tool_policy(allowed, excluded).into()
    }

    async fn before_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeToolExecuteAction> {
        let Some(tool_id) = ctx.tool_name() else {
            return ActionSet::empty();
        };

        let run_config = ctx.run_config();
        if !scope::is_scope_allowed(
            Some(run_config),
            tool_id,
            scope::SCOPE_ALLOWED_TOOLS_KEY,
            scope::SCOPE_EXCLUDED_TOOLS_KEY,
        ) {
            ActionSet::single(reject_out_of_scope(tool_id))
        } else {
            ActionSet::empty()
        }
    }
}
