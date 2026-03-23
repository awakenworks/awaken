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

/// Durable state for context compaction tracking.
///
/// Stores a history of compaction boundaries so that load-time trimming
/// and plugin queries can identify already-summarized ranges.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactionState {
    /// Ordered list of compaction boundaries (most recent last).
    pub boundaries: Vec<CompactionBoundary>,
    /// Total number of compaction passes performed.
    pub total_compactions: u64,
}

/// Reducer actions for [`CompactionState`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompactionAction {
    /// Record a new compaction boundary.
    RecordBoundary(CompactionBoundary),
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
            CompactionAction::Clear => {
                self.boundaries.clear();
                self.total_compactions = 0;
            }
        }
    }

    /// Latest compaction boundary, if any.
    pub fn latest_boundary(&self) -> Option<&CompactionBoundary> {
        self.boundaries.last()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateStore;

    #[test]
    fn compaction_state_record_boundary() {
        let mut state = CompactionState::default();
        assert_eq!(state.total_compactions, 0);
        assert!(state.boundaries.is_empty());

        state.reduce(CompactionAction::RecordBoundary(CompactionBoundary {
            summary: "User asked to implement feature X.".into(),
            pre_tokens: 5000,
            post_tokens: 200,
            timestamp_ms: 1234567890,
        }));

        assert_eq!(state.total_compactions, 1);
        assert_eq!(state.boundaries.len(), 1);
        assert_eq!(
            state.latest_boundary().unwrap().summary,
            "User asked to implement feature X."
        );
    }

    #[test]
    fn compaction_state_multiple_boundaries() {
        let mut state = CompactionState::default();

        for i in 0..3 {
            state.reduce(CompactionAction::RecordBoundary(CompactionBoundary {
                summary: format!("summary {i}"),
                pre_tokens: 1000 * (i + 1),
                post_tokens: 100 * (i + 1),
                timestamp_ms: 1000 + i as u64,
            }));
        }

        assert_eq!(state.total_compactions, 3);
        assert_eq!(state.boundaries.len(), 3);
        assert_eq!(state.latest_boundary().unwrap().summary, "summary 2");
    }

    #[test]
    fn compaction_state_clear() {
        let mut state = CompactionState {
            boundaries: vec![CompactionBoundary {
                summary: "old".into(),
                pre_tokens: 100,
                post_tokens: 10,
                timestamp_ms: 1,
            }],
            total_compactions: 1,
        };

        state.reduce(CompactionAction::Clear);
        assert!(state.boundaries.is_empty());
        assert_eq!(state.total_compactions, 0);
    }

    #[test]
    fn compaction_state_latest_boundary_empty() {
        let state = CompactionState::default();
        assert!(state.latest_boundary().is_none());
    }

    #[test]
    fn compaction_state_serde_roundtrip() {
        let state = CompactionState {
            boundaries: vec![
                CompactionBoundary {
                    summary: "first".into(),
                    pre_tokens: 5000,
                    post_tokens: 200,
                    timestamp_ms: 1000,
                },
                CompactionBoundary {
                    summary: "second".into(),
                    pre_tokens: 3000,
                    post_tokens: 150,
                    timestamp_ms: 2000,
                },
            ],
            total_compactions: 2,
        };

        let json = serde_json::to_string(&state).unwrap();
        let parsed: CompactionState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, state);
    }

    #[test]
    fn compaction_plugin_registers_key() {
        let store = StateStore::new();
        store.install_plugin(CompactionPlugin::default()).unwrap();
        let registry = store.registry.lock();
        assert!(registry.keys_by_name.contains_key("__context_compaction"));
    }

    #[test]
    fn compaction_plugin_state_via_store() {
        let store = StateStore::new();
        store.install_plugin(CompactionPlugin::default()).unwrap();

        let mut patch = store.begin_mutation();
        patch.update::<CompactionStateKey>(super::super::record_compaction_boundary(
            CompactionBoundary {
                summary: "test summary".into(),
                pre_tokens: 4000,
                post_tokens: 180,
                timestamp_ms: 9999,
            },
        ));
        store.commit(patch).unwrap();

        let state = store.read::<CompactionStateKey>().unwrap();
        assert_eq!(state.total_compactions, 1);
        assert_eq!(state.boundaries[0].summary, "test summary");
    }

    #[test]
    fn record_compaction_boundary_constructor() {
        let action = super::super::record_compaction_boundary(CompactionBoundary {
            summary: "s".into(),
            pre_tokens: 100,
            post_tokens: 10,
            timestamp_ms: 0,
        });
        assert!(matches!(action, CompactionAction::RecordBoundary(_)));
    }

    #[test]
    fn compaction_state_record_then_clear_then_record() {
        let mut state = CompactionState::default();

        state.reduce(CompactionAction::RecordBoundary(CompactionBoundary {
            summary: "first".into(),
            pre_tokens: 1000,
            post_tokens: 100,
            timestamp_ms: 1,
        }));
        assert_eq!(state.total_compactions, 1);

        state.reduce(CompactionAction::Clear);
        assert_eq!(state.total_compactions, 0);
        assert!(state.boundaries.is_empty());
        assert!(state.latest_boundary().is_none());

        state.reduce(CompactionAction::RecordBoundary(CompactionBoundary {
            summary: "after clear".into(),
            pre_tokens: 2000,
            post_tokens: 150,
            timestamp_ms: 2,
        }));
        assert_eq!(state.total_compactions, 1);
        assert_eq!(state.latest_boundary().unwrap().summary, "after clear");
    }

    #[test]
    fn compaction_state_key_properties() {
        assert_eq!(CompactionStateKey::KEY, "__context_compaction");
    }

    #[test]
    fn compaction_state_key_apply() {
        let mut state = CompactionState::default();
        CompactionStateKey::apply(
            &mut state,
            CompactionAction::RecordBoundary(CompactionBoundary {
                summary: "via apply".into(),
                pre_tokens: 500,
                post_tokens: 50,
                timestamp_ms: 42,
            }),
        );
        assert_eq!(state.total_compactions, 1);
        assert_eq!(state.boundaries[0].summary, "via apply");
    }

    #[test]
    fn compaction_plugin_descriptor_name() {
        let plugin = CompactionPlugin::default();
        assert_eq!(plugin.descriptor().name, CONTEXT_COMPACTION_PLUGIN_ID);
    }

    #[test]
    fn compaction_plugin_new_with_config() {
        let config = CompactionConfig {
            min_savings_ratio: 0.8,
            ..Default::default()
        };
        let plugin = CompactionPlugin::new(config);
        assert!((plugin.config.min_savings_ratio - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn compaction_boundary_equality() {
        let a = CompactionBoundary {
            summary: "s".into(),
            pre_tokens: 100,
            post_tokens: 10,
            timestamp_ms: 0,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn compaction_boundary_serde_roundtrip() {
        let boundary = CompactionBoundary {
            summary: "test summary".into(),
            pre_tokens: 3000,
            post_tokens: 200,
            timestamp_ms: 1234567890,
        };
        let json = serde_json::to_string(&boundary).unwrap();
        let parsed: CompactionBoundary = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, boundary);
    }
}
