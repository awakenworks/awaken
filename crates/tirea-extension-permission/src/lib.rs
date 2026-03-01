//! Permission policy extension.
//!
//! External callers only depend on [`PermissionAction`]. Internal permission
//! state/reducer details are handled by [`PermissionPlugin`].

pub mod scope;
pub use scope::*;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use tirea_contract::io::ResumeDecisionAction;
use tirea_contract::runtime::behavior::{AgentBehavior, ReadOnlyContext};
use tirea_contract::runtime::action::Action;
use tirea_contract::runtime::inference::InferenceContext;
use tirea_contract::runtime::tool_call::ToolGate;
use tirea_contract::runtime::state::{AnyStateAction, StateSpec};
use tirea_contract::runtime::phase::step::StepContext;
use tirea_contract::runtime::phase::{Phase, SuspendTicket};
use tirea_contract::runtime::{PendingToolCall, ToolCallResumeMode};
use tirea_state::State;

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

/// Public helper to wrap a `PermissionAction` into an `AnyStateAction`.
///
/// This avoids exposing `PermissionState` publicly while letting external
/// callers (e.g. `SkillActivateTool`) emit permission state mutations.
pub fn permission_state_action(action: PermissionAction) -> AnyStateAction {
    AnyStateAction::new::<PermissionState>(action)
}

/// Persisted permission state (internal).
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[serde(default)]
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
    let perms_value = snapshot.get(PermissionState::PATH);

    // Happy path: full struct deserializes cleanly.
    if let Some(perms) = perms_value.and_then(|v| PermissionState::from_value(v).ok()) {
        return perms
            .tools
            .get(tool_id)
            .copied()
            .unwrap_or(perms.default_behavior);
    }

    // Fallback for corrupted tools map: honor default_behavior if parseable.
    perms_value
        .and_then(|v| v.get("default_behavior"))
        .and_then(|v| serde_json::from_value::<ToolPermissionBehavior>(v.clone()).ok())
        .unwrap_or_default()
}

// =============================================================================
// Permission-domain Actions
// =============================================================================

/// Block tool execution with a denial reason.
pub struct DenyTool(pub String);

impl Action for DenyTool {
    fn label(&self) -> &'static str {
        "block_tool"
    }

    fn validate(&self, phase: Phase) -> Result<(), String> {
        if phase == Phase::BeforeToolExecute {
            Ok(())
        } else {
            Err(format!(
                "DenyTool is only allowed in BeforeToolExecute, got {phase}"
            ))
        }
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        if let Some(gate) = step.extensions.get_mut::<ToolGate>() {
            gate.blocked = true;
            gate.block_reason = Some(self.0);
            gate.pending = false;
            gate.suspend_ticket = None;
        }
    }
}

/// Suspend tool execution pending user permission confirmation.
pub struct RequestPermission(pub SuspendTicket);

impl Action for RequestPermission {
    fn label(&self) -> &'static str {
        "suspend_tool"
    }

    fn validate(&self, phase: Phase) -> Result<(), String> {
        if phase == Phase::BeforeToolExecute {
            Ok(())
        } else {
            Err(format!(
                "RequestPermission is only allowed in BeforeToolExecute, got {phase}"
            ))
        }
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        if let Some(gate) = step.extensions.get_mut::<ToolGate>() {
            gate.blocked = false;
            gate.block_reason = None;
            gate.pending = true;
            gate.suspend_ticket = Some(self.0);
        }
    }
}

/// Apply tool policy: keep only allowed tools, remove excluded ones.
pub struct ApplyToolPolicy {
    pub allowed: Option<Vec<String>>,
    pub excluded: Option<Vec<String>>,
}

impl Action for ApplyToolPolicy {
    fn label(&self) -> &'static str {
        "apply_tool_policy"
    }

    fn validate(&self, phase: Phase) -> Result<(), String> {
        if phase == Phase::BeforeInference {
            Ok(())
        } else {
            Err(format!(
                "ApplyToolPolicy is only allowed in BeforeInference, got {phase}"
            ))
        }
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        let inf = step.extensions.get_or_default::<InferenceContext>();
        if let Some(allowed) = &self.allowed {
            inf.tools.retain(|t| allowed.iter().any(|id| id == &t.id));
        }
        if let Some(excluded) = &self.excluded {
            for id in excluded {
                inf.tools.retain(|t| t.id != *id);
            }
        }
    }
}

