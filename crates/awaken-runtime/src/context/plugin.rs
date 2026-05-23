//! CompactionPlugin, CompactionConfig, and compaction state tracking.

use serde::{Deserialize, Serialize};

use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use crate::state::{MutationBatch, StateKey, StateKeyOptions};

/// Plugin ID for context compaction.
pub const CONTEXT_COMPACTION_PLUGIN_ID: &str = "context_compaction";

// ---------------------------------------------------------------------------
// CompactionConfig — configurable prompts and thresholds
// ---------------------------------------------------------------------------

/// Configuration for the compaction subsystem.
///
/// Controls summarizer prompts, model selection, and savings thresholds.
/// Stored in `AgentSpec.sections["compaction"]` and read via `PluginConfigKey`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CompactionConfig {
    /// System prompt for the summarizer LLM call.
    pub summarizer_system_prompt: String,
    /// User prompt template. `{messages}` is replaced with the conversation transcript.
    pub summarizer_user_prompt: String,
    /// Maximum tokens for the summary response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_max_tokens: Option<u32>,
    /// Model to use for summarization (if different from the agent's model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_model: Option<String>,
    /// Minimum token savings ratio to accept a compaction (0.0-1.0).
    pub min_savings_ratio: f64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            summarizer_system_prompt: "You are a conversation summarizer. Preserve all key facts, decisions, tool results, and action items. Be concise but complete.".into(),
            summarizer_user_prompt: "Summarize the following conversation:\n\n{messages}".into(),
            summary_max_tokens: None,
            summary_model: None,
            min_savings_ratio: 0.3,
        }
    }
}

/// Plugin config key for [`CompactionConfig`].
pub struct CompactionConfigKey;

impl awaken_contract::registry_spec::PluginConfigKey for CompactionConfigKey {
    const KEY: &'static str = "compaction";
    type Config = CompactionConfig;
}

// ---------------------------------------------------------------------------
// Compaction boundary tracking
// ---------------------------------------------------------------------------

/// A recorded compaction boundary — snapshot of a single compaction event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactionBoundary {
    /// Summary text produced by the compaction pass.
    pub summary: String,
    /// Estimated tokens before compaction (in the compacted range).
    pub pre_tokens: usize,
    /// Estimated tokens after compaction (summary message tokens).
    pub post_tokens: usize,
    /// Timestamp of the compaction event (millis since UNIX epoch).
    pub timestamp_ms: u64,
}

/// A failed background compaction attempt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactionFailure {
    /// Background task id when the failure was tied to an in-flight task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Stable message id of the boundary message at trigger time.
    pub boundary_message_id: String,
    /// Internal failure text recorded by the summarizer/task runner.
    pub error: String,
    /// Timestamp of the failure event (millis since UNIX epoch).
    pub timestamp_ms: u64,
}

/// Pointer to a single in-flight background compaction pass. Used as a
/// single-flight guard so the runtime never spawns a second compaction
/// while one is still summarizing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactionInFlight {
    /// Background task id of the running compaction.
    pub task_id: String,
    /// Stable message id of the boundary message at trigger time. Used
    /// to locate the cut point against the current message list when the
    /// summary lands — robust to messages appended during the window.
    pub boundary_message_id: String,
    /// Wall-clock millis when the task was spawned.
    pub started_at_ms: u64,
}

/// Durable state for context compaction tracking.
///
/// Stores a history of compaction boundaries so that load-time trimming
/// and plugin queries can identify already-summarized ranges, plus a
/// single-flight guard for background compaction passes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactionState {
    /// Ordered list of compaction boundaries (most recent last).
    pub boundaries: Vec<CompactionBoundary>,
    /// Ordered list of failed compaction attempts (most recent last).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<CompactionFailure>,
    /// Total number of compaction passes performed.
    pub total_compactions: u64,
    /// Currently running background compaction, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_flight: Option<CompactionInFlight>,
}

