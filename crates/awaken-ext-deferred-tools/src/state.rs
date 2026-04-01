//! State keys for deferred tool management.

use std::collections::HashMap;

use awaken_contract::contract::profile_store::ProfileKey;
use awaken_contract::model::{Phase, ScheduledActionSpec};
use awaken_runtime::state::{MergeStrategy, StateKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::ToolLoadMode;

pub struct DeferralRegistry;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToolDescriptor {
    pub id: String,
    pub name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

impl From<awaken_contract::contract::tool::ToolDescriptor> for StoredToolDescriptor {
    fn from(desc: awaken_contract::contract::tool::ToolDescriptor) -> Self {
        Self {
            id: desc.id,
            name: desc.name,
            description: desc.description,
            parameters: desc.parameters,
            category: desc.category,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeferralRegistryValue {
    pub tools: HashMap<String, StoredToolDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeferralRegistryAction {
    Register(StoredToolDescriptor),
    RegisterBatch(Vec<StoredToolDescriptor>),
    Remove(String),
}

impl StateKey for DeferralRegistry {
    const KEY: &'static str = "deferred_tools.registry";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;
    type Value = DeferralRegistryValue;
    type Update = DeferralRegistryAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            DeferralRegistryAction::Register(desc) => {
                value.tools.insert(desc.id.clone(), desc);
            }
            DeferralRegistryAction::RegisterBatch(descs) => {
                for desc in descs {
                    value.tools.insert(desc.id.clone(), desc);
                }
            }
            DeferralRegistryAction::Remove(id) => {
                value.tools.remove(&id);
            }
        }
    }
}

pub struct DeferralState;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeferralStateValue {
    pub modes: HashMap<String, ToolLoadMode>,
}

impl DeferralStateValue {
    pub fn deferred_tool_ids(&self) -> Vec<&str> {
        self.modes
            .iter()
            .filter(|(_, mode)| **mode == ToolLoadMode::Deferred)
            .map(|(id, _)| id.as_str())
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeferralStateAction {
    Defer(String),
    Promote(String),
    PromoteBatch(Vec<String>),
    SetBatch(Vec<(String, ToolLoadMode)>),
}

impl StateKey for DeferralState {
    const KEY: &'static str = "deferred_tools.state";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;
    type Value = DeferralStateValue;
    type Update = DeferralStateAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            DeferralStateAction::Defer(id) => {
                value.modes.insert(id, ToolLoadMode::Deferred);
            }
            DeferralStateAction::Promote(id) => {
                value.modes.insert(id, ToolLoadMode::Eager);
            }
            DeferralStateAction::PromoteBatch(ids) => {
                for id in ids {
                    value.modes.insert(id, ToolLoadMode::Eager);
                }
            }
            DeferralStateAction::SetBatch(entries) => {
                for (id, mode) in entries {
                    value.modes.insert(id, mode);
                }
            }
        }
    }
}

/// Action to move tools from eager to deferred.
pub struct DeferToolAction;

impl ScheduledActionSpec for DeferToolAction {
    const KEY: &'static str = "deferred_tools.defer";
    const PHASE: Phase = Phase::BeforeInference;
    type Payload = Vec<String>;
}

/// Action to move tools from deferred to eager.
pub struct PromoteToolAction;

impl ScheduledActionSpec for PromoteToolAction {
    const KEY: &'static str = "deferred_tools.promote";
    const PHASE: Phase = Phase::BeforeInference;
    type Payload = Vec<String>;
}

pub struct ToolUsageStats;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolUsageStatsValue {
    pub total_turns: u64,
    pub tools: HashMap<String, ToolUsageEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolUsageEntry {
    pub turn_presence_count: u64,
    pub total_call_count: u64,
    pub first_use_turn: Option<u64>,
    pub last_use_turn: Option<u64>,
}

impl ToolUsageEntry {
    pub fn presence_freq(&self, total_turns: u64) -> f64 {
        if total_turns == 0 {
            return 0.0;
        }
        self.turn_presence_count as f64 / total_turns as f64
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolUsageStatsAction {
    IncrementTurn,
    RecordCall { tool_id: String },
    RecordTurnCalls { calls: Vec<(String, u64)> },
}

impl StateKey for ToolUsageStats {
    const KEY: &'static str = "deferred_tools.usage_stats";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;
    type Value = ToolUsageStatsValue;
    type Update = ToolUsageStatsAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            ToolUsageStatsAction::IncrementTurn => {
                value.total_turns += 1;
            }
            ToolUsageStatsAction::RecordCall { tool_id } => {
                let turn = value.total_turns;
                let entry = value.tools.entry(tool_id).or_default();
                entry.total_call_count += 1;
                if entry.last_use_turn != Some(turn) {
                    entry.turn_presence_count += 1;
                }
                if entry.first_use_turn.is_none() {
                    entry.first_use_turn = Some(turn);
                }
                entry.last_use_turn = Some(turn);
            }
            ToolUsageStatsAction::RecordTurnCalls { calls } => {
                let turn = value.total_turns;
                for (tool_id, count) in calls {
                    let entry = value.tools.entry(tool_id).or_default();
                    entry.total_call_count += count;
                    if entry.last_use_turn != Some(turn) {
                        entry.turn_presence_count += 1;
                    }
                    if entry.first_use_turn.is_none() {
                        entry.first_use_turn = Some(turn);
                    }
                    entry.last_use_turn = Some(turn);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// DiscBetaState — per-tool discounted Beta distribution
// ---------------------------------------------------------------------------

/// Per-tool discounted Beta parameters for online probability estimation.
pub struct DiscBetaState;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscBetaStateValue {
    pub tools: HashMap<String, DiscBetaEntry>,
}

/// Per-tool Beta distribution state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscBetaEntry {
    pub alpha: f64,
    pub beta_param: f64,
    /// Turn when this tool was last called.
    pub last_used_turn: Option<u64>,
    /// Full schema token cost (for breakeven calculation).
    pub c: f64,
    /// Name-only token cost.
    pub c_bar: f64,
}

impl DiscBetaEntry {
    pub fn new(p_prior: f64, n0: f64, c: f64, c_bar: f64) -> Self {
        Self {
            alpha: (p_prior * n0).max(0.01),
            beta_param: ((1.0 - p_prior) * n0).max(0.01),
            last_used_turn: None,
            c,
            c_bar,
        }
    }

    /// Posterior mean estimate of p.
    pub fn mean(&self) -> f64 {
        let total = self.alpha + self.beta_param;
        if total < 1e-10 {
            0.0
        } else {
            self.alpha / total
        }
    }

    /// Upper bound of credible interval (normal approximation).
    pub fn upper_ci(&self, confidence: f64) -> f64 {
        let m = self.mean();
        let total = self.alpha + self.beta_param;
        if total < 1e-10 {
            return 1.0;
        }
        let var = (self.alpha * self.beta_param) / (total * total * (total + 1.0));
        let z = if confidence >= 0.95 {
            1.645
        } else if confidence >= 0.90 {
            1.282
        } else {
            1.0
        };
        (m + z * var.sqrt()).min(1.0)
    }

    /// Effective sample size.
    pub fn effective_n(&self) -> f64 {
        self.alpha + self.beta_param
    }

    /// Breakeven per-turn frequency: deferral is profitable below this.
    pub fn breakeven_p(&self, gamma: f64) -> f64 {
        let saving = self.c - self.c_bar;
        if saving <= 0.0 || gamma <= 0.0 {
            return f64::INFINITY;
        }
        saving / gamma
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DiscBetaAction {
    /// Initialize entries from priors.
    InitBatch(Vec<(String, DiscBetaEntry)>),
    /// Observe one turn: discount all, then update called tools.
    ObserveTurn {
        omega: f64,
        current_turn: u64,
        tools_called: Vec<String>,
    },
}

impl StateKey for DiscBetaState {
    const KEY: &'static str = "deferred_tools.disc_beta";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;
    type Value = DiscBetaStateValue;
    type Update = DiscBetaAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            DiscBetaAction::InitBatch(entries) => {
                for (id, entry) in entries {
                    value.tools.entry(id).or_insert(entry);
                }
            }
            DiscBetaAction::ObserveTurn {
                omega,
                current_turn,
                tools_called,
            } => {
                let called_set: std::collections::HashSet<&str> =
                    tools_called.iter().map(|s| s.as_str()).collect();
                for (tid, entry) in value.tools.iter_mut() {
                    entry.alpha *= omega;
                    entry.beta_param *= omega;
                    if called_set.contains(tid.as_str()) {
                        entry.alpha += 1.0;
                        entry.last_used_turn = Some(current_turn);
                    } else {
                        entry.beta_param += 1.0;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// AgentToolPriors — cross-session persistence via ProfileStore
// ---------------------------------------------------------------------------

/// ProfileKey for persisted agent-level tool usage priors.
pub struct AgentToolPriorsKey;

/// Per-tool historical presence frequency, updated via EWMA across sessions.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentToolPriors {
    /// Per-tool EWMA presence frequency (0..1).
    pub tools: HashMap<String, f64>,
    /// Number of sessions this data is based on.
    pub session_count: u64,
}

impl ProfileKey for AgentToolPriorsKey {
    const KEY: &'static str = "deferred_tools.agent_priors";
    type Value = AgentToolPriors;
}
