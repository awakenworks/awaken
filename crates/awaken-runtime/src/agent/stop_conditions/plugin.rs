use std::sync::Arc;

use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use awaken_contract::StateError;
use awaken_contract::model::Phase;

use super::hook::StopConditionHook;
use super::policy::{MaxRoundsPolicy, StopPolicy};
use super::state::StopConditionStatsKey;

/// Plugin that evaluates stop policies after each inference step.
pub struct StopConditionPlugin {
    policies: Vec<Arc<dyn StopPolicy>>,
}

impl StopConditionPlugin {
    pub fn new(policies: Vec<Arc<dyn StopPolicy>>) -> Self {
        Self { policies }
    }
}

impl Plugin for StopConditionPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "stop-condition",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<StopConditionStatsKey>(crate::state::StateKeyOptions::default())?;
        registrar.register_phase_hook(
            "stop-condition",
            Phase::AfterInference,
            StopConditionHook {
                policies: self.policies.clone(),
            },
        )
    }
}

/// Convenience plugin that terminates the run after a maximum number of steps.
///
/// Wraps `StopConditionPlugin` with a single `MaxRoundsPolicy`.
pub struct MaxRoundsPlugin {
    max_rounds: usize,
}

impl MaxRoundsPlugin {
    pub fn new(max_rounds: usize) -> Self {
        Self { max_rounds }
    }
}

impl Plugin for MaxRoundsPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "stop-condition:max-rounds",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        // Delegate to StopConditionPlugin internals
        let policies: Vec<Arc<dyn StopPolicy>> =
            vec![Arc::new(MaxRoundsPolicy::new(self.max_rounds))];
        registrar.register_key::<StopConditionStatsKey>(crate::state::StateKeyOptions::default())?;
        registrar.register_phase_hook(
            "stop-condition:max-rounds",
            Phase::AfterInference,
            StopConditionHook { policies },
        )
    }
}
