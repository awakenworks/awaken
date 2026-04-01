//! DeferredToolsPlugin — wires config, state, hooks, and ToolSearch together.

use std::sync::Arc;

use awaken_contract::StateError;
use awaken_contract::contract::tool::ToolDescriptor;
use awaken_contract::model::Phase;
use awaken_contract::registry_spec::AgentSpec;
use awaken_contract::state::StateKeyOptions;
use awaken_runtime::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use awaken_runtime::state::MutationBatch;

use crate::config::{DeferredToolsConfigKey, ToolLoadMode};
use crate::policy::{ConfigOnlyPolicy, DeferralPolicy};
use crate::state::{
    AgentToolPriorsKey, DeferToolAction, DeferralRegistry, DeferralRegistryAction, DeferralState,
    DeferralStateAction, DiscBetaAction, DiscBetaEntry, DiscBetaState, PromoteToolAction,
    ToolUsageStats,
};
use crate::tool_search::{TOOL_SEARCH_ID, ToolSearchTool};

use super::hooks::{
    DeferToolHandler, DeferredToolsAfterInferenceHook, DeferredToolsAfterToolExecuteHook,
    DeferredToolsBeforeInferenceHook, LoadPriorsHook, PersistPriorsHook, PromoteToolHandler,
};

/// Stable plugin ID for the deferred tools extension.
pub const DEFERRED_TOOLS_PLUGIN_ID: &str = "ext-deferred-tools";

/// Plugin that manages deferred tool loading.
///
/// Registers state keys, the ToolSearch tool, and phase hooks that evaluate
/// deferral policy at each inference step.
pub struct DeferredToolsPlugin {
    policy: Arc<dyn DeferralPolicy>,
    seed_tools: Vec<ToolDescriptor>,
}

impl DeferredToolsPlugin {
    /// Create a new plugin with `ConfigOnlyPolicy` and the given seed tools.
    #[must_use]
    pub fn new(seed_tools: Vec<ToolDescriptor>) -> Self {
        Self {
            policy: Arc::new(ConfigOnlyPolicy),
            seed_tools,
        }
    }

    /// Create a new plugin with a custom deferral policy.
    #[must_use]
    pub fn with_policy(policy: Arc<dyn DeferralPolicy>, seed_tools: Vec<ToolDescriptor>) -> Self {
        Self { policy, seed_tools }
    }
}

impl Plugin for DeferredToolsPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: DEFERRED_TOOLS_PLUGIN_ID,
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        // State keys
        registrar.register_key::<DeferralRegistry>(StateKeyOptions::default())?;
        registrar.register_key::<DeferralState>(StateKeyOptions::default())?;
        registrar.register_key::<ToolUsageStats>(StateKeyOptions::default())?;
        registrar.register_key::<DiscBetaState>(StateKeyOptions::default())?;

        // Profile key for cross-session priors
        registrar.register_profile_key::<AgentToolPriorsKey>()?;

        // ToolSearch tool
        registrar.register_tool(TOOL_SEARCH_ID, Arc::new(ToolSearchTool))?;

        // Scheduled action handlers
        registrar.register_scheduled_action::<DeferToolAction, _>(DeferToolHandler)?;
        registrar.register_scheduled_action::<PromoteToolAction, _>(PromoteToolHandler)?;

        // Phase hooks
        registrar.register_phase_hook(DEFERRED_TOOLS_PLUGIN_ID, Phase::RunStart, LoadPriorsHook)?;
        registrar.register_phase_hook(
            DEFERRED_TOOLS_PLUGIN_ID,
            Phase::BeforeInference,
            DeferredToolsBeforeInferenceHook {
                policy: Arc::clone(&self.policy),
            },
        )?;
        registrar.register_phase_hook(
            DEFERRED_TOOLS_PLUGIN_ID,
            Phase::AfterToolExecute,
            DeferredToolsAfterToolExecuteHook,
        )?;
        registrar.register_phase_hook(
            DEFERRED_TOOLS_PLUGIN_ID,
            Phase::AfterInference,
            DeferredToolsAfterInferenceHook,
        )?;
        registrar.register_phase_hook(
            DEFERRED_TOOLS_PLUGIN_ID,
            Phase::RunEnd,
            PersistPriorsHook,
        )?;

        Ok(())
    }

    fn on_activate(
        &self,
        agent_spec: &AgentSpec,
        patch: &mut MutationBatch,
    ) -> Result<(), StateError> {
        let config = agent_spec
            .config::<DeferredToolsConfigKey>()
            .unwrap_or_default();

        let mut state_entries = Vec::new();
        let mut registry_entries = Vec::new();
        let mut disc_beta_batch = Vec::new();
        let mut total_savings = 0.0_f64;

        for tool in &self.seed_tools {
            // Never defer ToolSearch itself.
            if tool.id == TOOL_SEARCH_ID {
                continue;
            }

            let mode = config.resolve_mode(&tool.id);

            // Estimate token costs from schema size (rough heuristic: 1 token ≈ 4 chars).
            let c = (tool.parameters.to_string().len() as f64 / 4.0).max(10.0);
            let c_bar = (tool.name.len() as f64 / 4.0).max(1.0);

            if mode == ToolLoadMode::Deferred {
                total_savings += (c - c_bar).max(0.0);
            }

            state_entries.push((tool.id.clone(), mode));

            if mode == ToolLoadMode::Deferred {
                registry_entries.push(tool.clone().into());
            }

            let prior_p = config.prior_p(&tool.id);
            disc_beta_batch.push((
                tool.id.clone(),
                DiscBetaEntry::new(prior_p, config.disc_beta.n0, c, c_bar),
            ));
        }

        // Auto-enable check: if config.enabled is None, use savings heuristic.
        if !config.should_enable(total_savings) {
            return Ok(());
        }

        if !state_entries.is_empty() {
            patch.update::<DeferralState>(DeferralStateAction::SetBatch(state_entries));
        }
        if !registry_entries.is_empty() {
            patch.update::<DeferralRegistry>(DeferralRegistryAction::RegisterBatch(
                registry_entries,
            ));
        }
        if !disc_beta_batch.is_empty() {
            patch.update::<DiscBetaState>(DiscBetaAction::InitBatch(disc_beta_batch));
        }

        Ok(())
    }
}
