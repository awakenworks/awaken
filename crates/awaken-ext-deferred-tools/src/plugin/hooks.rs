//! Phase hooks for deferred tool management.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::StateError;
use awaken_contract::contract::context_message::ContextMessage;
use awaken_runtime::agent::state::{AddContextMessage, ExcludeTool};
use awaken_runtime::phase::{PhaseContext, PhaseHook};
use awaken_runtime::state::StateCommand;

use awaken_runtime::phase::TypedScheduledActionHandler;

use awaken_contract::contract::profile_store::ProfileOwner;

use crate::config::{DeferredToolsConfigKey, ToolLoadMode};
use crate::policy::{DeferralDecision, DeferralPolicy, DiscBetaEvaluator};
use crate::state::{
    AgentToolPriors, AgentToolPriorsKey, DeferToolAction, DeferralState, DeferralStateAction,
    DeferralStateValue, DiscBetaAction, DiscBetaEntry, DiscBetaState, PromoteToolAction,
    ToolUsageStats, ToolUsageStatsAction,
};
use crate::tool_search::TOOL_SEARCH_ID;

// ---------------------------------------------------------------------------
// Public helper functions (testable without PhaseContext)
// ---------------------------------------------------------------------------

/// Build a sorted, comma-separated list of deferred tool names, prefixed with
/// an explanatory header. Returns an empty string if no tools are deferred.
pub fn build_deferred_tool_list(state: &DeferralStateValue) -> String {
    let mut deferred: Vec<&str> = state
        .modes
        .iter()
        .filter(|(_, mode)| **mode == ToolLoadMode::Deferred)
        .map(|(id, _)| id.as_str())
        .collect();

    if deferred.is_empty() {
        return String::new();
    }

    deferred.sort();
    format!(
        "The following deferred tools are available via ToolSearch:\n{}",
        deferred.join(", ")
    )
}

/// Return the IDs of all deferred tools, excluding ToolSearch itself
/// (ToolSearch must never be excluded from the tool set).
pub fn collect_exclusions(state: &DeferralStateValue) -> Vec<String> {
    state
        .modes
        .iter()
        .filter(|(id, mode)| **mode == ToolLoadMode::Deferred && id.as_str() != TOOL_SEARCH_ID)
        .map(|(id, _)| id.clone())
        .collect()
}

/// Extract promoted tool IDs from a ToolSearch result payload.
///
/// Reads `data["__promote"]` as an array of strings.
/// Returns an empty vec if the field is missing or not an array.
pub fn extract_promote_ids_from_tool_result(data: &serde_json::Value) -> Vec<String> {
    data.get("__promote")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Apply defer and promote actions to a mutable deferral state.
///
/// Defers are applied first, then promotes. This means promote wins on
/// conflict when the same tool appears in both sets.
pub fn apply_deferral_actions(
    state: &mut DeferralStateValue,
    defer_payloads: &[Vec<String>],
    promote_payloads: &[Vec<String>],
) {
    for batch in defer_payloads {
        for id in batch {
            state.modes.insert(id.clone(), ToolLoadMode::Deferred);
        }
    }
    for batch in promote_payloads {
        for id in batch {
            state.modes.insert(id.clone(), ToolLoadMode::Eager);
        }
    }
}

/// Check whether the plugin is effectively enabled.
/// With `Option<bool>` semantics, `None` means auto-enable was used at
/// activation time; if DeferralState is populated, the plugin is active.
fn is_enabled(ctx: &PhaseContext) -> bool {
    let config = ctx.config::<DeferredToolsConfigKey>().unwrap_or_default();
    match config.enabled {
        Some(false) => false,
        Some(true) => true,
        None => {
            // Auto-enable: check if DeferralState was seeded (on_activate decided yes).
            ctx.state::<DeferralState>()
                .map(|s| !s.modes.is_empty())
                .unwrap_or(false)
        }
    }
}

// ---------------------------------------------------------------------------
// BeforeInference hook
// ---------------------------------------------------------------------------

pub struct DeferredToolsBeforeInferenceHook {
    pub policy: Arc<dyn DeferralPolicy>,
}

#[async_trait]
impl PhaseHook for DeferredToolsBeforeInferenceHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new();

        if !is_enabled(ctx) {
            return Ok(cmd);
        }

        let config = ctx.config::<DeferredToolsConfigKey>().unwrap_or_default();
        let deferral_state = ctx.state::<DeferralState>().cloned().unwrap_or_default();
        let usage_stats = ctx.state::<ToolUsageStats>().cloned().unwrap_or_default();

        // Collect all tool IDs currently tracked in deferral state.
        let tool_ids: Vec<String> = deferral_state.modes.keys().cloned().collect();

        // Evaluate policy decisions (initial classification).
        let decisions: Vec<DeferralDecision> =
            self.policy
                .evaluate(&usage_stats, &deferral_state, &config, &tool_ids);

        // Schedule state updates only for tools whose mode actually changed.
        for decision in &decisions {
            let current = deferral_state
                .modes
                .get(&decision.tool_id)
                .copied()
                .unwrap_or(ToolLoadMode::Eager);
            if current != decision.target_mode {
                match decision.target_mode {
                    ToolLoadMode::Deferred => {
                        cmd.update::<DeferralState>(DeferralStateAction::Defer(
                            decision.tool_id.clone(),
                        ));
                    }
                    ToolLoadMode::Eager => {
                        cmd.update::<DeferralState>(DeferralStateAction::Promote(
                            decision.tool_id.clone(),
                        ));
                    }
                }
            }
        }

        // Compute effective deferred set from decisions.
        let mut effective = deferral_state.clone();
        for decision in &decisions {
            effective
                .modes
                .insert(decision.tool_id.clone(), decision.target_mode);
        }

        // DiscBeta dynamic re-defer evaluation.
        let disc_beta_state = ctx.state::<DiscBetaState>().cloned().unwrap_or_default();
        let re_defer_ids = DiscBetaEvaluator::tools_to_defer(
            &disc_beta_state,
            &effective,
            &config,
            usage_stats.total_turns,
        );
        for id in &re_defer_ids {
            effective.modes.insert(id.clone(), ToolLoadMode::Deferred);
            cmd.update::<DeferralState>(DeferralStateAction::Defer(id.clone()));
        }

        // Schedule ExcludeTool for each effectively deferred tool (except ToolSearch).
        let exclusions = collect_exclusions(&effective);
        for tool_id in &exclusions {
            cmd.schedule_action::<ExcludeTool>(tool_id.clone())?;
        }

        // Schedule context message with the sorted deferred tool list.
        let list = build_deferred_tool_list(&effective);
        if !list.is_empty() {
            cmd.schedule_action::<AddContextMessage>(ContextMessage::system_persistent(
                "deferred_tools.list",
                list,
            ))?;
        }

        Ok(cmd)
    }
}

