//! AgentRuntime::run() implementation.

use std::sync::Arc;

use crate::loop_runner::{
    AgentLoopError, AgentLoopParams, AgentRunResult, prepare_resume, run_agent_loop,
};
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::suspension::ToolCallResumeMode;

use super::AgentRuntime;
use super::run_request::RunRequest;

const DEFAULT_AGENT_ID: &str = "default";

/// RAII guard that unregisters the active run on drop, ensuring cleanup
/// even if the run future panics or is cancelled.
struct RunSlotGuard<'a> {
    runtime: &'a AgentRuntime,
    run_id: String,
}

impl Drop for RunSlotGuard<'_> {
    fn drop(&mut self) {
        self.runtime.unregister_run(&self.run_id);
    }
}

impl AgentRuntime {
    /// Run an agent loop.
    ///
    /// This is the single production entry point. It:
    /// 1. Resolves the agent from the registry
    /// 2. Loads thread messages from storage (if configured)
    /// 3. Applies resume decisions (if present in request)
    /// 4. Creates a PhaseRuntime and StateStore
    /// 5. Registers the active run
    /// 6. Calls `run_agent_loop` internally
    /// 7. Unregisters the run when complete
    ///
    /// Run an agent loop. Returns the result when the run completes.
    ///
    /// Use `cancel()` / `send_decisions()` on `AgentRuntime` for external
    /// control of in-flight runs.
    pub async fn run(
        &self,
        request: RunRequest,
        sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        let RunRequest {
            messages: request_messages,
            thread_id,
            agent_id,
            overrides,
            decisions,
        } = request;
        let agent_id = agent_id.unwrap_or_else(|| DEFAULT_AGENT_ID.to_string());

        // Create runtime infrastructure
        let store = crate::state::StateStore::new();
        let phase_runtime =
            crate::phase::PhaseRuntime::new(store.clone()).map_err(AgentLoopError::PhaseError)?;

        // Install state keys needed by the loop (RunLifecycle, ToolCallStates, etc.)
        // These are registered via the resolved agent's plugins during resolve.
        // For keys needed by the loop itself, install a minimal plugin.
        store
            .install_plugin(crate::loop_runner::LoopStatePlugin)
            .map_err(AgentLoopError::PhaseError)?;

        // Preflight resolve to register plugin-declared keys before restoring persisted state.
        // Without this, thread-scoped keys may be skipped as unknown during restore.
        let preflight_key_registrations = self
            .resolver
            .resolve(&agent_id)
            .map_err(AgentLoopError::RuntimeError)?
            .env
            .key_registrations;
        if !preflight_key_registrations.is_empty() {
            store
                .register_keys(&preflight_key_registrations)
                .map_err(AgentLoopError::PhaseError)?;
        }

        // Load existing thread messages and restore thread-scoped state
        let mut messages = if let Some(ref ts) = self.storage {
            // Restore thread-scoped state from the latest run checkpoint
            if let Some(prev_run) = ts
                .latest_run(&thread_id)
                .await
                .map_err(|e| AgentLoopError::StorageError(e.to_string()))?
                && let Some(persisted) = prev_run.state
            {
                store
                    .restore_thread_scoped(persisted, awaken_contract::UnknownKeyPolicy::Skip)
                    .map_err(AgentLoopError::PhaseError)?;
            }
            ts.load_messages(&thread_id)
                .await
                .map_err(|e| AgentLoopError::StorageError(e.to_string()))?
                .unwrap_or_default()
        } else {
            vec![]
        };
        messages.extend(request_messages);

        // Apply resume decisions to state if present
        if !decisions.is_empty() {
            prepare_resume(&store, decisions, ToolCallResumeMode::ReplayToolCall)
                .map_err(AgentLoopError::PhaseError)?;
        }

        // Create run identity
        let run_id = uuid::Uuid::now_v7().to_string();
        let run_identity = RunIdentity::new(
            thread_id.clone(),
            None,
            run_id.clone(),
            None,
            agent_id.clone(),
            awaken_contract::contract::identity::RunOrigin::User,
        );

        // Create channels for external control
        let (handle, cancellation_token, decision_rx) = self.create_run_channels(run_id.clone());

        // Register active run (guard ensures cleanup on drop/panic/cancellation)
        self.register_run(&thread_id, handle)
            .map_err(AgentLoopError::RuntimeError)?;
        let _guard = RunSlotGuard {
            runtime: self,
            run_id: run_id.clone(),
        };

        // Execute the loop
        let result = run_agent_loop(AgentLoopParams {
            resolver: self.resolver.as_ref(),
            agent_id: &agent_id,
            runtime: &phase_runtime,
            sink,
            checkpoint_store: self.storage.as_deref(),
            messages,
            run_identity,
            cancellation_token: Some(cancellation_token),
            decision_rx: Some(decision_rx),
            overrides,
        })
        .await;

        // Guard drops here, calling unregister_run automatically

        result
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::agent::config::AgentConfig;
    use crate::loop_runner::build_agent_env;
    use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
    use crate::registry::{AgentResolver, ResolvedAgent};
    use crate::state::{KeyScope, StateCommand, StateKey, StateKeyOptions};
    use crate::{PhaseContext, PhaseHook};
    use async_trait::async_trait;
    use awaken_contract::contract::content::ContentBlock;
    use awaken_contract::contract::event_sink::{EventSink, NullEventSink};
    use awaken_contract::contract::executor::{
        InferenceExecutionError, InferenceRequest, LlmExecutor,
    };
    use awaken_contract::contract::inference::{InferenceOverride, StopReason, StreamResult};
    use awaken_contract::contract::message::Message;
    use awaken_contract::contract::storage::{RunQuery, RunStore, ThreadRunStore, ThreadStore};
    use awaken_contract::contract::suspension::ResumeDecisionAction;
    use awaken_contract::contract::suspension::ToolCallResume;
    use awaken_contract::contract::tool::{
        Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult,
    };
    use awaken_stores::InMemoryStore;
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    struct ScriptedLlm {
        responses: Mutex<Vec<StreamResult>>,
        seen_overrides: Mutex<Vec<Option<InferenceOverride>>>,
    }

    impl ScriptedLlm {
        fn new(responses: Vec<StreamResult>) -> Self {
            Self {
                responses: Mutex::new(responses),
                seen_overrides: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl LlmExecutor for ScriptedLlm {
        async fn execute(
            &self,
            request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            self.seen_overrides
                .lock()
                .expect("lock poisoned")
                .push(request.overrides.clone());
            let mut responses = self.responses.lock().expect("lock poisoned");
            if responses.is_empty() {
                Ok(StreamResult {
                    content: vec![ContentBlock::text("done")],
                    tool_calls: vec![],
                    usage: None,
                    stop_reason: Some(StopReason::EndTurn),
                    has_incomplete_tool_calls: false,
                })
            } else {
                Ok(responses.remove(0))
            }
        }

        fn name(&self) -> &str {
            "scripted"
        }
    }

    struct ToggleSuspendTool {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Tool for ToggleSuspendTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("dangerous", "dangerous", "suspend then succeed")
        }

        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolResult, ToolError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(ToolResult::suspended("dangerous", "needs approval"))
            } else {
                Ok(ToolResult::success_with_message(
                    "dangerous",
                    args,
                    "approved",
                ))
            }
        }
    }

    struct FixedResolver {
        agent: AgentConfig,
        plugins: Vec<Arc<dyn Plugin>>,
    }

    impl AgentResolver for FixedResolver {
        fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, crate::error::RuntimeError> {
            let env = build_agent_env(&self.plugins, &self.agent)?;
            Ok(ResolvedAgent {
                config: self.agent.clone(),
                env,
            })
        }
    }

    struct ThreadCounterKey;

    impl StateKey for ThreadCounterKey {
        const KEY: &'static str = "test.thread_counter";
        type Value = u32;
        type Update = u32;

        fn apply(value: &mut Self::Value, update: Self::Update) {
            *value = update;
        }
    }

    struct ThreadCounterPlugin;

    impl Plugin for ThreadCounterPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "test.thread-counter",
            }
        }

        fn register(
            &self,
            registrar: &mut PluginRegistrar,
        ) -> Result<(), awaken_contract::StateError> {
            registrar.register_key::<ThreadCounterKey>(StateKeyOptions {
                persistent: true,
                scope: KeyScope::Thread,
                ..StateKeyOptions::default()
            })?;
            registrar.register_phase_hook(
                "test.thread-counter",
                awaken_contract::model::Phase::RunStart,
                ThreadCounterHook,
            )
        }
    }

    struct ThreadCounterHook;

    #[async_trait]
    impl PhaseHook for ThreadCounterHook {
        async fn run(
            &self,
            ctx: &PhaseContext,
        ) -> Result<StateCommand, awaken_contract::StateError> {
            let next = ctx.state::<ThreadCounterKey>().copied().unwrap_or(0) + 1;
            let mut cmd = StateCommand::new();
            cmd.update::<ThreadCounterKey>(next);
            Ok(cmd)
        }
    }

    #[tokio::test]
    async fn run_request_overrides_are_forwarded_to_inference() {
        let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
            content: vec![ContentBlock::text("ok")],
            tool_calls: vec![],
            usage: Some(awaken_contract::contract::inference::TokenUsage {
                prompt_tokens: Some(11),
                completion_tokens: Some(7),
                ..Default::default()
            }),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        }]));
        let resolver = Arc::new(FixedResolver {
            agent: AgentConfig::new("agent", "m", "sys", llm.clone()),
            plugins: vec![],
        });
        let runtime = AgentRuntime::new(resolver);
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);
        let override_req = InferenceOverride {
            temperature: Some(0.3),
            max_tokens: Some(77),
            ..Default::default()
        };

        let result = runtime
            .run(
                RunRequest::new("thread-ovr", vec![Message::user("hi")])
                    .with_agent_id("agent")
                    .with_overrides(override_req.clone()),
                sink.clone(),
            )
            .await
            .expect("run should succeed");

        assert_eq!(
            result.termination,
            awaken_contract::contract::lifecycle::TerminationReason::NaturalEnd
        );
        let seen = llm.seen_overrides.lock().expect("lock poisoned");
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].as_ref().and_then(|o| o.temperature),
            override_req.temperature
        );
        assert_eq!(
            seen[0].as_ref().and_then(|o| o.max_tokens),
            override_req.max_tokens
        );
    }

    #[tokio::test]
    async fn send_decisions_resumes_waiting_run() {
        let llm = Arc::new(ScriptedLlm::new(vec![
            StreamResult {
                content: vec![ContentBlock::text("calling tool")],
                tool_calls: vec![awaken_contract::contract::message::ToolCall::new(
                    "c1",
                    "dangerous",
                    json!({"x": 1}),
                )],
                usage: None,
                stop_reason: Some(StopReason::ToolUse),
                has_incomplete_tool_calls: false,
            },
            StreamResult {
                content: vec![ContentBlock::text("finished")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            },
        ]));
        let tool = Arc::new(ToggleSuspendTool {
            calls: AtomicUsize::new(0),
        });
        let resolver = Arc::new(FixedResolver {
            agent: AgentConfig::new("agent", "m", "sys", llm).with_tool(tool),
            plugins: vec![],
        });
        let runtime = Arc::new(AgentRuntime::new(resolver));

        let run_task = {
            let runtime = Arc::clone(&runtime);
            tokio::spawn(async move {
                let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);
                runtime
                    .run(
                        RunRequest::new("thread-live", vec![Message::user("go")])
                            .with_agent_id("agent"),
                        sink.clone(),
                    )
                    .await
            })
        };

        let mut sent = false;
        for _ in 0..40 {
            if runtime.send_decisions(
                "thread-live",
                vec![(
                    "c1".into(),
                    ToolCallResume {
                        decision_id: "d1".into(),
                        action: ResumeDecisionAction::Resume,
                        result: Value::Null,
                        reason: None,
                        updated_at: 1,
                    },
                )],
            ) {
                sent = true;
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(sent, "should send decision while run is active");

        let result = run_task
            .await
            .expect("join should succeed")
            .expect("run should succeed");
        assert_eq!(
            result.termination,
            awaken_contract::contract::lifecycle::TerminationReason::NaturalEnd
        );
    }

    #[tokio::test]
    async fn checkpoint_persists_state_and_thread_together() {
        let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
            content: vec![ContentBlock::text("ok")],
            tool_calls: vec![],
            usage: Some(awaken_contract::contract::inference::TokenUsage {
                prompt_tokens: Some(11),
                completion_tokens: Some(7),
                ..Default::default()
            }),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        }]));
        let resolver = Arc::new(FixedResolver {
            agent: AgentConfig::new("agent", "m", "sys", llm),
            plugins: vec![],
        });
        let store = Arc::new(InMemoryStore::new());
        let runtime = AgentRuntime::new(resolver)
            .with_thread_run_store(store.clone() as Arc<dyn ThreadRunStore>);
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

        let result = runtime
            .run(
                RunRequest::new("thread-tx", vec![Message::user("hi")]).with_agent_id("agent"),
                sink.clone(),
            )
            .await
            .expect("run should succeed");
        assert_eq!(
            result.termination,
            awaken_contract::contract::lifecycle::TerminationReason::NaturalEnd
        );

        let latest = store
            .latest_run("thread-tx")
            .await
            .expect("latest run lookup")
            .expect("run persisted");
        assert_eq!(latest.thread_id, "thread-tx");
        assert!(latest.state.is_some(), "state snapshot should be persisted");
        assert_eq!(latest.input_tokens, 11);
        assert_eq!(latest.output_tokens, 7);

        let msgs = store
            .load_messages("thread-tx")
            .await
            .expect("load messages")
            .expect("thread should exist");
        assert!(!msgs.is_empty());
    }

    #[tokio::test]
    async fn thread_scoped_state_restores_before_run_start_hooks() {
        let llm = Arc::new(ScriptedLlm::new(vec![
            StreamResult {
                content: vec![ContentBlock::text("ok-1")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            },
            StreamResult {
                content: vec![ContentBlock::text("ok-2")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            },
        ]));
        let resolver = Arc::new(FixedResolver {
            agent: AgentConfig::new("agent", "m", "sys", llm),
            plugins: vec![Arc::new(ThreadCounterPlugin)],
        });
        let store = Arc::new(InMemoryStore::new());
        let runtime = AgentRuntime::new(resolver)
            .with_thread_run_store(store.clone() as Arc<dyn ThreadRunStore>);
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

        runtime
            .run(
                RunRequest::new("thread-counter", vec![Message::user("first")])
                    .with_agent_id("agent"),
                sink.clone(),
            )
            .await
            .expect("first run should succeed");

        runtime
            .run(
                RunRequest::new("thread-counter", vec![Message::user("second")])
                    .with_agent_id("agent"),
                sink.clone(),
            )
            .await
            .expect("second run should succeed");

        let runs = store
            .list_runs(&RunQuery {
                thread_id: Some("thread-counter".into()),
                ..RunQuery::default()
            })
            .await
            .expect("run list lookup");

        let max_counter = runs
            .items
            .iter()
            .filter_map(|record| record.state.as_ref())
            .filter_map(|persisted| persisted.extensions.get(ThreadCounterKey::KEY))
            .filter_map(serde_json::Value::as_u64)
            .max()
            .expect("thread counter should be persisted");
        assert_eq!(max_counter, 2, "counter should continue across runs");
    }

    // -----------------------------------------------------------------------
    // Truncation recovery tests
    // -----------------------------------------------------------------------

    /// LLM executor that emits truncated tool call JSON on the first call,
    /// then a normal response on subsequent calls.
    struct TruncatingLlm {
        call_count: AtomicUsize,
        /// Responses to return after the first (truncated) call.
        followup_responses: Mutex<Vec<StreamResult>>,
    }

    impl TruncatingLlm {
        fn new(followup_responses: Vec<StreamResult>) -> Self {
            Self {
                call_count: AtomicUsize::new(0),
                followup_responses: Mutex::new(followup_responses),
            }
        }
    }

    #[async_trait]
    impl LlmExecutor for TruncatingLlm {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            unreachable!("execute_stream is overridden");
        }

        fn execute_stream(
            &self,
            _request: InferenceRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            awaken_contract::contract::executor::InferenceStream,
                            InferenceExecutionError,
                        >,
                    > + Send
                    + '_,
            >,
        > {
            use awaken_contract::contract::executor::{InferenceStream, StreamEvent};
            use awaken_contract::contract::inference::TokenUsage;

            Box::pin(async move {
                let n = self.call_count.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    // First call: emit a tool call with truncated JSON, then MaxTokens
                    let events: Vec<Result<StreamEvent, InferenceExecutionError>> = vec![
                        Ok(StreamEvent::TextDelta("partial ".into())),
                        Ok(StreamEvent::ToolCallStart {
                            id: "tc1".into(),
                            name: "calculator".into(),
                        }),
                        // Truncated JSON: missing closing brace
                        Ok(StreamEvent::ToolCallDelta {
                            id: "tc1".into(),
                            args_delta: r#"{"expr": "1+1"#.into(),
                        }),
                        Ok(StreamEvent::Usage(TokenUsage {
                            prompt_tokens: Some(50),
                            completion_tokens: Some(100),
                            ..Default::default()
                        })),
                        Ok(StreamEvent::Stop(StopReason::MaxTokens)),
                    ];
                    Ok(Box::pin(futures::stream::iter(events)) as InferenceStream)
                } else {
                    // Subsequent calls: return from followup queue
                    let mut followups = self.followup_responses.lock().expect("lock poisoned");
                    let result = if followups.is_empty() {
                        StreamResult {
                            content: vec![ContentBlock::text("final response")],
                            tool_calls: vec![],
                            usage: None,
                            stop_reason: Some(StopReason::EndTurn),
                            has_incomplete_tool_calls: false,
                        }
                    } else {
                        followups.remove(0)
                    };
                    let events =
                        awaken_contract::contract::executor::collected_to_stream_events(result);
                    Ok(Box::pin(futures::stream::iter(events)) as InferenceStream)
                }
            })
        }

        fn name(&self) -> &str {
            "truncating"
        }
    }

    #[tokio::test]
    async fn truncation_recovery_continues_on_max_tokens() {
        // First call returns MaxTokens with truncated tool call
        // Second call returns EndTurn with final text
        let llm = Arc::new(TruncatingLlm::new(vec![StreamResult {
            content: vec![ContentBlock::text("completed response")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        }]));
        let resolver = Arc::new(FixedResolver {
            agent: AgentConfig::new("agent", "m", "sys", llm.clone())
                .with_max_continuation_retries(2),
            plugins: vec![],
        });
        let runtime = AgentRuntime::new(resolver);
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

        let result = runtime
            .run(
                RunRequest::new("thread-trunc", vec![Message::user("hi")]).with_agent_id("agent"),
                sink.clone(),
            )
            .await
            .expect("run should succeed");

        assert_eq!(
            result.termination,
            awaken_contract::contract::lifecycle::TerminationReason::NaturalEnd
        );
        // The final response should be from the second (continuation) call
        assert_eq!(result.response, "completed response");
        // Two calls total: truncated + continuation
        assert_eq!(llm.call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn truncation_recovery_gives_up_after_max_retries() {
        // All calls return MaxTokens with truncated tool calls
        // (the TruncatingLlm always returns truncated on first call,
        //  and we provide followups that are also truncated)
        struct AlwaysTruncatingLlm {
            call_count: AtomicUsize,
        }

        #[async_trait]
        impl LlmExecutor for AlwaysTruncatingLlm {
            async fn execute(
                &self,
                _request: InferenceRequest,
            ) -> Result<StreamResult, InferenceExecutionError> {
                unreachable!("execute_stream is overridden");
            }

            fn execute_stream(
                &self,
                _request: InferenceRequest,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                awaken_contract::contract::executor::InferenceStream,
                                InferenceExecutionError,
                            >,
                        > + Send
                        + '_,
                >,
            > {
                use awaken_contract::contract::executor::{InferenceStream, StreamEvent};
                use awaken_contract::contract::inference::TokenUsage;

                Box::pin(async move {
                    self.call_count.fetch_add(1, Ordering::SeqCst);
                    // Always return truncated tool call
                    let events: Vec<Result<StreamEvent, InferenceExecutionError>> = vec![
                        Ok(StreamEvent::TextDelta("truncated ".into())),
                        Ok(StreamEvent::ToolCallStart {
                            id: format!("tc{}", self.call_count.load(Ordering::SeqCst)),
                            name: "calculator".into(),
                        }),
                        Ok(StreamEvent::ToolCallDelta {
                            id: format!("tc{}", self.call_count.load(Ordering::SeqCst)),
                            args_delta: r#"{"incomplete"#.into(),
                        }),
                        Ok(StreamEvent::Usage(TokenUsage {
                            prompt_tokens: Some(50),
                            completion_tokens: Some(100),
                            ..Default::default()
                        })),
                        Ok(StreamEvent::Stop(StopReason::MaxTokens)),
                    ];
                    Ok(Box::pin(futures::stream::iter(events)) as InferenceStream)
                })
            }

            fn name(&self) -> &str {
                "always_truncating"
            }
        }

        let llm = Arc::new(AlwaysTruncatingLlm {
            call_count: AtomicUsize::new(0),
        });
        let resolver = Arc::new(FixedResolver {
            agent: AgentConfig::new("agent", "m", "sys", llm.clone())
                .with_max_continuation_retries(2),
            plugins: vec![],
        });
        let runtime = AgentRuntime::new(resolver);
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

        let result = runtime
            .run(
                RunRequest::new("thread-trunc-max", vec![Message::user("hi")])
                    .with_agent_id("agent"),
                sink.clone(),
            )
            .await
            .expect("run should succeed");

        // Should give up after 1 initial + 2 retries = 3 calls total
        assert_eq!(llm.call_count.load(Ordering::SeqCst), 3);
        // After giving up, the result has no tools, so it ends naturally
        // with the text from the last truncated response
        assert_eq!(
            result.termination,
            awaken_contract::contract::lifecycle::TerminationReason::NaturalEnd
        );
        assert_eq!(result.response, "truncated ");
    }
}
