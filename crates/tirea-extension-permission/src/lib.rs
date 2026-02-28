//! Permission policy extension.
//!
//! External callers only depend on [`PermissionAction`]. Internal permission
//! state/reducer details are handled by [`PermissionPlugin`].

pub mod scope;
pub use scope::*;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::borrow::Cow;
use std::collections::HashMap;
use tirea_contract::io::ResumeDecisionAction;
use tirea_contract::runtime::plugin::agent::{AgentBehavior, ReadOnlyContext};
use tirea_contract::runtime::plugin::phase::effect::PhaseOutput;
use tirea_contract::runtime::plugin::phase::state_spec::{
    reduce_state_actions, AnyStateAction, StateSpec,
};
use tirea_contract::runtime::plugin::phase::{AnyPluginAction, SuspendTicket};
use tirea_contract::runtime::{PendingToolCall, ToolCallResumeMode};
use tirea_state::{State, TrackedPatch};

/// Tool permission behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPermissionBehavior {
    /// Tool is allowed without confirmation.
    Allow,
    /// Tool requires user confirmation before execution.
    #[default]
    Ask,
    /// Tool is denied (will not execute).
    Deny,
}

/// Public permission-domain action exposed to tools/plugins.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PermissionAction {
    /// Set default behavior for tools with no override.
    SetDefault { behavior: ToolPermissionBehavior },
    /// Set behavior override for a specific tool.
    SetTool {
        tool_id: String,
        behavior: ToolPermissionBehavior,
    },
    /// Remove a specific tool override.
    RemoveTool { tool_id: String },
    /// Remove all per-tool overrides.
    ClearTools,
}

/// Stable plugin id for permission actions.
pub const PERMISSION_PLUGIN_ID: &str = "permission";

impl From<PermissionAction> for AnyPluginAction {
    fn from(action: PermissionAction) -> Self {
        AnyPluginAction::new(PERMISSION_PLUGIN_ID, action)
    }
}

/// Persisted permission state (internal).
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(path = "permissions")]
struct PermissionState {
    /// Default behavior for tools not explicitly configured.
    pub default_behavior: ToolPermissionBehavior,
    /// Per-tool permission overrides.
    pub tools: HashMap<String, ToolPermissionBehavior>,
}

impl StateSpec for PermissionState {
    type Action = PermissionAction;

    fn reduce(&mut self, action: Self::Action) {
        match action {
            PermissionAction::SetDefault { behavior } => {
                self.default_behavior = behavior;
            }
            PermissionAction::SetTool { tool_id, behavior } => {
                self.tools.insert(tool_id, behavior);
            }
            PermissionAction::RemoveTool { tool_id } => {
                self.tools.remove(&tool_id);
            }
            PermissionAction::ClearTools => {
                self.tools.clear();
            }
        }
    }
}

/// Frontend tool name for permission confirmation prompts.
pub const PERMISSION_CONFIRM_TOOL_NAME: &str = "PermissionConfirm";

/// Resolve effective permission behavior from a state snapshot.
#[must_use]
pub fn resolve_permission_behavior(
    snapshot: &serde_json::Value,
    tool_id: &str,
) -> ToolPermissionBehavior {
    let perms = snapshot
        .get("permissions")
        .unwrap_or(&serde_json::Value::Null);

    let tool_permission = perms
        .get("tools")
        .and_then(|tools| tools.get(tool_id))
        .and_then(|v| serde_json::from_value::<ToolPermissionBehavior>(v.clone()).ok());

    tool_permission.unwrap_or_else(|| {
        perms
            .get("default_behavior")
            .and_then(|v| serde_json::from_value::<ToolPermissionBehavior>(v.clone()).ok())
            .unwrap_or_default()
    })
}

/// Permission strategy plugin.
///
/// This plugin checks permissions in `before_tool_execute`.
/// - `Allow`: no-op
/// - `Deny`: block tool
/// - `Ask`: suspend the tool call and emit a confirmation ticket
pub struct PermissionPlugin;

#[async_trait]
impl AgentBehavior for PermissionPlugin {
    fn id(&self) -> &str {
        PERMISSION_PLUGIN_ID
    }