// ---------------------------------------------------------------------------
// AfterToolExecute hook
// ---------------------------------------------------------------------------

pub struct DeferredToolsAfterToolExecuteHook;

#[async_trait]
impl PhaseHook for DeferredToolsAfterToolExecuteHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new();

        if !is_enabled(ctx) {
            return Ok(cmd);
        }

        let tool_name = match &ctx.tool_name {
            Some(name) => name.clone(),
            None => return Ok(cmd),
        };

        // Record the tool call in usage stats.
        cmd.update::<ToolUsageStats>(ToolUsageStatsAction::RecordCall {
            tool_id: tool_name.clone(),
        });

        // If this was a ToolSearch call, extract promote IDs from the result.
        if tool_name == TOOL_SEARCH_ID
            && let Some(ref result) = ctx.tool_result
        {
            let ids = extract_promote_ids_from_tool_result(&result.data);
            if !ids.is_empty() {
                cmd.update::<DeferralState>(DeferralStateAction::PromoteBatch(ids));
            }
        }

        Ok(cmd)
    }
}

// ---------------------------------------------------------------------------
// AfterInference hook
// ---------------------------------------------------------------------------

pub struct DeferredToolsAfterInferenceHook;

#[async_trait]
impl PhaseHook for DeferredToolsAfterInferenceHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new();

        if !is_enabled(ctx) {
            return Ok(cmd);
        }

        let config = ctx.config::<DeferredToolsConfigKey>().unwrap_or_default();

        // Observe turn for DiscBeta: identify tools called in this turn from
        // ToolUsageStats (those whose last_use_turn matches the current turn).
        let stats = ctx.state::<ToolUsageStats>().cloned().unwrap_or_default();
        let current_turn = stats.total_turns;
        let tools_called: Vec<String> = stats
            .tools
            .iter()
            .filter(|(_, e)| e.last_use_turn == Some(current_turn))
            .map(|(id, _)| id.clone())
            .collect();

        cmd.update::<DiscBetaState>(DiscBetaAction::ObserveTurn {
            omega: config.disc_beta.omega,
            current_turn,
            tools_called,
        });

        // Increment turn counter (after DiscBeta observation).
        cmd.update::<ToolUsageStats>(ToolUsageStatsAction::IncrementTurn);

        Ok(cmd)
    }
}

