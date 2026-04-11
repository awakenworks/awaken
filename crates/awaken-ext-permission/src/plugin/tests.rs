use super::*;
use crate::config::{PermissionConfigKey, PermissionRuleEntry, PermissionRulesConfig};
use crate::plugin::checker::PermissionToolGateHook;
use crate::rules::{PermissionRuleScope, ToolPermissionBehavior};
use awaken_contract::PluginConfigKey;
use awaken_contract::contract::tool_intercept::ToolInterceptPayload;
use awaken_contract::model::Phase;
use awaken_contract::registry_spec::AgentSpec;
use awaken_runtime::ToolGateHook;
use awaken_runtime::phase::PhaseContext;
use awaken_runtime::plugins::Plugin;
use awaken_runtime::state::{MutationBatch, StateStore};
use serde_json::json;

#[test]
fn plugin_descriptor() {
    let plugin = PermissionPlugin;
    assert_eq!(plugin.descriptor().name, PERMISSION_PLUGIN_NAME);
}

#[test]
fn plugin_exposes_permission_config_schema() {
    let plugin = PermissionPlugin;
    let schemas = plugin.config_schemas();

    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].key, PermissionConfigKey::KEY);
    assert_eq!(schemas[0].json_schema["type"], "object");
}

#[tokio::test]
async fn plugin_agent_config_seeds_policy_used_by_tool_gate() {
    let store = StateStore::new();
    store.install_plugin(PermissionPlugin).unwrap();

    let spec = AgentSpec::new("permission-agent")
        .with_config::<PermissionConfigKey>(PermissionRulesConfig {
            default_behavior: ToolPermissionBehavior::Deny,
            rules: vec![PermissionRuleEntry {
                tool: "safe_tool".into(),
                behavior: ToolPermissionBehavior::Allow,
                scope: PermissionRuleScope::Project,
            }],
        })
        .unwrap();

    let plugin = PermissionPlugin;
    let mut patch = MutationBatch::new();
    plugin.on_activate(&spec, &mut patch).unwrap();
    store.commit(patch).unwrap();

    let denied_ctx = PhaseContext::new(Phase::ToolGate, store.snapshot()).with_tool_info(
        "unknown_tool",
        "call-denied",
        Some(json!({})),
    );
    assert!(matches!(
        PermissionToolGateHook.run(&denied_ctx).await.unwrap(),
        Some(ToolInterceptPayload::Block { .. })
    ));

    let allowed_ctx = PhaseContext::new(Phase::ToolGate, store.snapshot()).with_tool_info(
        "safe_tool",
        "call-allowed",
        Some(json!({})),
    );
    assert!(
        PermissionToolGateHook
            .run(&allowed_ctx)
            .await
            .unwrap()
            .is_none()
    );
}