    async fn before_tool_execute(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        let Some(tool_id) = ctx.tool_name() else {
            return PhaseOutput::default();
        };

        let call_id = ctx.tool_call_id().unwrap_or_default().to_string();
        if !call_id.is_empty() {
            let has_resume_grant = ctx
                .resume_input()
                .is_some_and(|resume| matches!(resume.action, ResumeDecisionAction::Resume));
            if has_resume_grant {
                return PhaseOutput::default();
            }
        }

        let snapshot = ctx.snapshot();
        let permission = resolve_permission_behavior(&snapshot, tool_id);

        match permission {
            ToolPermissionBehavior::Allow => PhaseOutput::default(),
            ToolPermissionBehavior::Deny => {
                PhaseOutput::new().block_tool(format!("Tool '{}' is denied", tool_id))
            }
            ToolPermissionBehavior::Ask => {
                if call_id.is_empty() {
                    return PhaseOutput::new()
                        .block_tool("Permission check requires non-empty tool call id");
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
                PhaseOutput::new().suspend_tool(SuspendTicket::new(
                    suspension,
                    PendingToolCall::new(pending_call_id, PERMISSION_CONFIRM_TOOL_NAME, arguments),
                    ToolCallResumeMode::ReplayToolCall,
                ))
            }
        }
    }

    fn reduce_plugin_actions(
        &self,
        actions: Vec<AnyPluginAction>,
        base_snapshot: &serde_json::Value,
    ) -> Result<Vec<TrackedPatch>, String> {
        let mut state_actions = Vec::new();
        for action in actions {
            if action.plugin_id() != PERMISSION_PLUGIN_ID {
                return Err(format!(
                    "permission plugin received action for unexpected plugin '{}'",
                    action.plugin_id()
                ));
            }
            let action = action.downcast::<PermissionAction>().map_err(|other| {
                format!(
                    "permission plugin failed to downcast action '{}'",
                    other.action_type_name()
                )
            })?;
            state_actions.push(AnyStateAction::new::<PermissionState>(action));
        }

        let snapshot_for_reduce = match base_snapshot.get("permissions") {
            Some(value) if !value.is_null() => Cow::Borrowed(base_snapshot),
            _ => {
                let mut snapshot = base_snapshot.clone();
                let Some(root) = snapshot.as_object_mut() else {
                    return Err(
                        "permission plugin reducer requires object root state snapshot".to_string(),
                    );
                };
                let default_state = serde_json::to_value(PermissionState::default())
                    .map_err(|err| format!("serialize default permission state failed: {err}"))?;
                root.insert("permissions".to_string(), default_state);
                Cow::Owned(snapshot)
            }
        };

        reduce_state_actions(
            state_actions,
            snapshot_for_reduce.as_ref(),
            "plugin:permission",
        )
        .map_err(|e| e.to_string())
    }
}

/// Tool scope policy plugin.
///
/// Enforces allow/deny list filtering for tools via `RunConfig` scope keys.
/// Should be installed before `PermissionPlugin` so that out-of-scope tools
/// are blocked before per-tool permission checks run.
pub struct ToolPolicyPlugin;

#[async_trait]
impl AgentBehavior for ToolPolicyPlugin {
    fn id(&self) -> &str {
        "tool_policy"
    }

    async fn before_inference(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        let run_config = ctx.run_config();
        let allowed = scope::parse_scope_filter(run_config.value(SCOPE_ALLOWED_TOOLS_KEY));
        let excluded = scope::parse_scope_filter(run_config.value(SCOPE_EXCLUDED_TOOLS_KEY));

        let mut output = PhaseOutput::new();
        if let Some(ref allowed) = allowed {
            output = output.include_only_tools(allowed.clone());
        }
        if let Some(ref excluded) = excluded {
            for id in excluded {
                output = output.exclude_tool(id);
            }
        }
        output
    }

