use async_trait::async_trait;
use genai::chat::ChatOptions;
use std::sync::Arc;

use tirea_contract::runtime::behavior::ReadOnlyContext;
use tirea_contract::runtime::inference::{ContextWindowPolicy, InferenceRequestTransform};
use tirea_contract::runtime::phase::{ActionSet, AfterToolExecuteAction, BeforeInferenceAction};
use tirea_contract::runtime::state::{AnyStateAction, StateScope};
use tirea_contract::runtime::tool_call::{
    suspended_calls_from_state, tool_call_states_from_state, ToolResult,
};
use tirea_contract::thread::Message;

use crate::engine::token_estimator::{estimate_messages_tokens, estimate_tokens};
use crate::runtime::loop_runner::LlmExecutor;

use super::compaction::{
    build_artifact_preview, find_compaction_plan, now_ms, render_messages_for_summary,
    ContextSummarizer, LlmContextSummarizer,
    DEFAULT_ARTIFACT_COMPACT_THRESHOLD_TOKENS, MIN_COMPACTION_GAIN_TOKENS, SummaryPayload,
};
use super::state::{ArtifactRef, CompactBoundary, ContextAction, ContextState};
use super::transform::ContextTransform;
use super::{policy_for_model, CONTEXT_PLUGIN_ID};

/// Unified context plugin: logical compression + hard truncation + prompt caching.
#[derive(Clone)]
pub struct ContextPlugin {
    pub(super) policy: ContextWindowPolicy,
    artifact_compact_threshold_tokens: usize,
    summarizer: Option<Arc<dyn ContextSummarizer>>,
}

impl std::fmt::Debug for ContextPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextPlugin")
            .field("policy", &self.policy)
            .field(
                "artifact_compact_threshold_tokens",
                &self.artifact_compact_threshold_tokens,
            )
            .field("has_summarizer", &self.summarizer.is_some())
            .finish()
    }
}

impl Default for ContextPlugin {
    fn default() -> Self {
        Self::new(ContextWindowPolicy::default())
    }
}

impl ContextPlugin {
    pub fn new(policy: ContextWindowPolicy) -> Self {
        Self {
            policy,
            artifact_compact_threshold_tokens: DEFAULT_ARTIFACT_COMPACT_THRESHOLD_TOKENS,
            summarizer: None,
        }
    }

    /// Create with model-specific defaults.
    pub fn for_model(model: &str) -> Self {
        Self::new(policy_for_model(model))
    }

    pub fn with_artifact_compact_threshold_tokens(mut self, threshold: usize) -> Self {
        self.artifact_compact_threshold_tokens = threshold;
        self
    }

    #[cfg(test)]
    pub(super) fn with_summarizer(mut self, summarizer: Arc<dyn ContextSummarizer>) -> Self {
        self.summarizer = Some(summarizer);
        self
    }

    pub(crate) fn with_llm_summarizer(
        mut self,
        model: String,
        executor: Arc<dyn LlmExecutor>,
        chat_options: Option<ChatOptions>,
    ) -> Self {
        self.summarizer = Some(Arc::new(LlmContextSummarizer::new(
            model,
            executor,
            chat_options,
        )));
        self
    }

