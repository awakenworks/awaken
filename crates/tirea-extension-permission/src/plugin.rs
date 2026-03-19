use super::actions::{apply_tool_policy, reject_out_of_scope};
use super::config::PermissionRulesConfig;
use super::mechanism::{enforce_permission, PermissionMechanismDecision, PermissionMechanismInput};
use super::model::PermissionRuleset;
use super::state::{PermissionOverrides, PermissionPolicy};
use super::strategy::evaluate_tool_permission;
use async_trait::async_trait;
use serde_json::Value;
use tirea_contract::runtime::behavior::{AgentBehavior, ReadOnlyContext};
use tirea_contract::runtime::phase::{ActionSet, BeforeInferenceAction, BeforeToolExecuteAction};
use tirea_contract::runtime::run::config::AgentRunConfig;
use tirea_contract::scope;

/// Stable plugin id for permission actions.
pub const PERMISSION_PLUGIN_ID: &str = "permission";

/// Build a merged ruleset from config-layer rules (AgentRunConfig) and
/// state-layer rules (thread snapshot). State rules take precedence.
fn merged_ruleset(run_config: &AgentRunConfig, snapshot: &Value) -> PermissionRuleset {
    let mut ruleset = super::state::permission_rules_from_snapshot(snapshot);

    // Layer config rules underneath: only insert if key is absent in state.
    if let Some(config) = run_config.extensions().get::<PermissionRulesConfig>() {
        for rule in config.rules() {
            let key = rule.subject.key();
            ruleset.rules.entry(key).or_insert_with(|| rule.clone());
        }
    }

    ruleset
}

/// Permission strategy plugin.
///
/// Checks permissions in `before_inference` (visibility filtering) and
/// `before_tool_execute` (enforcement). Merges config-layer rules from
/// `AgentRunConfig.extensions` with state-layer rules from thread snapshot.
pub struct PermissionPlugin;

#[async_trait]
impl AgentBehavior for PermissionPlugin {
    fn id(&self) -> &str {
        PERMISSION_PLUGIN_ID
    }

    tirea_contract::declare_plugin_states!(PermissionPolicy, PermissionOverrides);

    async fn before_inference(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        let snapshot = ctx.snapshot();
        let ruleset = merged_ruleset(ctx.run_config(), &snapshot);
        let denied = ruleset.unconditionally_denied_tools();
        if denied.is_empty() {
            return ActionSet::empty();
        }
        denied
            .into_iter()
            .map(|id| BeforeInferenceAction::ExcludeTool(id.to_string()))
            .collect::<Vec<_>>()
            .into()
    }

    async fn before_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeToolExecuteAction> {
        let Some(tool_id) = ctx.tool_name() else {
            return ActionSet::empty();
        };

        let snapshot = ctx.snapshot();
        let ruleset = merged_ruleset(ctx.run_config(), &snapshot);
        let tool_args = ctx.tool_args().cloned().unwrap_or_default();
        let evaluation = evaluate_tool_permission(&ruleset, tool_id, &tool_args);
        let decision = enforce_permission(
            PermissionMechanismInput {
                tool_id,
                tool_args: ctx.tool_args().cloned().unwrap_or_default(),
                call_id: ctx.tool_call_id(),
                resume_action: ctx.resume_input().map(|resume| resume.action.clone()),
            },
            &evaluation,
        );

        match decision {
            PermissionMechanismDecision::Proceed => ActionSet::empty(),
            PermissionMechanismDecision::Action(action) => ActionSet::single(*action),
        }
    }
}

/// Tool scope policy plugin.
///
/// Enforces allow/deny list filtering via typed `RunPolicy` policy fields.
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
        let run_policy = ctx.run_policy();
        let allowed = run_policy.allowed_tools();
        let excluded = run_policy.excluded_tools();

        if allowed.is_none() && excluded.is_none() {
            return ActionSet::empty();
        }
        apply_tool_policy(
            allowed.map(|values| values.to_vec()),
            excluded.map(|values| values.to_vec()),
        )
        .into()
    }

    async fn before_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeToolExecuteAction> {
        let Some(tool_id) = ctx.tool_name() else {
            return ActionSet::empty();
        };

        let run_policy = ctx.run_policy();
        if !scope::is_scope_allowed(Some(run_policy), tool_id, scope::ScopeDomain::Tool) {
            ActionSet::single(reject_out_of_scope(tool_id))
        } else {
            ActionSet::empty()
        }
    }
}