    async fn before_tool_execute(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        let Some(tool_id) = ctx.tool_name() else {
            return PhaseOutput::default();
        };

        let run_config = ctx.run_config();
        if !scope::is_scope_allowed(
            Some(run_config),
            tool_id,
            SCOPE_ALLOWED_TOOLS_KEY,
            SCOPE_EXCLUDED_TOOLS_KEY,
        ) {
            PhaseOutput::new().block_tool(format!(
                "Tool '{}' is not allowed by current policy",
                tool_id
            ))
        } else {
            PhaseOutput::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tirea_contract::io::ResumeDecisionAction;
    use tirea_contract::runtime::plugin::phase::effect::PhaseEffect;
    use tirea_contract::runtime::plugin::phase::Phase;
    use tirea_contract::runtime::tool_call::ToolCallResume;
    use tirea_contract::RunConfig;
    use tirea_state::DocCell;

    fn has_block(output: &PhaseOutput) -> bool {
        output
            .effects
            .iter()
            .any(|e| matches!(e, PhaseEffect::BlockTool(_)))
    }

    fn has_suspend(output: &PhaseOutput) -> bool {
        output
            .effects
            .iter()
            .any(|e| matches!(e, PhaseEffect::SuspendTool(_)))
    }

    fn extract_suspend_ticket(output: &PhaseOutput) -> Option<&SuspendTicket> {
        output.effects.iter().find_map(|e| match e {
            PhaseEffect::SuspendTool(ticket) => Some(ticket),
            _ => None,
        })
    }

    #[test]
    fn test_permission_state_default() {
        let state = PermissionState::default();
        assert_eq!(state.default_behavior, ToolPermissionBehavior::Ask);
        assert!(state.tools.is_empty());
    }

    #[test]
    fn test_permission_state_serialization() {
        let mut state = PermissionState::default();
        state
            .tools
            .insert("read".to_string(), ToolPermissionBehavior::Allow);

        let json = serde_json::to_string(&state).unwrap();
        let parsed: PermissionState = serde_json::from_str(&json).unwrap();

        assert_eq!(
            parsed.tools.get("read"),
            Some(&ToolPermissionBehavior::Allow)
        );
    }

    #[test]
    fn test_resolve_permission_prefers_tool_override() {
        let snapshot = json!({
            "permissions": {
                "default_behavior": "deny",
                "tools": {
                    "recover_agent_run": "allow"
                }
            }
        });
        assert_eq!(
            resolve_permission_behavior(&snapshot, "recover_agent_run"),
            ToolPermissionBehavior::Allow
        );
    }

    #[test]
    fn test_resolve_permission_falls_back_to_default() {
        let snapshot = json!({
            "permissions": {
                "default_behavior": "deny",
                "tools": {}
            }
        });
        assert_eq!(
            resolve_permission_behavior(&snapshot, "unknown_tool"),
            ToolPermissionBehavior::Deny
        );
    }

    #[test]
    fn test_resolve_permission_missing_state_falls_back_to_ask() {
        assert_eq!(
            resolve_permission_behavior(&json!({}), "recover_agent_run"),
            ToolPermissionBehavior::Ask
        );
    }

    #[test]
    fn test_permission_plugin_reduces_permission_actions() {
        let base = json!({
            "permissions": {
                "default_behavior": "ask",
                "tools": {}
            }
        });
        let actions = vec![
            PermissionAction::SetTool {
                tool_id: "read_file".to_string(),
                behavior: ToolPermissionBehavior::Allow,
            }
            .into(),
            PermissionAction::SetDefault {
                behavior: ToolPermissionBehavior::Deny,
            }
            .into(),
        ];

        let patches = PermissionPlugin
            .reduce_plugin_actions(actions, &base)
            .expect("permission action reduce should succeed");
        let next = tirea_state::apply_patches(&base, patches.iter().map(|p| p.patch()))
            .expect("patches should apply");
        assert_eq!(
            resolve_permission_behavior(&next, "read_file"),
            ToolPermissionBehavior::Allow
        );
        assert_eq!(
            resolve_permission_behavior(&next, "unknown_tool"),
            ToolPermissionBehavior::Deny
        );
    }

    #[test]
    fn test_permission_plugin_id() {
        let plugin = PermissionPlugin;
        assert_eq!(AgentBehavior::id(&plugin), PERMISSION_PLUGIN_ID);
    }

    #[tokio::test]
    async fn test_permission_plugin_allow() {
        let config = RunConfig::new();
        let doc =
            DocCell::new(json!({ "permissions": { "default_behavior": "allow", "tools": {} } }));
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("any_tool", "call_1", Some(&args));

        let output = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(!has_block(&output));
        assert!(!has_suspend(&output));
    }

    #[tokio::test]
    async fn test_permission_plugin_deny() {
        let config = RunConfig::new();
        let doc =
            DocCell::new(json!({ "permissions": { "default_behavior": "deny", "tools": {} } }));
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("any_tool", "call_1", Some(&args));

        let output = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_block(&output));
    }

    #[tokio::test]
    async fn test_permission_plugin_ask() {
        let config = RunConfig::new();
        let doc =
            DocCell::new(json!({ "permissions": { "default_behavior": "ask", "tools": {} } }));
        let args = json!({"path": "a.txt"});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("test_tool", "call_1", Some(&args));

        let output = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_suspend(&output));

        let ticket = extract_suspend_ticket(&output).expect("suspend ticket should exist");
        assert_eq!(
            ticket.suspension.action,
            format!("tool:{}", PERMISSION_CONFIRM_TOOL_NAME)
        );
        assert_eq!(ticket.pending.id, "fc_call_1");
        assert_eq!(ticket.pending.name, PERMISSION_CONFIRM_TOOL_NAME);
        assert_eq!(ticket.pending.arguments["tool_name"], "test_tool");
        assert_eq!(ticket.pending.arguments["tool_args"]["path"], "a.txt");
        assert_eq!(ticket.resume_mode, ToolCallResumeMode::ReplayToolCall);
    }

