use awaken_contract::PluginConfigKey;
use awaken_contract::contract::tool::ToolDescriptor;
use awaken_contract::model::Phase;
use awaken_contract::registry_spec::AgentSpec;
use awaken_runtime::PhaseHook;
use awaken_runtime::phase::PhaseContext;
use awaken_runtime::plugins::Plugin;
use awaken_runtime::state::{MutationBatch, StateStore};
use serde_json::json;

use crate::config::{DeferralRule, DeferredToolsConfig, DeferredToolsConfigKey, ToolLoadMode};
use crate::plugin::DeferredToolsPlugin;
use crate::plugin::hooks::DeferredToolsBeforeInferenceHook;
use crate::state::{DeferralRegistry, DeferralState};

fn tool(id: &str) -> ToolDescriptor {
    ToolDescriptor::new(id, id, format!("{id} tool")).with_parameters(json!({
        "type": "object",
        "properties": {
            "input": {
                "type": "string",
                "description": "large enough to create a stable descriptor body"
            }
        }
    }))
}

#[test]
fn plugin_exposes_deferred_tools_config_schema() {
    let plugin = DeferredToolsPlugin::new(vec![]);
    let schemas = plugin.config_schemas();

    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].key, DeferredToolsConfigKey::KEY);
    assert_eq!(schemas[0].json_schema["type"], "object");
}

#[test]
fn plugin_agent_config_seeds_deferral_state_and_registry() {
    let seed_tools = vec![tool("Bash"), tool("mcp__search")];
    let store = StateStore::new();
    store
        .install_plugin(DeferredToolsPlugin::new(seed_tools.clone()))
        .unwrap();

    let spec = AgentSpec::new("deferred-agent")
        .with_config::<DeferredToolsConfigKey>(DeferredToolsConfig {
            enabled: Some(true),
            default_mode: ToolLoadMode::Deferred,
            rules: vec![DeferralRule {
                tool: "Bash".into(),
                mode: ToolLoadMode::Eager,
            }],
            ..Default::default()
        })
        .unwrap();

    let plugin = DeferredToolsPlugin::new(seed_tools);
    let mut patch = MutationBatch::new();
    plugin.on_activate(&spec, &mut patch).unwrap();
    store.commit(patch).unwrap();

    let state = store.read::<DeferralState>().unwrap();
    assert_eq!(state.modes["Bash"], ToolLoadMode::Eager);
    assert_eq!(state.modes["mcp__search"], ToolLoadMode::Deferred);

    let registry = store.read::<DeferralRegistry>().unwrap();
    assert!(!registry.tools.contains_key("Bash"));
    assert!(registry.tools.contains_key("mcp__search"));
}

#[test]
fn plugin_on_activate_rejects_invalid_agent_config() {
    let spec = AgentSpec::new("bad-deferred").with_section(
        DeferredToolsConfigKey::KEY,
        json!({"default_mode": "not_a_mode"}),
    );

    let plugin = DeferredToolsPlugin::new(vec![tool("Bash")]);
    let mut patch = MutationBatch::new();
    let err = plugin.on_activate(&spec, &mut patch).unwrap_err();

    assert!(err.to_string().contains(DeferredToolsConfigKey::KEY));
}

#[tokio::test]
async fn hook_rejects_invalid_agent_config() {
    let spec = AgentSpec::new("bad-deferred")
        .with_section(DeferredToolsConfigKey::KEY, json!({"enabled": "sometimes"}));
    let store = StateStore::new();
    let ctx =
        PhaseContext::new(Phase::BeforeInference, store.snapshot()).with_agent_spec(spec.into());

    let err = DeferredToolsBeforeInferenceHook
        .run(&ctx)
        .await
        .unwrap_err();

    assert!(err.to_string().contains(DeferredToolsConfigKey::KEY));
}
