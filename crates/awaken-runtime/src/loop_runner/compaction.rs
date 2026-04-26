//! Spawn-side wiring for non-blocking context compaction.
//!
//! `maybe_spawn_compaction` is called from the inference phase. When the
//! agent is configured with both a context summarizer and a background
//! task manager, and an in-flight compaction is not already running, it
//! plans the compaction and offloads the LLM summarization to a
//! background task. The completion event flows back via the inbox where
//! [`crate::context::try_consume_compaction_event`] performs the swap.

use std::sync::Arc;

use awaken_contract::contract::inference::ContextWindowPolicy;
use serde_json::json;

use super::step::StepContext;
use crate::context::{
    COMPACTION_COMPLETED_EVENT, COMPACTION_FAILED_EVENT, CompactionInFlight, CompactionStateKey,
    plan_compaction, record_compaction_in_flight,
};
use crate::extensions::background::{TaskParentContext, TaskResult};
use crate::state::MutationBatch;

/// Background task type used for the spawned compaction.
pub const COMPACTION_TASK_TYPE: &str = "context_compaction";

/// Background task description used for telemetry.
const COMPACTION_TASK_DESCRIPTION: &str = "background context compaction";

/// Spawn a background compaction pass when the conditions are met.
///
/// Returns `true` when a new background task was queued, `false` when the
/// call was a no-op (no manager, no summarizer, no thread id, already
/// compacting, or no useful boundary). Never blocks on the LLM call —
/// the summarization runs in `tokio::spawn` and signals back through the
/// owner inbox.
pub(super) async fn maybe_spawn_compaction(
    ctx: &mut StepContext<'_>,
    policy: &ContextWindowPolicy,
) -> bool {
    let Some(manager) = ctx.agent.background_manager.clone() else {
        return false;
    };
    let Some(summarizer) = ctx.agent.context_summarizer.clone() else {
        return false;
    };
    let Some(thread_id) = ctx.run_identity.thread_id_opt() else {
        return false;
    };
    let owner_thread_id = thread_id.to_string();

    let store = ctx.runtime.store();
    if store
        .read::<CompactionStateKey>()
        .is_some_and(|s| s.is_compacting())
    {
        return false;
    }

    let Some(plan) = plan_compaction(ctx.messages, policy) else {
        return false;
    };

    let executor = Arc::clone(&ctx.agent.llm_executor);
    let plan_for_task = plan.clone();
    let boundary_id_for_state = plan.boundary_message_id.clone();
    let pre_tokens = plan.pre_tokens;

    let task_id = match manager
        .spawn(
            &owner_thread_id,
            COMPACTION_TASK_TYPE,
            None,
            COMPACTION_TASK_DESCRIPTION,
            TaskParentContext::default(),
            move |task_ctx| async move {
                let res = summarizer
                    .summarize(
                        &plan_for_task.transcript,
                        plan_for_task.previous_summary.as_deref(),
                        executor.as_ref(),
                    )
                    .await;
                match res {
                    Ok(summary) => {
                        task_ctx.emit(
                            COMPACTION_COMPLETED_EVENT,
                            json!({
                                "boundary_message_id": plan_for_task.boundary_message_id,
                                "summary": summary,
                                "pre_tokens": pre_tokens,
                            }),
                        );
                        TaskResult::Success(serde_json::Value::Null)
                    }
                    Err(error) => {
                        let error_text = error.to_string();
                        task_ctx.emit(
                            COMPACTION_FAILED_EVENT,
                            json!({
                                "boundary_message_id": plan_for_task.boundary_message_id,
                                "error": error_text,
                            }),
                        );
                        TaskResult::Failed(error.to_string())
                    }
                }
            },
        )
        .await
    {
        Ok(id) => id,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "failed to spawn background compaction task; skipping this round"
            );
            return false;
        }
    };

    let mut batch = MutationBatch::new();
    batch.update::<CompactionStateKey>(record_compaction_in_flight(CompactionInFlight {
        task_id: task_id.clone(),
        boundary_message_id: boundary_id_for_state,
        started_at_ms: now_ms(),
    }));
    if let Err(error) = store.commit(batch) {
        tracing::warn!(
            error = %error,
            task_id = %task_id,
            "failed to record CompactionInFlight; another spawn may race"
        );
    }
    true
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use async_trait::async_trait;
    use awaken_contract::contract::executor::{
        InferenceExecutionError, InferenceRequest, LlmExecutor,
    };
    use awaken_contract::contract::identity::RunIdentity;
    use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
    use awaken_contract::contract::message::{Message, gen_message_id};
    use tokio::sync::Notify;

    use awaken_contract::contract::event_sink::{EventSink, NullEventSink};
    use awaken_contract::contract::identity::RunOrigin;

    use crate::cancellation::CancellationToken;
    use crate::context::{
        CompactionPlugin, CompactionStateKey, ContextSummarizer, SummarizationError,
        TruncationState,
    };
    use crate::extensions::background::{BackgroundTaskManager, BackgroundTaskPlugin};
    use crate::phase::{ExecutionEnv, PhaseRuntime};
    use crate::plugins::Plugin;
    use crate::registry::ResolvedAgent;
    use crate::state::StateStore;

    struct GatedSummarizer {
        gate: Arc<Notify>,
        summary: String,
        observed: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl ContextSummarizer for GatedSummarizer {
        async fn summarize(
            &self,
            _transcript: &str,
            _previous_summary: Option<&str>,
            _executor: &dyn LlmExecutor,
        ) -> Result<String, SummarizationError> {
            self.observed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.gate.notified().await;
            Ok(self.summary.clone())
        }
    }

    struct NoopExecutor;

    #[async_trait]
    impl LlmExecutor for NoopExecutor {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            Ok(StreamResult {
                content: vec![],
                tool_calls: vec![],
                usage: Some(TokenUsage::default()),
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }
        fn name(&self) -> &str {
            "noop"
        }
    }

    fn make_long_messages() -> Vec<Arc<Message>> {
        let mut messages: Vec<Arc<Message>> = (0..6)
            .map(|i| {
                if i % 2 == 0 {
                    Arc::new(Message::user("filler ".repeat(600)))
                } else {
                    Arc::new(Message::assistant("ack"))
                }
            })
            .collect();
        messages.push(Arc::new(Message::user("recent")));
        messages
    }

    fn make_resolved_agent(
        manager: Arc<BackgroundTaskManager>,
        summarizer: Arc<dyn ContextSummarizer>,
    ) -> ResolvedAgent {
        ResolvedAgent::new(
            "test-agent",
            "test-model",
            "system prompt",
            Arc::new(NoopExecutor),
        )
        .with_context_summarizer(summarizer)
        .with_background_manager(manager)
    }

    fn make_phase_runtime(
        manager: &Arc<BackgroundTaskManager>,
    ) -> (PhaseRuntime, StateStore, ExecutionEnv) {
        let store = StateStore::new();
        let runtime = PhaseRuntime::new(store.clone()).expect("runtime");
        manager.set_store(store.clone());
        let plugin: Arc<dyn Plugin> = Arc::new(BackgroundTaskPlugin::new(manager.clone()));
        let env = ExecutionEnv::from_plugins(&[plugin], &Default::default()).unwrap();
        store.register_keys(&env.key_registrations).unwrap();
        store.install_plugin(CompactionPlugin::default()).unwrap();
        (runtime, store, env)
    }

    fn run_identity(thread_id: &str) -> RunIdentity {
        RunIdentity::new(
            thread_id.to_string(),
            None,
            gen_message_id(),
            None,
            "agent".to_string(),
            RunOrigin::User,
        )
    }

    #[tokio::test]
    async fn maybe_spawn_compaction_emits_event_after_summary_completes() {
        use awaken_contract::contract::inference::ContextWindowPolicy;

        let manager = Arc::new(BackgroundTaskManager::new());
        let (runtime, store, env) = make_phase_runtime(&manager);

        let (inbox_tx, mut inbox_rx) = crate::inbox::inbox_channel();
        manager.set_owner_inbox(inbox_tx);

        let gate = Arc::new(Notify::new());
        let observed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let summarizer = Arc::new(GatedSummarizer {
            gate: gate.clone(),
            summary: "synthetic summary text".into(),
            observed: observed.clone(),
        });

        let mut agent = make_resolved_agent(manager.clone(), summarizer);
        agent.env = env;
        let mut messages = make_long_messages();
        let identity = run_identity("thread-bg-compact");
        let cancel = CancellationToken::new();
        let policy = ContextWindowPolicy {
            compaction_raw_suffix_messages: 1,
            ..Default::default()
        };
        let mut total_in = 0u64;
        let mut total_out = 0u64;
        let mut truncation = TruncationState::default();
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

        let mut ctx = StepContext {
            agent: &mut agent,
            messages: &mut messages,
            runtime: &runtime,
            sink,
            checkpoint_store: None,
            run_identity: &identity,
            input_message_count: 0,
            cancellation_token: Some(&cancel),
            run_overrides: &None,
            total_input_tokens: &mut total_in,
            total_output_tokens: &mut total_out,
            truncation_state: &mut truncation,
            run_created_at: 0,
            thread_ctx: None,
        };

        // Summarizer is gated → spawn returns immediately, in_flight set,
        // background task is parked at the gate.
        let spawned = maybe_spawn_compaction(&mut ctx, &policy).await;
        assert!(spawned, "compaction should have been spawned");
        let mid_state = store.read::<CompactionStateKey>().unwrap();
        assert!(mid_state.is_compacting(), "in-flight must be set");

        // A second call must be a no-op while the first is still running.
        let again = maybe_spawn_compaction(&mut ctx, &policy).await;
        assert!(!again, "single-flight guard must reject second spawn");

        // Release the gate; wait for the inbox event to arrive.
        gate.notify_one();
        let payload = tokio::time::timeout(Duration::from_secs(2), inbox_rx.recv_or_cancel(None))
            .await
            .expect("event arrives in time")
            .expect("event present");
        assert_eq!(payload["kind"], "custom");
        assert_eq!(payload["event_type"], "context.compacted");
        assert_eq!(payload["payload"]["summary"], "synthetic summary text");
        assert_eq!(
            observed.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "summarizer entered exactly once"
        );
    }
}