    #[tokio::test]
    async fn test_permission_plugin_ask_with_empty_call_id_blocks() {
        let config = RunConfig::new();
        let doc =
            DocCell::new(json!({ "permissions": { "default_behavior": "ask", "tools": {} } }));
        let args = json!({"path": "a.txt"});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("test_tool", "", Some(&args));

        let output = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_block(&output));
        assert!(!has_suspend(&output));
    }

    #[test]
    fn test_resolve_default_permission() {
        let snapshot = json!({
            "permissions": {
                "default_behavior": "allow",
                "tools": {}
            }
        });
        assert_eq!(
            resolve_permission_behavior(&snapshot, "unknown_tool"),
            ToolPermissionBehavior::Allow
        );
    }

    #[test]
    fn test_resolve_default_permission_deny() {
        let snapshot = json!({
            "permissions": {
                "default_behavior": "deny",
                "tools": {}
            }
        });
        assert_eq!(
            resolve_permission_behavior(&snapshot, "unknown_tool"),
            ToolPermissionBehavior::Deny
        );
    }

    #[tokio::test]
    async fn test_permission_plugin_tool_specific_allow() {
        let config = RunConfig::new();
        let doc = DocCell::new(
            json!({ "permissions": { "default_behavior": "deny", "tools": { "allowed_tool": "allow" } } }),
        );
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("allowed_tool", "call_1", Some(&args));

        let output = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(!has_block(&output));
    }

