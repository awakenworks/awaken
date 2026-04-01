//! Configuration types for deferred tool loading.

use std::collections::HashMap;

use awaken_contract::PluginConfigKey;
use awaken_tool_pattern::wildcard_match;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Loading mode for a tool.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolLoadMode {
    Eager,
    #[default]
    Deferred,
}

/// A single rule mapping a tool name pattern to a load mode.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DeferralRule {
    pub tool: String,
    pub mode: ToolLoadMode,
}

/// Parameters for the discounted Beta probability model.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct DiscBetaParams {
    /// Discount factor per turn. Effective memory ≈ 1/(1-ω) turns.
    pub omega: f64,
    /// Prior strength in equivalent observations.
    pub n0: f64,
    /// Consecutive idle turns before re-defer is considered.
    pub defer_after: u64,
    /// Defer threshold multiplier against breakeven frequency.
    pub thresh_mult: f64,
    /// Estimated ToolSearch call cost in tokens (for breakeven calculation).
    pub gamma: f64,
}

impl Default for DiscBetaParams {
    fn default() -> Self {
        Self {
            omega: 0.95,
            n0: 5.0,
            defer_after: 5,
            thresh_mult: 0.5,
            gamma: 2000.0,
        }
    }
}

/// Top-level configuration for the deferred tools plugin.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct DeferredToolsConfig {
    /// `None` = auto-enable when total schema savings > beta overhead.
    /// `Some(true)` = always enable. `Some(false)` = always disable.
    pub enabled: Option<bool>,
    /// Ordered rules — first match wins.
    pub rules: Vec<DeferralRule>,
    /// Mode for tools not matching any rule.
    pub default_mode: ToolLoadMode,
    /// Per-turn overhead of ToolSearch schema + deferred list prompt (tokens).
    pub beta_overhead: f64,
    /// Agent-level prior per-turn presence frequencies per tool (from historical data).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub agent_priors: HashMap<String, f64>,
    /// Discounted Beta model parameters.
    pub disc_beta: DiscBetaParams,
}

impl Default for DeferredToolsConfig {
    fn default() -> Self {
        Self {
            enabled: None,
            rules: Vec::new(),
            default_mode: ToolLoadMode::default(),
            beta_overhead: 1136.0,
            agent_priors: HashMap::new(),
            disc_beta: DiscBetaParams::default(),
        }
    }
}

impl DeferredToolsConfig {
    /// Resolve the load mode for a tool by matching rules in order.
    pub fn resolve_mode(&self, tool_id: &str) -> ToolLoadMode {
        for rule in &self.rules {
            if rule.tool == tool_id || wildcard_match(&rule.tool, tool_id) {
                return rule.mode;
            }
        }
        self.default_mode
    }

    /// Check if deferred tools should be enabled.
    /// `total_savings` is sum of (c_i - c_bar_i) for all deferrable tools.
    pub fn should_enable(&self, total_savings: f64) -> bool {
        match self.enabled {
            Some(forced) => forced,
            None => total_savings > self.beta_overhead,
        }
    }

    /// Get the prior p_i for a tool from agent-level stats.
    pub fn prior_p(&self, tool_id: &str) -> f64 {
        self.agent_priors.get(tool_id).copied().unwrap_or(0.01)
    }
}

/// [`PluginConfigKey`] binding for deferred tools config in agent specs.
pub struct DeferredToolsConfigKey;

impl PluginConfigKey for DeferredToolsConfigKey {
    const KEY: &'static str = "deferred_tools";
    type Config = DeferredToolsConfig;
}
