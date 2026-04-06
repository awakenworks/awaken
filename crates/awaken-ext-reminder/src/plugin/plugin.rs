use std::sync::Arc;

use awaken_contract::model::Phase;
use awaken_contract::{PluginConfigKey, StateError};
use awaken_runtime::plugins::{ConfigSchema, Plugin, PluginDescriptor, PluginRegistrar};

use crate::config::{ReminderConfigKey, ReminderRulesConfig};
use crate::rule::ReminderRule;

use super::hook::ReminderHook;

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
        vec![ConfigSchema {
            key: ReminderConfigKey::KEY,
            json_schema: serde_json::to_value(schemars::schema_for!(ReminderRulesConfig))
                .unwrap_or_default(),
        }]
    }
}