    async fn maybe_compact(
        &self,
        ctx: &ReadOnlyContext<'_>,
        state: &ContextState,
    ) -> Option<(CompactBoundary, Option<ContextAction>)> {
        let threshold = self.policy.autocompact_threshold?;
        let raw_messages: Vec<Message> = ctx
            .messages()
            .iter()
            .map(|message| (**message).clone())
            .collect();
        let effective_messages = ContextTransform::new(
            state.clone(),
            ContextWindowPolicy {
                max_context_tokens: usize::MAX,
                ..self.policy.clone()
            },
        )
        .transform(raw_messages, &[])
        .messages;
        let effective_tokens = estimate_messages_tokens(&effective_messages);
        if effective_tokens < threshold {
            return None;
        }

        let snapshot = ctx.snapshot();
        let tool_states = tool_call_states_from_state(&snapshot);
        let suspended_calls = suspended_calls_from_state(&snapshot);
        let plan = find_compaction_plan(
            ctx.messages(),
            state,
            &tool_states,
            &suspended_calls,
            self.policy.compaction_mode,
            self.policy.compaction_raw_suffix_messages,
        )?;
        if plan.covered_token_count < MIN_COMPACTION_GAIN_TOKENS {
            return None;
        }
        let summarizer = self.summarizer.as_ref()?;

        let mut delta_messages: Vec<Message> = ctx.messages()
            [plan.start_index..=plan.boundary_index]
            .iter()
            .map(|message| (**message).clone())
            .collect();
        ContextTransform::new(state.clone(), ContextWindowPolicy::default())
            .apply_artifact_refs(&mut delta_messages);

        let payload = SummaryPayload {
            previous_summary: state
                .latest_boundary()
                .map(|boundary| boundary.summary.clone()),
            transcript: render_messages_for_summary(&delta_messages),
        };
        let summary = match summarizer.summarize(payload).await {
            Ok(summary) => summary,
            Err(error) => {
                tracing::warn!(
                    thread_id = %ctx.thread_id(),
                    error = %error,
                    "context compaction skipped after summary generation failed"
                );
                return None;
            }
        };

        let boundary = CompactBoundary {
            covers_through_message_id: plan.boundary_message_id.clone(),
            summary,
            original_token_count: state
                .latest_boundary()
                .map(|boundary| boundary.original_token_count)
                .unwrap_or(0)
                + plan.covered_token_count,
            created_at_ms: now_ms(),
        };

        let prune_action =
            if plan.covered_message_ids.is_empty() && plan.covered_tool_call_ids.is_empty() {
                None
            } else {
                Some(ContextAction::PruneArtifacts {
                    message_ids: plan.covered_message_ids,
                    tool_call_ids: plan.covered_tool_call_ids,
                })
            };

        Some((boundary, prune_action))
    }

    fn maybe_build_artifact_ref(&self, call_id: &str, result: &ToolResult) -> Option<ArtifactRef> {
        let raw_content = serde_json::to_string(result).unwrap_or_else(|_| {
            result
                .message
                .clone()
                .unwrap_or_else(|| result.data.to_string())
        });
        let token_count = estimate_tokens(&raw_content);
        if token_count < self.artifact_compact_threshold_tokens {
            return None;
        }

        Some(ArtifactRef {
            message_id: None,
            tool_call_id: Some(call_id.to_string()),
            label: result.tool_name.clone(),
            summary: build_artifact_preview(result),
            original_size: raw_content.len(),
            original_token_count: token_count,
        })
    }
}

#[async_trait]
impl tirea_contract::runtime::AgentBehavior for ContextPlugin {
    fn id(&self) -> &str {
        CONTEXT_PLUGIN_ID
    }

    tirea_contract::declare_plugin_states!(ContextState);

    async fn before_inference(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        let state = ctx
            .scoped_state_of::<ContextState>(StateScope::Thread)
            .ok()
            .unwrap_or_default();

        let mut effective_state = state.clone();
        let mut actions = ActionSet::empty();

        if let Some((boundary, prune_action)) = self.maybe_compact(ctx, &state).await {
            let boundary_action = ContextAction::AddBoundary(boundary.clone());
            effective_state.reduce(boundary_action.clone());
            actions = actions.and(BeforeInferenceAction::State(AnyStateAction::new::<
                ContextState,
            >(boundary_action)));
            if let Some(prune_action) = prune_action {
                effective_state.reduce(prune_action.clone());
                actions = actions.and(BeforeInferenceAction::State(AnyStateAction::new::<
                    ContextState,
                >(prune_action)));
            }
        }

        // Always register the combined transform: compaction is a no-op when
        // state is empty, but truncation must always run.
        actions.and(BeforeInferenceAction::AddRequestTransform(Arc::new(
            ContextTransform::new(effective_state, self.policy.clone()),
        )))
    }

    async fn after_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<AfterToolExecuteAction> {
        let Some(result) = ctx.tool_result() else {
            return ActionSet::empty();
        };
        let Some(call_id) = ctx.tool_call_id() else {
            return ActionSet::empty();
        };

        let Some(artifact) = self.maybe_build_artifact_ref(call_id, result) else {
            return ActionSet::empty();
        };

        ActionSet::single(AfterToolExecuteAction::State(AnyStateAction::new::<
            ContextState,
        >(
            ContextAction::AddArtifact(artifact),
        )))
    }
}
