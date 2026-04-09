use awaken_contract::model::Phase;
use awaken_contract::registry_spec::AgentSpec;
use awaken_contract::{PluginConfigKey, StateError};
use awaken_runtime::plugins::{ConfigSchema, Plugin, PluginDescriptor, PluginRegistrar};
use awaken_runtime::state::{KeyScope, MutationBatch, StateKeyOptions};

use crate::config::{PermissionConfigKey, PermissionRulesConfig};
use crate::state::{PermissionAction, PermissionOverridesKey, PermissionPolicyKey};

use super::checker::PermissionToolGateHook;
use super::filter::PermissionToolFilterHook;

/// Stable plugin name for the permission extension.
pub const PERMISSION_PLUGIN_NAME: &str = "permission";

/// Permission extension plugin.
///
/// Registers:
/// - [`PermissionPolicyKey`]: thread-scoped persisted permission rules
/// - [`PermissionOverridesKey`]: run-scoped temporary overrides
/// - A `BeforeInference` phase hook that removes unconditionally denied tools
///   from the tool list before the LLM sees them
/// - A `ToolGate` hook that evaluates rules and blocks or suspends tool calls
pub struct PermissionPlugin;

impl Plugin for PermissionPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: PERMISSION_PLUGIN_NAME,
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<PermissionPolicyKey>(StateKeyOptions {
            persistent: true,
            retain_on_uninstall: false,
            scope: KeyScope::Thread,
        })?;

        registrar.register_key::<PermissionOverridesKey>(StateKeyOptions {
            persistent: false,
            retain_on_uninstall: false,
            scope: KeyScope::Run,
        })?;

        registrar.register_phase_hook(
            PERMISSION_PLUGIN_NAME,
            Phase::BeforeInference,
            PermissionToolFilterHook,
        )?;

        registrar.register_tool_gate_hook(PERMISSION_PLUGIN_NAME, PermissionToolGateHook)?;

        Ok(())
    }

    fn config_schemas(&self) -> Vec<ConfigSchema> {
        vec![ConfigSchema {
            key: PermissionConfigKey::KEY,
            json_schema: serde_json::to_value(schemars::schema_for!(PermissionRulesConfig))
                .unwrap_or_default(),
        }]
    }

    fn on_activate(
        &self,
        agent_spec: &AgentSpec,
        patch: &mut MutationBatch,
    ) -> Result<(), StateError> {
        let config: PermissionRulesConfig = agent_spec.config::<PermissionConfigKey>()?;

        // Seed default behavior from config
        if config.default_behavior != Default::default() {
            patch.update::<PermissionPolicyKey>(PermissionAction::SetDefault {
                behavior: config.default_behavior,
            });
        }

        // Seed rules from config entries
        for entry in &config.rules {
            patch.update::<PermissionPolicyKey>(PermissionAction::SetRule {
                pattern: entry.tool.clone(),
                behavior: entry.behavior,
            });
        }

        Ok(())
    }
}