/// Reducer actions for [`CompactionState`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompactionAction {
    /// Record a new compaction boundary.
    RecordBoundary(CompactionBoundary),
    /// Record a failed compaction attempt.
    RecordFailure(CompactionFailure),
    /// Mark a background compaction as in flight.
    SetInFlight(CompactionInFlight),
    /// Clear the in-flight marker (called on success and failure).
    ClearInFlight,
    /// Clear all tracked boundaries (e.g. on thread reset).
    Clear,
}

impl CompactionState {
    fn reduce(&mut self, action: CompactionAction) {
        match action {
            CompactionAction::RecordBoundary(boundary) => {
                self.boundaries.push(boundary);
                self.total_compactions += 1;
            }
            CompactionAction::RecordFailure(failure) => {
                self.failures.push(failure);
            }
            CompactionAction::SetInFlight(in_flight) => {
                self.in_flight = Some(in_flight);
            }
            CompactionAction::ClearInFlight => {
                self.in_flight = None;
            }
            CompactionAction::Clear => {
                self.boundaries.clear();
                self.failures.clear();
                self.total_compactions = 0;
                self.in_flight = None;
            }
        }
    }

    /// Latest compaction boundary, if any.
    pub fn latest_boundary(&self) -> Option<&CompactionBoundary> {
        self.boundaries.last()
    }

    /// True when a background compaction pass is already running.
    pub fn is_compacting(&self) -> bool {
        self.in_flight.is_some()
    }
}

/// State key for context compaction state.
pub struct CompactionStateKey;

impl StateKey for CompactionStateKey {
    const KEY: &'static str = "__context_compaction";
    type Value = CompactionState;
    type Update = CompactionAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        value.reduce(update);
    }
}

// ---------------------------------------------------------------------------
// CompactionPlugin
// ---------------------------------------------------------------------------

/// Plugin that integrates context compaction state into the plugin system.
///
/// Registers the [`CompactionStateKey`] state key so that compaction boundaries
/// are tracked durably and available to other plugins and external observers.
/// Accepts an optional [`CompactionConfig`] for configurable prompts and thresholds.
#[derive(Debug, Clone, Default)]
pub struct CompactionPlugin {
    /// Compaction configuration (prompts, model, thresholds).
    pub config: CompactionConfig,
}

impl CompactionPlugin {
    /// Create with explicit config.
    pub fn new(config: CompactionConfig) -> Self {
        Self { config }
    }
}

impl Plugin for CompactionPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: CONTEXT_COMPACTION_PLUGIN_ID,
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), awaken_contract::StateError> {
        registrar.register_key::<CompactionStateKey>(StateKeyOptions::default())?;
        Ok(())
    }

    fn on_activate(
        &self,
        _agent_spec: &awaken_contract::registry_spec::AgentSpec,
        _patch: &mut MutationBatch,
    ) -> Result<(), awaken_contract::StateError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ContextTransformPlugin — registers the context truncation request transform
// ---------------------------------------------------------------------------

/// Plugin ID for context truncation transform.
pub const CONTEXT_TRANSFORM_PLUGIN_ID: &str = "context_transform";

/// Plugin that registers the built-in context truncation request transform.
///
/// Wraps a `ContextWindowPolicy` and registers a `ContextTransform` via
/// `register_request_transform()` during plugin registration. This ensures
/// the transform flows through the standard plugin mechanism (ADR-0001)
/// instead of being manually appended post-hoc.
pub struct ContextTransformPlugin {
    policy: awaken_contract::contract::inference::ContextWindowPolicy,
}

impl ContextTransformPlugin {
    pub fn new(policy: awaken_contract::contract::inference::ContextWindowPolicy) -> Self {
        Self { policy }
    }
}

impl Plugin for ContextTransformPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: CONTEXT_TRANSFORM_PLUGIN_ID,
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), awaken_contract::StateError> {
        registrar.register_request_transform(
            CONTEXT_TRANSFORM_PLUGIN_ID,
            super::ContextTransform::new(self.policy.clone()),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;