/// Block tool execution due to policy violation.
pub struct RejectPolicyViolation(pub String);

impl Action for RejectPolicyViolation {
    fn label(&self) -> &'static str {
        "block_tool"
    }

    fn validate(&self, phase: Phase) -> Result<(), String> {
        if phase == Phase::BeforeToolExecute {
            Ok(())
        } else {
            Err(format!(
                "RejectPolicyViolation is only allowed in BeforeToolExecute, got {phase}"
            ))
        }
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        if let Some(gate) = step.extensions.get_mut::<ToolGate>() {
            gate.blocked = true;
            gate.block_reason = Some(self.0);
            gate.pending = false;
            gate.suspend_ticket = None;
        }
    }
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

    async fn before_tool_execute(&self, ctx: &ReadOnlyContext<'_>) -> Vec<Box<dyn Action>> {
        let Some(tool_id) = ctx.tool_name() else {
            return vec![];
        };

        let call_id = ctx.tool_call_id().unwrap_or_default().to_string();
        if !call_id.is_empty() {
            let has_resume_grant = ctx
                .resume_input()
                .is_some_and(|resume| matches!(resume.action, ResumeDecisionAction::Resume));
            if has_resume_grant {
                return vec![];
            }
        }

        let snapshot = ctx.snapshot();
        let permission = resolve_permission_behavior(&snapshot, tool_id);

        match permission {
            ToolPermissionBehavior::Allow => vec![],
            ToolPermissionBehavior::Deny => {
                vec![Box::new(DenyTool(format!("Tool '{}' is denied", tool_id)))]
            }
            ToolPermissionBehavior::Ask => {
                if call_id.is_empty() {
                    return vec![Box::new(DenyTool(
                        "Permission check requires non-empty tool call id".to_string(),
                    ))];
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
                vec![Box::new(RequestPermission(SuspendTicket::new(
                    suspension,
                    PendingToolCall::new(pending_call_id, PERMISSION_CONFIRM_TOOL_NAME, arguments),
                    ToolCallResumeMode::ReplayToolCall,
                )))]
            }
        }
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

    async fn before_inference(&self, ctx: &ReadOnlyContext<'_>) -> Vec<Box<dyn Action>> {
        let run_config = ctx.run_config();
        let allowed = scope::parse_scope_filter(run_config.value(SCOPE_ALLOWED_TOOLS_KEY));
        let excluded = scope::parse_scope_filter(run_config.value(SCOPE_EXCLUDED_TOOLS_KEY));

        if allowed.is_none() && excluded.is_none() {
            return vec![];
        }
        vec![Box::new(ApplyToolPolicy { allowed, excluded })]
    }

    async fn before_tool_execute(&self, ctx: &ReadOnlyContext<'_>) -> Vec<Box<dyn Action>> {
        let Some(tool_id) = ctx.tool_name() else {
            return vec![];
        };

        let run_config = ctx.run_config();
        if !scope::is_scope_allowed(
            Some(run_config),
            tool_id,
            SCOPE_ALLOWED_TOOLS_KEY,
            SCOPE_EXCLUDED_TOOLS_KEY,
        ) {
            vec![Box::new(RejectPolicyViolation(format!(
                "Tool '{}' is not allowed by current policy",
                tool_id
            )))]
        } else {
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tirea_contract::runtime::phase::Phase;
    use tirea_contract::io::ResumeDecisionAction;
    use tirea_contract::runtime::tool_call::ToolCallResume;
    use tirea_contract::RunConfig;
    use tirea_state::DocCell;

    fn has_block(actions: &[Box<dyn Action>]) -> bool {
        actions.iter().any(|a| a.label() == "block_tool")
    }

    fn has_suspend(actions: &[Box<dyn Action>]) -> bool {
        actions.iter().any(|a| a.label() == "suspend_tool")
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
    fn test_permission_state_action_helper() {
        let action = PermissionAction::SetDefault {
            behavior: ToolPermissionBehavior::Allow,
        };
        let state_action = permission_state_action(action);
        // Verify it produces a valid AnyStateAction (not Patch variant)
        assert!(!matches!(state_action, AnyStateAction::Patch(_)));
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

        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(!has_block(&actions));
        assert!(!has_suspend(&actions));
    }

    #[tokio::test]
    async fn test_permission_plugin_deny() {
        let config = RunConfig::new();
        let doc =
            DocCell::new(json!({ "permissions": { "default_behavior": "deny", "tools": {} } }));
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("any_tool", "call_1", Some(&args));

        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_block(&actions));
    }

    #[tokio::test]
    async fn test_permission_plugin_ask() {
        let config = RunConfig::new();
        let doc =
            DocCell::new(json!({ "permissions": { "default_behavior": "ask", "tools": {} } }));
        let args = json!({"path": "a.txt"});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("test_tool", "call_1", Some(&args));

        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_suspend(&actions));
    }

    #[tokio::test]
    async fn test_permission_plugin_ask_with_empty_call_id_blocks() {
        let config = RunConfig::new();
        let doc =
            DocCell::new(json!({ "permissions": { "default_behavior": "ask", "tools": {} } }));
        let args = json!({"path": "a.txt"});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("test_tool", "", Some(&args));

        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_block(&actions));
        assert!(!has_suspend(&actions));
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

        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(!has_block(&actions));
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

        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_block(&actions));
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

        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_suspend(&actions));
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

        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        // Should fall back to default "allow" behavior
        assert!(!has_block(&actions));
        assert!(!has_suspend(&actions));
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

        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        // Should fall back to Ask behavior
        assert!(has_suspend(&actions));
    }

    #[tokio::test]
    async fn test_permission_plugin_no_state() {
        // Thread with no permission state at all — should default to Ask
        let config = RunConfig::new();
        let doc = DocCell::new(json!({}));
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("any_tool", "call_1", Some(&args));

        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_suspend(&actions));
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

        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        // Falls back to default "allow" behavior
        assert!(!has_block(&actions));
        assert!(!has_suspend(&actions));
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

        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        // Falls back to Ask
        assert!(has_suspend(&actions));
    }

    #[tokio::test]
    async fn test_permission_plugin_default_behavior_is_number() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({ "permissions": { "default_behavior": 42, "tools": {} } }));
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("any_tool", "call_1", Some(&args));

        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        // Falls back to Ask
        assert!(has_suspend(&actions));
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

        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        // Falls back to default "allow"
        assert!(!has_block(&actions));
        assert!(!has_suspend(&actions));
    }

    #[tokio::test]
    async fn test_permission_plugin_permissions_is_array() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({ "permissions": [1, 2, 3] }));
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("any_tool", "call_1", Some(&args));

        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        // Falls back to Ask
        assert!(has_suspend(&actions));
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

        let actions = AgentBehavior::before_tool_execute(&ToolPolicyPlugin, &ctx).await;
        assert!(has_block(&actions), "out-of-scope tool should be blocked");
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

        let actions = AgentBehavior::before_tool_execute(&ToolPolicyPlugin, &ctx).await;
        assert!(!has_block(&actions));
    }

    #[tokio::test]
    async fn test_tool_policy_no_filters_allows_all() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({}));
        let args = json!({});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("any_tool", "call_1", Some(&args));

        let actions = AgentBehavior::before_tool_execute(&ToolPolicyPlugin, &ctx).await;
        assert!(!has_block(&actions));
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

        let actions = AgentBehavior::before_tool_execute(&ToolPolicyPlugin, &ctx).await;
        assert!(has_block(&actions), "excluded tool should be blocked");
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

        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(
            !has_block(&actions),
            "resume-approved call should be allowed"
        );
        assert!(
            !has_suspend(&actions),
            "resume-approved call should not suspend again"
        );
    }
}