    #[tokio::test]
    async fn test_permission_plugin_tool_specific_deny() {
        let config = RunConfig::new();
        let doc = DocCell::new(
            json!({ "permissions": { "default_behavior": "allow", "tools": { "denied_tool": "deny" } } }),
        );
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("denied_tool", "call_1", Some(&args));

        let output = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_block(&output));
    }

    #[tokio::test]
    async fn test_permission_plugin_tool_specific_ask() {
        let config = RunConfig::new();
        let doc = DocCell::new(
            json!({ "permissions": { "default_behavior": "allow", "tools": { "ask_tool": "ask" } } }),
        );
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("ask_tool", "call_1", Some(&args));

        let output = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_suspend(&output));
    }

    #[tokio::test]
    async fn test_permission_plugin_invalid_tool_behavior() {
        let config = RunConfig::new();
        let doc = DocCell::new(
            json!({ "permissions": { "default_behavior": "allow", "tools": { "invalid_tool": "invalid_behavior" } } }),
        );
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("invalid_tool", "call_1", Some(&args));

        let output = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        // Should fall back to default "allow" behavior
        assert!(!has_block(&output));
        assert!(!has_suspend(&output));
    }

    #[tokio::test]
    async fn test_permission_plugin_invalid_default_behavior() {
        let config = RunConfig::new();
        let doc = DocCell::new(
            json!({ "permissions": { "default_behavior": "invalid_default", "tools": {} } }),
        );
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("any_tool", "call_1", Some(&args));

        let output = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        // Should fall back to Ask behavior
        assert!(has_suspend(&output));
    }

    #[tokio::test]
    async fn test_permission_plugin_no_state() {
        // Thread with no permission state at all — should default to Ask
        let config = RunConfig::new();
        let doc = DocCell::new(json!({}));
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("any_tool", "call_1", Some(&args));

        let output = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_suspend(&output));
    }

    // ========================================================================
    // Corrupted / unexpected state shape fallback tests
    // ========================================================================

    #[tokio::test]
    async fn test_permission_plugin_tools_is_string_not_object() {
        let config = RunConfig::new();
        let doc = DocCell::new(
            json!({ "permissions": { "default_behavior": "allow", "tools": "corrupted" } }),
        );
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("any_tool", "call_1", Some(&args));

        let output = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        // Falls back to default "allow" behavior
        assert!(!has_block(&output));
        assert!(!has_suspend(&output));
    }

    #[tokio::test]
    async fn test_permission_plugin_default_behavior_invalid_string() {
        let config = RunConfig::new();
        let doc = DocCell::new(
            json!({ "permissions": { "default_behavior": "invalid_value", "tools": {} } }),
        );
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("any_tool", "call_1", Some(&args));

        let output = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        // Falls back to Ask
        assert!(has_suspend(&output));
    }

    #[tokio::test]
    async fn test_permission_plugin_default_behavior_is_number() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({ "permissions": { "default_behavior": 42, "tools": {} } }));
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("any_tool", "call_1", Some(&args));

        let output = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        // Falls back to Ask
        assert!(has_suspend(&output));
    }

    #[tokio::test]
    async fn test_permission_plugin_tool_value_is_number() {
        let config = RunConfig::new();
        let doc = DocCell::new(
            json!({ "permissions": { "default_behavior": "allow", "tools": { "my_tool": 123 } } }),
        );
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("my_tool", "call_1", Some(&args));

        let output = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        // Falls back to default "allow"
        assert!(!has_block(&output));
        assert!(!has_suspend(&output));
    }

    #[tokio::test]
    async fn test_permission_plugin_permissions_is_array() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({ "permissions": [1, 2, 3] }));
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("any_tool", "call_1", Some(&args));

        let output = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        // Falls back to Ask
        assert!(has_suspend(&output));
    }

    // ========================================================================
    // ToolPolicyPlugin tests
    // ========================================================================

    #[test]
    fn test_tool_policy_plugin_id() {
        assert_eq!(AgentBehavior::id(&ToolPolicyPlugin), "tool_policy");
    }

    #[tokio::test]
    async fn test_tool_policy_blocks_out_of_scope() {
        let mut config = RunConfig::new();
        config
            .set(scope::SCOPE_ALLOWED_TOOLS_KEY, vec!["other_tool"])
            .unwrap();
        let doc = DocCell::new(json!({}));
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("blocked_tool", "call_1", Some(&args));

        let output = AgentBehavior::before_tool_execute(&ToolPolicyPlugin, &ctx).await;
        assert!(has_block(&output), "out-of-scope tool should be blocked");
    }

    #[tokio::test]
    async fn test_tool_policy_allows_in_scope() {
        let mut config = RunConfig::new();
        config
            .set(scope::SCOPE_ALLOWED_TOOLS_KEY, vec!["my_tool"])
            .unwrap();
        let doc = DocCell::new(json!({}));
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("my_tool", "call_1", Some(&args));

        let output = AgentBehavior::before_tool_execute(&ToolPolicyPlugin, &ctx).await;
        assert!(!has_block(&output));
    }

    #[tokio::test]
    async fn test_tool_policy_no_filters_allows_all() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({}));
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("any_tool", "call_1", Some(&args));

        let output = AgentBehavior::before_tool_execute(&ToolPolicyPlugin, &ctx).await;
        assert!(!has_block(&output));
    }

    #[tokio::test]
    async fn test_tool_policy_excluded_tool_is_blocked() {
        let mut config = RunConfig::new();
        config
            .set(scope::SCOPE_EXCLUDED_TOOLS_KEY, vec!["excluded_tool"])
            .unwrap();
        let doc = DocCell::new(json!({}));
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("excluded_tool", "call_1", Some(&args));

        let output = AgentBehavior::before_tool_execute(&ToolPolicyPlugin, &ctx).await;
        assert!(has_block(&output), "excluded tool should be blocked");
    }

    #[tokio::test]
    async fn test_permission_resume_input_bypasses_ask() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({
            "permissions": {
                "default_behavior": "ask",
                "tools": {}
            }
        }));
        let args = json!({});
        let resume = ToolCallResume {
            decision_id: "fc_call_1".to_string(),
            action: ResumeDecisionAction::Resume,
            result: serde_json::Value::Bool(true),
            reason: None,
            updated_at: 1,
        };
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("test_tool", "call_1", Some(&args))
            .with_resume_input(resume);

        let output = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(
            !has_block(&output),
            "resume-approved call should be allowed"
        );
        assert!(
            !has_suspend(&output),
            "resume-approved call should not suspend again"
        );
    }
}
