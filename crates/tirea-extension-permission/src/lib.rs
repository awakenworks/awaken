//! Permission policy extension.
//!
//! External callers use [`PermissionAction`], [`permission_state_action`],
//! [`PermissionPlugin`], and [`ToolPolicyPlugin`].

mod actions;
mod plugin;
mod state;
pub mod scope;

pub use actions::{ApplyToolPolicy, DenyTool, RejectPolicyViolation, RequestPermission};
pub use plugin::{PERMISSION_CONFIRM_TOOL_NAME, PERMISSION_PLUGIN_ID, PermissionPlugin, ToolPolicyPlugin};
pub use scope::*;
pub use state::{
    permission_state_action, resolve_permission_behavior, PermissionAction, PermissionPolicy,
    PermissionPolicyAction, ToolPermissionBehavior,
};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tirea_contract::io::ResumeDecisionAction;
    use tirea_contract::runtime::action::Action;
    use tirea_contract::runtime::behavior::AgentBehavior;
    use tirea_contract::runtime::phase::Phase;
    use tirea_contract::runtime::state::AnyStateAction;
    use tirea_contract::runtime::tool_call::ToolCallResume;
    use tirea_contract::RunConfig;
    use tirea_state::{DocCell, LatticeRegistry};

    fn has_block(actions: &[Box<dyn Action>]) -> bool {
        actions.iter().any(|a| a.label() == "block_tool")
    }

    fn has_suspend(actions: &[Box<dyn Action>]) -> bool {
        actions.iter().any(|a| a.label() == "suspend_tool")
    }

    #[test]
    fn test_permission_state_default() {
        let state = state::PermissionOverrides::default();
        assert_eq!(state.default_behavior, ToolPermissionBehavior::Ask);
        assert!(state.tools.is_empty());
    }

    #[test]
    fn test_permission_state_serialization() {
        let mut s = state::PermissionOverrides::default();
        s.tools.insert("read".to_string(), ToolPermissionBehavior::Allow);

        let json = serde_json::to_string(&s).unwrap();
        let parsed: state::PermissionOverrides = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.tools.get("read"), Some(&ToolPermissionBehavior::Allow));
    }

    #[test]
    fn test_resolve_permission_prefers_tool_override() {
        let snapshot = json!({
            "permissions": { "default_behavior": "deny", "tools": { "recover_agent_run": "allow" } }
        });
        assert_eq!(
            resolve_permission_behavior(&snapshot, "recover_agent_run"),
            ToolPermissionBehavior::Allow
        );
    }

    #[test]
    fn test_resolve_permission_falls_back_to_default() {
        let snapshot = json!({ "permissions": { "default_behavior": "deny", "tools": {} } });
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
        let action = PermissionAction::SetDefault { behavior: ToolPermissionBehavior::Allow };
        let state_action = permission_state_action(action);
        assert!(!matches!(state_action, AnyStateAction::Patch(_)));
    }

    #[test]
    fn test_permission_plugin_id() {
        assert_eq!(AgentBehavior::id(&PermissionPlugin), PERMISSION_PLUGIN_ID);
    }

    #[tokio::test]
    async fn test_permission_plugin_allow() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({ "permissions": { "default_behavior": "allow", "tools": {} } }));
        let args = json!({});
        let ctx = tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute, "t1", &[], &config, &doc,
        ).with_tool_info("any_tool", "call_1", Some(&args));
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(!has_block(&actions));
        assert!(!has_suspend(&actions));
    }

    #[tokio::test]
    async fn test_permission_plugin_deny() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({ "permissions": { "default_behavior": "deny", "tools": {} } }));
        let args = json!({});
        let ctx = tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute, "t1", &[], &config, &doc,
        ).with_tool_info("any_tool", "call_1", Some(&args));
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_block(&actions));
    }

    #[tokio::test]
    async fn test_permission_plugin_ask() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({ "permissions": { "default_behavior": "ask", "tools": {} } }));
        let args = json!({"path": "a.txt"});
        let ctx = tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute, "t1", &[], &config, &doc,
        ).with_tool_info("test_tool", "call_1", Some(&args));
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_suspend(&actions));
    }

    #[tokio::test]
    async fn test_permission_plugin_ask_with_empty_call_id_blocks() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({ "permissions": { "default_behavior": "ask", "tools": {} } }));
        let args = json!({"path": "a.txt"});
        let ctx = tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute, "t1", &[], &config, &doc,
        ).with_tool_info("test_tool", "", Some(&args));
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_block(&actions));
        assert!(!has_suspend(&actions));
    }

    #[test]
    fn test_resolve_default_permission() {
        let snapshot = json!({ "permissions": { "default_behavior": "allow", "tools": {} } });
        assert_eq!(
            resolve_permission_behavior(&snapshot, "unknown_tool"),
            ToolPermissionBehavior::Allow
        );
    }

    #[test]
    fn test_resolve_default_permission_deny() {
        let snapshot = json!({ "permissions": { "default_behavior": "deny", "tools": {} } });
        assert_eq!(
            resolve_permission_behavior(&snapshot, "unknown_tool"),
            ToolPermissionBehavior::Deny
        );
    }

    #[tokio::test]
    async fn test_permission_plugin_tool_specific_allow() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({
            "permissions": { "default_behavior": "deny", "tools": { "allowed_tool": "allow" } }
        }));
        let args = json!({});
        let ctx = tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute, "t1", &[], &config, &doc,
        ).with_tool_info("allowed_tool", "call_1", Some(&args));
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(!has_block(&actions));
    }

    #[tokio::test]
    async fn test_permission_plugin_tool_specific_deny() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({
            "permissions": { "default_behavior": "allow", "tools": { "denied_tool": "deny" } }
        }));
        let args = json!({});
        let ctx = tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute, "t1", &[], &config, &doc,
        ).with_tool_info("denied_tool", "call_1", Some(&args));
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_block(&actions));
    }

    #[tokio::test]
    async fn test_permission_plugin_tool_specific_ask() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({
            "permissions": { "default_behavior": "allow", "tools": { "ask_tool": "ask" } }
        }));
        let args = json!({});
        let ctx = tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute, "t1", &[], &config, &doc,
        ).with_tool_info("ask_tool", "call_1", Some(&args));
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_suspend(&actions));
    }

    #[tokio::test]
    async fn test_permission_plugin_invalid_tool_behavior() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({
            "permissions": { "default_behavior": "allow", "tools": { "invalid_tool": "invalid_behavior" } }
        }));
        let args = json!({});
        let ctx = tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute, "t1", &[], &config, &doc,
        ).with_tool_info("invalid_tool", "call_1", Some(&args));
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(!has_block(&actions));
        assert!(!has_suspend(&actions));
    }

    #[tokio::test]
    async fn test_permission_plugin_invalid_default_behavior() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({ "permissions": { "default_behavior": "invalid_default", "tools": {} } }));
        let args = json!({});
        let ctx = tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute, "t1", &[], &config, &doc,
        ).with_tool_info("any_tool", "call_1", Some(&args));
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_suspend(&actions));
    }

    #[tokio::test]
    async fn test_permission_plugin_no_state() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({}));
        let args = json!({});
        let ctx = tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute, "t1", &[], &config, &doc,
        ).with_tool_info("any_tool", "call_1", Some(&args));
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_suspend(&actions));
    }

    #[tokio::test]
    async fn test_permission_plugin_tools_is_string_not_object() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({
            "permissions": { "default_behavior": "allow", "tools": "corrupted" }
        }));
        let args = json!({});
        let ctx = tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute, "t1", &[], &config, &doc,
        ).with_tool_info("any_tool", "call_1", Some(&args));
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(!has_block(&actions));
        assert!(!has_suspend(&actions));
    }

    #[tokio::test]
    async fn test_permission_plugin_default_behavior_invalid_string() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({ "permissions": { "default_behavior": "invalid_value", "tools": {} } }));
        let args = json!({});
        let ctx = tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute, "t1", &[], &config, &doc,
        ).with_tool_info("any_tool", "call_1", Some(&args));
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_suspend(&actions));
    }

    #[tokio::test]
    async fn test_permission_plugin_default_behavior_is_number() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({ "permissions": { "default_behavior": 42, "tools": {} } }));
        let args = json!({});
        let ctx = tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute, "t1", &[], &config, &doc,
        ).with_tool_info("any_tool", "call_1", Some(&args));
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_suspend(&actions));
    }

    #[tokio::test]
    async fn test_permission_plugin_tool_value_is_number() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({
            "permissions": { "default_behavior": "allow", "tools": { "my_tool": 123 } }
        }));
        let args = json!({});
        let ctx = tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute, "t1", &[], &config, &doc,
        ).with_tool_info("my_tool", "call_1", Some(&args));
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(!has_block(&actions));
        assert!(!has_suspend(&actions));
    }

    #[tokio::test]
    async fn test_permission_plugin_permissions_is_array() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({ "permissions": [1, 2, 3] }));
        let args = json!({});
        let ctx = tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute, "t1", &[], &config, &doc,
        ).with_tool_info("any_tool", "call_1", Some(&args));
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_suspend(&actions));
    }

    #[test]
    fn test_tool_policy_plugin_id() {
        assert_eq!(AgentBehavior::id(&ToolPolicyPlugin), "tool_policy");
    }

    #[tokio::test]
    async fn test_tool_policy_blocks_out_of_scope() {
        let mut config = RunConfig::new();
        config.set(scope::SCOPE_ALLOWED_TOOLS_KEY, vec!["other_tool"]).unwrap();
        let doc = DocCell::new(json!({}));
        let args = json!({});
        let ctx = tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute, "t1", &[], &config, &doc,
        ).with_tool_info("blocked_tool", "call_1", Some(&args));
        let actions = AgentBehavior::before_tool_execute(&ToolPolicyPlugin, &ctx).await;
        assert!(has_block(&actions));
    }

    #[tokio::test]
    async fn test_tool_policy_allows_in_scope() {
        let mut config = RunConfig::new();
        config.set(scope::SCOPE_ALLOWED_TOOLS_KEY, vec!["my_tool"]).unwrap();
        let doc = DocCell::new(json!({}));
        let args = json!({});
        let ctx = tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute, "t1", &[], &config, &doc,
        ).with_tool_info("my_tool", "call_1", Some(&args));
        let actions = AgentBehavior::before_tool_execute(&ToolPolicyPlugin, &ctx).await;
        assert!(!has_block(&actions));
    }

    #[tokio::test]
    async fn test_tool_policy_no_filters_allows_all() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({}));
        let args = json!({});
        let ctx = tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute, "t1", &[], &config, &doc,
        ).with_tool_info("any_tool", "call_1", Some(&args));
        let actions = AgentBehavior::before_tool_execute(&ToolPolicyPlugin, &ctx).await;
        assert!(!has_block(&actions));
    }

    #[tokio::test]
    async fn test_tool_policy_excluded_tool_is_blocked() {
        let mut config = RunConfig::new();
        config.set(scope::SCOPE_EXCLUDED_TOOLS_KEY, vec!["excluded_tool"]).unwrap();
        let doc = DocCell::new(json!({}));
        let args = json!({});
        let ctx = tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute, "t1", &[], &config, &doc,
        ).with_tool_info("excluded_tool", "call_1", Some(&args));
        let actions = AgentBehavior::before_tool_execute(&ToolPolicyPlugin, &ctx).await;
        assert!(has_block(&actions));
    }

    #[test]
    fn test_permission_policy_denied_tools_wins() {
        let snapshot = json!({
            "permission_policy": { "default_behavior": "allow", "allowed_tools": [], "denied_tools": ["bad_tool"] }
        });
        assert_eq!(
            resolve_permission_behavior(&snapshot, "bad_tool"),
            ToolPermissionBehavior::Deny
        );
    }

    #[test]
    fn test_permission_policy_allowed_tools() {
        let snapshot = json!({
            "permission_policy": { "default_behavior": "deny", "allowed_tools": ["good_tool"], "denied_tools": [] }
        });
        assert_eq!(
            resolve_permission_behavior(&snapshot, "good_tool"),
            ToolPermissionBehavior::Allow
        );
    }

    #[test]
    fn test_permission_policy_falls_back_to_legacy() {
        let snapshot = json!({
            "permission_policy": { "default_behavior": "ask", "allowed_tools": [], "denied_tools": [] },
            "permissions": { "default_behavior": "allow", "tools": { "legacy_tool": "deny" } }
        });
        assert_eq!(
            resolve_permission_behavior(&snapshot, "legacy_tool"),
            ToolPermissionBehavior::Deny
        );
    }

    #[test]
    fn test_permission_policy_denied_overrides_legacy_allow() {
        let snapshot = json!({
            "permission_policy": { "default_behavior": "ask", "allowed_tools": [], "denied_tools": ["tool_x"] },
            "permissions": { "default_behavior": "allow", "tools": { "tool_x": "allow" } }
        });
        assert_eq!(
            resolve_permission_behavior(&snapshot, "tool_x"),
            ToolPermissionBehavior::Deny
        );
    }

    #[test]
    fn test_permission_state_action_routes_allow_to_policy() {
        let action = PermissionAction::SetTool {
            tool_id: "my_tool".to_string(),
            behavior: ToolPermissionBehavior::Allow,
        };
        let state_action = permission_state_action(action);
        assert!(!matches!(state_action, AnyStateAction::Patch(_)));
    }

    #[test]
    fn test_permission_state_action_routes_deny_to_policy() {
        let action = PermissionAction::SetTool {
            tool_id: "my_tool".to_string(),
            behavior: ToolPermissionBehavior::Deny,
        };
        let state_action = permission_state_action(action);
        assert!(!matches!(state_action, AnyStateAction::Patch(_)));
    }

    #[test]
    fn test_permission_plugin_registers_lattice_paths() {
        let mut registry = LatticeRegistry::new();
        PermissionPlugin.register_lattice_paths(&mut registry);
        assert!(
            registry
                .get(&tirea_state::parse_path("permission_policy.allowed_tools"))
                .is_some()
        );
        assert!(
            registry
                .get(&tirea_state::parse_path("permission_policy.denied_tools"))
                .is_some()
        );
    }

    #[tokio::test]
    async fn test_permission_resume_input_bypasses_ask() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({ "permissions": { "default_behavior": "ask", "tools": {} } }));
        let args = json!({});
        let resume = ToolCallResume {
            decision_id: "fc_call_1".to_string(),
            action: ResumeDecisionAction::Resume,
            result: serde_json::Value::Bool(true),
            reason: None,
            updated_at: 1,
        };
        let ctx = tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute, "t1", &[], &config, &doc,
        )
        .with_tool_info("test_tool", "call_1", Some(&args))
        .with_resume_input(resume);
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(!has_block(&actions));
        assert!(!has_suspend(&actions));
    }
}
