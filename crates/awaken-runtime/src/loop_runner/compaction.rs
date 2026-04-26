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

    /// Summarizer that always fails. Used to drive the failure round-trip.
    struct FailingSummarizer {
        gate: Arc<Notify>,
        message: String,
    }

    #[async_trait]
    impl ContextSummarizer for FailingSummarizer {
        async fn summarize(
            &self,
            _transcript: &str,
            _previous_summary: Option<&str>,
            _executor: &dyn LlmExecutor,
        ) -> Result<String, SummarizationError> {
            self.gate.notified().await;
            Err(SummarizationError::Inference(self.message.clone()))
        }
    }

    /// Summarizer that records the transcript / previous_summary it received
    /// so tests can assert what the spawn helper actually plumbed through.
    struct CapturingSummarizer {
        gate: Arc<Notify>,
        captured_transcript: Arc<std::sync::Mutex<Option<String>>>,
        captured_previous: Arc<std::sync::Mutex<Option<Option<String>>>>,
    }

    #[async_trait]
    impl ContextSummarizer for CapturingSummarizer {
        async fn summarize(
            &self,
            transcript: &str,
            previous_summary: Option<&str>,
            _executor: &dyn LlmExecutor,
        ) -> Result<String, SummarizationError> {
            *self.captured_transcript.lock().unwrap() = Some(transcript.to_string());
            *self.captured_previous.lock().unwrap() = Some(previous_summary.map(|s| s.to_string()));
            self.gate.notified().await;
            Ok("captured".into())
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

    fn default_policy() -> awaken_contract::contract::inference::ContextWindowPolicy {
        awaken_contract::contract::inference::ContextWindowPolicy {
            compaction_raw_suffix_messages: 1,
            ..Default::default()
        }
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

    /// Without a manager, spawn must be a no-op even if a summarizer is set.
    /// This is the documented gating contract.
    #[tokio::test]
    async fn maybe_spawn_compaction_no_op_without_background_manager() {
        let manager = Arc::new(BackgroundTaskManager::new());
        let (runtime, store, env) = make_phase_runtime(&manager);
        let (inbox_tx, _inbox_rx) = crate::inbox::inbox_channel();
        manager.set_owner_inbox(inbox_tx);

        // Build an agent with a summarizer but DROP the background manager.
        let summarizer: Arc<dyn ContextSummarizer> = Arc::new(GatedSummarizer {
            gate: Arc::new(Notify::new()),
            summary: "unused".into(),
            observed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        let mut agent = ResolvedAgent::new(
            "test-agent",
            "test-model",
            "system prompt",
            Arc::new(NoopExecutor),
        )
        .with_context_summarizer(summarizer);
        agent.env = env;
        let mut messages = make_long_messages();
        let identity = run_identity("thread-no-mgr");
        let cancel = CancellationToken::new();
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

        assert!(!maybe_spawn_compaction(&mut ctx, &default_policy()).await);
        assert!(
            !store
                .read::<CompactionStateKey>()
                .is_some_and(|s| s.is_compacting()),
            "no in-flight should be recorded"
        );
    }

    /// Without a summarizer, spawn must be a no-op — the manager alone is not
    /// enough to enable compaction.
    #[tokio::test]
    async fn maybe_spawn_compaction_no_op_without_summarizer() {
        let manager = Arc::new(BackgroundTaskManager::new());
        let (runtime, store, env) = make_phase_runtime(&manager);
        let (inbox_tx, _inbox_rx) = crate::inbox::inbox_channel();
        manager.set_owner_inbox(inbox_tx);

        let mut agent = ResolvedAgent::new(
            "test-agent",
            "test-model",
            "system prompt",
            Arc::new(NoopExecutor),
        )
        .with_background_manager(manager.clone());
        agent.env = env;
        let mut messages = make_long_messages();
        let identity = run_identity("thread-no-sum");
        let cancel = CancellationToken::new();
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

        assert!(!maybe_spawn_compaction(&mut ctx, &default_policy()).await);
        assert!(
            !store
                .read::<CompactionStateKey>()
                .is_some_and(|s| s.is_compacting()),
            "no in-flight should be recorded"
        );
    }

    /// When the message list is short, plan_compaction returns None and spawn
    /// must NOT touch the in-flight marker. Avoids triggering background work
    /// for a useless summary that would not save tokens.
    #[tokio::test]
    async fn maybe_spawn_compaction_no_op_when_no_useful_boundary() {
        let manager = Arc::new(BackgroundTaskManager::new());
        let (runtime, store, env) = make_phase_runtime(&manager);
        let (inbox_tx, _inbox_rx) = crate::inbox::inbox_channel();
        manager.set_owner_inbox(inbox_tx);

        let summarizer: Arc<dyn ContextSummarizer> = Arc::new(GatedSummarizer {
            gate: Arc::new(Notify::new()),
            summary: "unused".into(),
            observed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        let mut agent = make_resolved_agent(manager.clone(), summarizer);
        agent.env = env;
        // Three short messages: nowhere near MIN_COMPACTION_GAIN_TOKENS.
        let mut messages: Vec<Arc<Message>> = vec![
            Arc::new(Message::user("hello")),
            Arc::new(Message::assistant("hi")),
            Arc::new(Message::user("again")),
        ];
        let identity = run_identity("thread-tiny");
        let cancel = CancellationToken::new();
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

        assert!(!maybe_spawn_compaction(&mut ctx, &default_policy()).await);
        assert!(
            !store
                .read::<CompactionStateKey>()
                .is_some_and(|s| s.is_compacting()),
            "in-flight must remain unset"
        );
    }

    /// End-to-end success: spawn → release → drain inbox → consume event →
    /// messages compacted in place AND in-flight cleared AND a boundary
    /// recorded. This is the full happy-path closing the loop the
    /// orchestrator runs in production.
    #[tokio::test]
    async fn round_trip_swap_completes_after_event_drained() {
        use crate::context::try_consume_compaction_event;

        let manager = Arc::new(BackgroundTaskManager::new());
        let (runtime, store, env) = make_phase_runtime(&manager);
        let (inbox_tx, mut inbox_rx) = crate::inbox::inbox_channel();
        manager.set_owner_inbox(inbox_tx);

        let gate = Arc::new(Notify::new());
        let summarizer: Arc<dyn ContextSummarizer> = Arc::new(GatedSummarizer {
            gate: gate.clone(),
            summary: "round trip summary".into(),
            observed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        let mut agent = make_resolved_agent(manager.clone(), summarizer);
        agent.env = env;
        let mut messages = make_long_messages();
        let original_len = messages.len();
        let identity = run_identity("thread-round-trip");
        let cancel = CancellationToken::new();
        let mut total_in = 0u64;
        let mut total_out = 0u64;
        let mut truncation = TruncationState::default();
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

        {
            let mut ctx = StepContext {
                agent: &mut agent,
                messages: &mut messages,
                runtime: &runtime,
                sink: sink.clone(),
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
            assert!(maybe_spawn_compaction(&mut ctx, &default_policy()).await);
        }

        gate.notify_one();
        let payload = tokio::time::timeout(Duration::from_secs(2), inbox_rx.recv_or_cancel(None))
            .await
            .expect("event arrives in time")
            .expect("event present");

        let consumed = try_consume_compaction_event(&mut messages, &payload, runtime.store());
        assert!(consumed, "router must claim compaction event");
        assert!(
            messages[0]
                .text()
                .contains("<conversation-summary>\nround trip summary"),
            "summary not at front: {}",
            messages[0].text()
        );
        assert!(
            messages.len() < original_len,
            "compaction must shrink the message list (was {original_len}, now {})",
            messages.len()
        );

        let final_state = store.read::<CompactionStateKey>().unwrap();
        assert!(!final_state.is_compacting(), "in-flight must be cleared");
        assert_eq!(
            final_state.boundaries.len(),
            1,
            "one boundary must be recorded"
        );
        assert_eq!(final_state.boundaries[0].summary, "round trip summary");
    }

    /// End-to-end failure: spawn → summarizer errs → failure event flows
    /// back → consume → in-flight cleared, no boundary recorded.
    #[tokio::test]
    async fn round_trip_failure_clears_in_flight() {
        use crate::context::try_consume_compaction_event;

        let manager = Arc::new(BackgroundTaskManager::new());
        let (runtime, store, env) = make_phase_runtime(&manager);
        let (inbox_tx, mut inbox_rx) = crate::inbox::inbox_channel();
        manager.set_owner_inbox(inbox_tx);

        let gate = Arc::new(Notify::new());
        let summarizer: Arc<dyn ContextSummarizer> = Arc::new(FailingSummarizer {
            gate: gate.clone(),
            message: "upstream timeout".into(),
        });
        let mut agent = make_resolved_agent(manager.clone(), summarizer);
        agent.env = env;
        let mut messages = make_long_messages();
        let identity = run_identity("thread-failure");
        let cancel = CancellationToken::new();
        let mut total_in = 0u64;
        let mut total_out = 0u64;
        let mut truncation = TruncationState::default();
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

        {
            let mut ctx = StepContext {
                agent: &mut agent,
                messages: &mut messages,
                runtime: &runtime,
                sink: sink.clone(),
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
            assert!(maybe_spawn_compaction(&mut ctx, &default_policy()).await);
        }
        let mid = store.read::<CompactionStateKey>().unwrap();
        assert!(mid.is_compacting());

        gate.notify_one();
        let payload = tokio::time::timeout(Duration::from_secs(2), inbox_rx.recv_or_cancel(None))
            .await
            .expect("event arrives in time")
            .expect("event present");
        assert_eq!(payload["event_type"], "context.compaction_failed");
        let err_text = payload["payload"]["error"].as_str().expect("error string");
        assert!(
            err_text.contains("upstream timeout"),
            "error payload should surface underlying message: {err_text}"
        );

        let consumed = try_consume_compaction_event(&mut messages, &payload, runtime.store());
        assert!(consumed);
        let after = store.read::<CompactionStateKey>().unwrap();
        assert!(!after.is_compacting(), "in-flight cleared after failure");
        assert!(
            after.boundaries.is_empty(),
            "failure must not record a boundary"
        );
    }

    /// Snapshot isolation: messages appended to the live list AFTER spawn
    /// must not appear in the transcript handed to the summarizer. The plan
    /// captures the transcript at trigger time and the background closure
    /// owns it for the duration of the LLM call.
    #[tokio::test]
    async fn background_summarizer_uses_snapshot_not_live_messages() {
        let manager = Arc::new(BackgroundTaskManager::new());
        let (runtime, store, env) = make_phase_runtime(&manager);
        let (inbox_tx, mut inbox_rx) = crate::inbox::inbox_channel();
        manager.set_owner_inbox(inbox_tx);

        let gate = Arc::new(Notify::new());
        let captured_transcript = Arc::new(std::sync::Mutex::new(None));
        let captured_previous = Arc::new(std::sync::Mutex::new(None));
        let summarizer: Arc<dyn ContextSummarizer> = Arc::new(CapturingSummarizer {
            gate: gate.clone(),
            captured_transcript: captured_transcript.clone(),
            captured_previous: captured_previous.clone(),
        });
        let mut agent = make_resolved_agent(manager.clone(), summarizer);
        agent.env = env;
        let mut messages = make_long_messages();
        let identity = run_identity("thread-snapshot");
        let cancel = CancellationToken::new();
        let mut total_in = 0u64;
        let mut total_out = 0u64;
        let mut truncation = TruncationState::default();
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

        {
            let mut ctx = StepContext {
                agent: &mut agent,
                messages: &mut messages,
                runtime: &runtime,
                sink: sink.clone(),
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
            assert!(maybe_spawn_compaction(&mut ctx, &default_policy()).await);
        }

        // Mutate the live list AFTER spawn — this must NOT reach the summarizer.
        messages.push(Arc::new(Message::user(
            "POSTSPAWN-MARKER-do-not-include-me",
        )));

        gate.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(2), inbox_rx.recv_or_cancel(None))
            .await
            .expect("event arrives in time");

        let transcript = captured_transcript.lock().unwrap().clone().unwrap();
        assert!(
            !transcript.contains("POSTSPAWN-MARKER"),
            "snapshot leaked live messages: {transcript}"
        );
        assert!(
            transcript.contains("filler"),
            "snapshot must contain pre-spawn content"
        );

        // Sanity: in-flight cleared after we drain the event in real flow,
        // but here we only consumed via inbox_rx so the marker may still be set.
        let _ = store; // suppress unused warning
    }

    /// Cumulative summarization: when an internal_system <conversation-summary>
    /// already exists in the message list, plan_compaction extracts it and
    /// the spawn helper hands it to the summarizer as previous_summary so the
    /// next pass produces an incremental update rather than re-summarizing
    /// already-summarized content.
    #[tokio::test]
    async fn previous_summary_is_passed_to_summarizer_on_subsequent_pass() {
        let manager = Arc::new(BackgroundTaskManager::new());
        let (runtime, store, env) = make_phase_runtime(&manager);
        let (inbox_tx, mut inbox_rx) = crate::inbox::inbox_channel();
        manager.set_owner_inbox(inbox_tx);

        let gate = Arc::new(Notify::new());
        let captured_transcript = Arc::new(std::sync::Mutex::new(None));
        let captured_previous = Arc::new(std::sync::Mutex::new(None));
        let summarizer: Arc<dyn ContextSummarizer> = Arc::new(CapturingSummarizer {
            gate: gate.clone(),
            captured_transcript: captured_transcript.clone(),
            captured_previous: captured_previous.clone(),
        });
        let mut agent = make_resolved_agent(manager.clone(), summarizer);
        agent.env = env;

        // Pre-existing summary at the head, then plenty of content after.
        let mut messages: Vec<Arc<Message>> = Vec::new();
        messages.push(Arc::new(Message::internal_system(
            "<conversation-summary>\nFirst pass summary text\n</conversation-summary>",
        )));
        for i in 0..6 {
            if i % 2 == 0 {
                messages.push(Arc::new(Message::user("filler ".repeat(600))));
            } else {
                messages.push(Arc::new(Message::assistant("ack")));
            }
        }
        messages.push(Arc::new(Message::user("recent")));

        let identity = run_identity("thread-cumulative");
        let cancel = CancellationToken::new();
        let mut total_in = 0u64;
        let mut total_out = 0u64;
        let mut truncation = TruncationState::default();
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

        {
            let mut ctx = StepContext {
                agent: &mut agent,
                messages: &mut messages,
                runtime: &runtime,
                sink: sink.clone(),
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
            assert!(maybe_spawn_compaction(&mut ctx, &default_policy()).await);
        }

        gate.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(2), inbox_rx.recv_or_cancel(None))
            .await
            .expect("event arrives in time");

        let prev = captured_previous.lock().unwrap().clone().unwrap();
        assert_eq!(
            prev.as_deref(),
            Some("First pass summary text"),
            "summarizer must receive the existing summary for cumulative update"
        );
        let _ = store;
    }

    /// Robustness: completion event with a missing/empty summary or boundary
    /// id must not panic and must still clear the in-flight marker. Defends
    /// against a faulty background task that emits a malformed payload.
    #[test]
    fn try_consume_compaction_event_handles_malformed_payload() {
        use crate::context::{
            CompactionInFlight, CompactionStateKey, record_compaction_in_flight,
            try_consume_compaction_event,
        };
        use crate::state::MutationBatch;
        use serde_json::json;

        let store = StateStore::new();
        store.install_plugin(CompactionPlugin::default()).unwrap();

        let mut messages: Vec<Arc<Message>> = vec![Arc::new(Message::user("only one"))];
        let mut batch = MutationBatch::new();
        batch.update::<CompactionStateKey>(record_compaction_in_flight(CompactionInFlight {
            task_id: "bg_77".into(),
            boundary_message_id: "any".into(),
            started_at_ms: 1,
        }));
        store.commit(batch).unwrap();
        assert!(store.read::<CompactionStateKey>().unwrap().is_compacting());

        // Missing payload entirely.
        let bad = json!({
            "kind": "custom",
            "task_id": "bg_77",
            "event_type": "context.compacted",
        });
        let consumed = try_consume_compaction_event(&mut messages, &bad, &store);
        assert!(consumed, "malformed compaction event still consumed");
        let state = store.read::<CompactionStateKey>().unwrap();
        assert!(
            !state.is_compacting(),
            "in-flight cleared even with malformed payload"
        );
        assert!(
            state.boundaries.is_empty(),
            "no boundary recorded for malformed payload"
        );
        assert_eq!(
            messages.len(),
            1,
            "live messages untouched on malformed payload"
        );
    }

    /// Persisted-state durability: CompactionInFlight must round-trip through
    /// JSON so a process restart preserves the marker. The orchestrator
    /// relies on the marker reaching the next process to suppress a
    /// duplicate compaction during recovery.
    #[test]
    fn compaction_in_flight_serde_roundtrips() {
        use crate::context::{CompactionInFlight, CompactionState};

        let state = CompactionState {
            in_flight: Some(CompactionInFlight {
                task_id: "bg_persisted".into(),
                boundary_message_id: "msg-id-stable".into(),
                started_at_ms: 4242,
            }),
            ..CompactionState::default()
        };

        let json = serde_json::to_string(&state).expect("serialize");
        let parsed: CompactionState = serde_json::from_str(&json).expect("deserialize");
        let live = parsed.in_flight.expect("in-flight survives roundtrip");
        assert_eq!(live.task_id, "bg_persisted");
        assert_eq!(live.boundary_message_id, "msg-id-stable");
        assert_eq!(live.started_at_ms, 4242);
    }
}
