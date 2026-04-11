use std::sync::Arc;

use awaken_contract::StateError;
use awaken_contract::model::Phase;
use awaken_contract::registry_spec::AgentSpec;
use awaken_runtime::plugins::{ConfigSchema, Plugin, PluginDescriptor, PluginRegistrar};
use awaken_runtime::state::MutationBatch;

use crate::config::ReminderConfigKey;
use crate::rule::ReminderRule;

use super::hook::{ReminderHook, rules_from_config};

/// Stable plugin name for the reminder extension.
pub const REMINDER_PLUGIN_NAME: &str = "reminder";

/// Reminder extension plugin.
///
/// Registers an `AfterToolExecute` phase hook that evaluates reminder rules
/// against the completed tool call. When a rule matches both input pattern
/// and output conditions, it schedules an `AddContextMessage` action.
pub struct ReminderPlugin {
    pub(crate) rules: Arc<[ReminderRule]>,
}

impl ReminderPlugin {
    /// Create a new reminder plugin with the given rules.
    #[must_use]
    pub fn new(rules: Vec<ReminderRule>) -> Self {
        Self {
            rules: rules.into(),
        }
    }
}

impl Plugin for ReminderPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: REMINDER_PLUGIN_NAME,
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_phase_hook(
            REMINDER_PLUGIN_NAME,
            Phase::AfterToolExecute,
            ReminderHook {
                rules: Arc::clone(&self.rules),
            },
        )?;
        Ok(())
    }

    fn config_schemas(&self) -> Vec<ConfigSchema> {
        vec![ConfigSchema::for_key::<ReminderConfigKey>()]
    }

    fn on_activate(
        &self,
        agent_spec: &AgentSpec,
        _patch: &mut MutationBatch,
    ) -> Result<(), StateError> {
        // Fail fast on rule DSL or target errors that JSON Schema cannot express.
        let config = agent_spec.config::<ReminderConfigKey>()?;
        rules_from_config(config).map(|_| ())
    }
}