// ---------------------------------------------------------------------------
// Scheduled action handlers
// ---------------------------------------------------------------------------

/// Handler for DeferToolAction — updates DeferralState to defer the specified tools.
pub(crate) struct DeferToolHandler;

#[async_trait]
impl TypedScheduledActionHandler<DeferToolAction> for DeferToolHandler {
    async fn handle_typed(
        &self,
        _ctx: &PhaseContext,
        payload: Vec<String>,
    ) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new();
        if !payload.is_empty() {
            cmd.update::<DeferralState>(DeferralStateAction::SetBatch(
                payload
                    .into_iter()
                    .map(|id| (id, ToolLoadMode::Deferred))
                    .collect(),
            ));
        }
        Ok(cmd)
    }
}

/// Handler for PromoteToolAction — updates DeferralState to promote the specified tools to eager.
pub(crate) struct PromoteToolHandler;

#[async_trait]
impl TypedScheduledActionHandler<PromoteToolAction> for PromoteToolHandler {
    async fn handle_typed(
        &self,
        _ctx: &PhaseContext,
        payload: Vec<String>,
    ) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new();
        if !payload.is_empty() {
            cmd.update::<DeferralState>(DeferralStateAction::PromoteBatch(payload));
        }
        Ok(cmd)
    }
}

// ---------------------------------------------------------------------------
// RunStart hook: load persisted priors from ProfileStore
// ---------------------------------------------------------------------------

pub(crate) struct LoadPriorsHook;

#[async_trait]
impl PhaseHook for LoadPriorsHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new();

        let Some(profile_access) = ctx.profile_access.as_deref() else {
            return Ok(cmd);
        };

        let config = ctx.config::<DeferredToolsConfigKey>().unwrap_or_default();
        let owner = ProfileOwner::Agent(ctx.agent_spec.id.clone());

        let priors = match profile_access.read::<AgentToolPriorsKey>(&owner).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "failed to read agent tool priors");
                return Ok(cmd);
            }
        };

        if priors.session_count == 0 {
            tracing::debug!("no persisted agent priors found");
            return Ok(cmd);
        }

        let disc_beta = ctx.state::<DiscBetaState>().cloned().unwrap_or_default();
        let n0 = config.disc_beta.n0;

        let updates: Vec<(String, DiscBetaEntry)> = priors
            .tools
            .iter()
            .filter_map(|(tool_id, &p)| {
                let existing = disc_beta.tools.get(tool_id)?;
                Some((
                    tool_id.clone(),
                    DiscBetaEntry::new(p, n0, existing.c, existing.c_bar),
                ))
            })
            .collect();

        if !updates.is_empty() {
            cmd.update::<DiscBetaState>(DiscBetaAction::InitBatch(updates));
        }

        tracing::debug!(
            session_count = priors.session_count,
            tools = priors.tools.len(),
            "loaded persisted agent priors"
        );

        Ok(cmd)
    }
}

// ---------------------------------------------------------------------------
// RunEnd hook: persist session stats to ProfileStore
// ---------------------------------------------------------------------------

pub(crate) struct PersistPriorsHook;

#[async_trait]
impl PhaseHook for PersistPriorsHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let Some(profile_access) = ctx.profile_access.as_deref() else {
            return Ok(StateCommand::new());
        };

        let stats = ctx.state::<ToolUsageStats>().cloned().unwrap_or_default();
        if stats.total_turns == 0 {
            return Ok(StateCommand::new());
        }

        let owner = ProfileOwner::Agent(ctx.agent_spec.id.clone());

        let mut priors = match profile_access.read::<AgentToolPriorsKey>(&owner).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "failed to read agent tool priors before update");
                AgentToolPriors::default()
            }
        };

        // EWMA: λ = max(0.1, 1/(n+1)) — equal weight early, stabilises at 10%
        let lambda = 0.1_f64.max(1.0 / (priors.session_count as f64 + 1.0));

        for (tool_id, entry) in &stats.tools {
            let session_p = entry.presence_freq(stats.total_turns);
            let prior_p = priors.tools.get(tool_id).copied().unwrap_or(session_p);
            let updated_p = (1.0 - lambda) * prior_p + lambda * session_p;
            priors.tools.insert(tool_id.clone(), updated_p);
        }
        priors.session_count += 1;

        if let Err(e) = profile_access
            .write::<AgentToolPriorsKey>(&owner, &priors)
            .await
        {
            tracing::warn!(error = %e, "failed to persist agent tool priors");
        } else {
            tracing::debug!(
                session_count = priors.session_count,
                tools = priors.tools.len(),
                "persisted agent tool priors"
            );
        }

        Ok(StateCommand::new())
    }
}
