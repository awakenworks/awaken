//! Agent runtime: top-level orchestrator for run management, routing, and control.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::agent::loop_runner::{
    AgentLoopError, AgentRunResult, prepare_resume, run_agent_loop_controlled,
};
use crate::contract::event_sink::EventSink;
use crate::contract::identity::RunIdentity;
use crate::contract::inference::InferenceOverride;
use crate::contract::message::Message;
use crate::contract::storage::ThreadRunStore;
use crate::contract::suspension::{ToolCallResume, ToolCallResumeMode};
use crate::error::StateError;
use futures::channel::mpsc;

use super::cancellation::CancellationToken;
use super::engine::PhaseRuntime;
use super::resolver::AgentResolver;

// ---------------------------------------------------------------------------
// RunRequest
// ---------------------------------------------------------------------------

/// Unified request for starting or resuming a run.
pub struct RunRequest {
    /// Main business input for this run.
    pub input: RunInput,
    /// Runtime control options (session/routing/overrides/resume).
    pub options: RunOptions,
}

/// Primary run input payload.
#[derive(Default)]
pub struct RunInput {
    /// New messages to append before running.
    pub messages: Vec<Message>,
}

/// Runtime control options for run execution.
pub struct RunOptions {
    /// Thread ID. Existing → load history; new → create.
    pub thread_id: String,
    /// Target agent ID. `None` = use default or infer from thread state.
    pub agent_id: Option<String>,
    /// Runtime parameter overrides for this run.
    pub overrides: Option<InferenceOverride>,
    /// Resume decisions for suspended tool calls. Empty = fresh run.
    pub decisions: Vec<(String, ToolCallResume)>,
}

impl RunRequest {
    /// Build a message-first request with default options.
    pub fn new(thread_id: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            input: RunInput { messages },
            options: RunOptions {
                thread_id: thread_id.into(),
                agent_id: None,
                overrides: None,
                decisions: Vec::new(),
            },
        }
    }

    #[must_use]
    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.options.agent_id = Some(agent_id.into());
        self
    }

    #[must_use]
    pub fn with_overrides(mut self, overrides: InferenceOverride) -> Self {
        self.options.overrides = Some(overrides);
        self
    }

    #[must_use]
    pub fn with_decisions(mut self, decisions: Vec<(String, ToolCallResume)>) -> Self {
        self.options.decisions = decisions;
        self
    }
}

// ---------------------------------------------------------------------------
// RunHandle
// ---------------------------------------------------------------------------

/// External control handle for a running agent loop.
///
/// Returned by `AgentRuntime`. Enables cancellation and
/// live decision injection.
#[derive(Clone)]
pub struct RunHandle {
    pub run_id: String,
    pub thread_id: String,
    pub agent_id: String,
    cancellation_token: CancellationToken,
    decision_tx: mpsc::UnboundedSender<(String, ToolCallResume)>,
}

impl RunHandle {
    /// Cancel the running agent loop cooperatively.
    pub fn cancel(&self) {
        self.cancellation_token.cancel();
    }

    /// Send a tool call decision to the running loop.
    pub fn send_decision(
        &self,
        call_id: String,
        resume: ToolCallResume,
    ) -> Result<(), mpsc::TrySendError<(String, ToolCallResume)>> {
        self.decision_tx.unbounded_send((call_id, resume))
    }
}

// ---------------------------------------------------------------------------
// ActiveRunRegistry
// ---------------------------------------------------------------------------

struct RunEntry {
    #[allow(dead_code)]
    run_id: String,
    #[allow(dead_code)]
    agent_id: String,
    handle: RunHandle,
}

/// Tracks active runs. At most one active run per thread.
pub(crate) struct ActiveRunRegistry {
    by_thread_id: RwLock<HashMap<String, RunEntry>>,
}

impl ActiveRunRegistry {
    pub(crate) fn new() -> Self {
        Self {
            by_thread_id: RwLock::new(HashMap::new()),
        }
    }

    fn insert(&self, thread_id: String, entry: RunEntry) {
        self.by_thread_id
            .write()
            .expect("active runs lock poisoned")
            .insert(thread_id, entry);
    }

    fn remove(&self, thread_id: &str) {
        self.by_thread_id
            .write()
            .expect("active runs lock poisoned")
            .remove(thread_id);
    }

    fn get_handle(&self, thread_id: &str) -> Option<RunHandle> {
        self.by_thread_id
            .read()
            .expect("active runs lock poisoned")
            .get(thread_id)
            .map(|e| e.handle.clone())
    }

    fn has_active_run(&self, thread_id: &str) -> bool {
        self.by_thread_id
            .read()
            .expect("active runs lock poisoned")
            .contains_key(thread_id)
    }
}

// ---------------------------------------------------------------------------
// AgentRuntime
// ---------------------------------------------------------------------------

/// Top-level agent runtime. Manages runs across threads.
///
/// Provides methods for cancelling and sending decisions
/// to active agent runs. Enforces one active run per thread.
pub struct AgentRuntime {
    resolver: Arc<dyn AgentResolver>,
    storage: Option<Arc<dyn ThreadRunStore>>,
    active_runs: ActiveRunRegistry,
}

impl AgentRuntime {
    pub fn new(resolver: Arc<dyn AgentResolver>) -> Self {
        Self {
            resolver,
            storage: None,
            active_runs: ActiveRunRegistry::new(),
        }
    }

    #[must_use]
    pub fn with_thread_run_store(mut self, store: Arc<dyn ThreadRunStore>) -> Self {
        self.storage = Some(store);
        self
    }

    pub fn resolver(&self) -> &dyn AgentResolver {
        self.resolver.as_ref()
    }

    pub fn thread_run_store(&self) -> Option<&dyn ThreadRunStore> {
        self.storage.as_deref()
    }

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
    /// Returns a `RunHandle` for external control (cancel, send decisions)
    /// and the `AgentRunResult` when the run completes.
    pub async fn run(
        &self,
        request: RunRequest,
        sink: &dyn EventSink,
    ) -> Result<(RunHandle, AgentRunResult), AgentLoopError> {
        let RunRequest { input, options } = request;
        let RunOptions {
            thread_id,
            agent_id,
            overrides,
            decisions,
        } = options;
        let request_messages = input.messages;
        let agent_id = agent_id.unwrap_or_else(|| "default".to_string());

        // Create runtime infrastructure
        let store = crate::state::StateStore::new();
        let phase_runtime = PhaseRuntime::new(store.clone()).map_err(AgentLoopError::PhaseError)?;

        // Install state keys needed by the loop (RunLifecycle, ToolCallStates, etc.)
        // These are registered via the resolved agent's plugins during resolve.
        // For keys needed by the loop itself, install a minimal plugin.
        store
            .install_plugin(crate::agent::loop_runner::LoopStatePlugin)
            .map_err(AgentLoopError::PhaseError)?;

        // Load existing thread messages and restore thread-scoped state
        let mut messages = if let Some(ref ts) = self.storage {
            // Restore thread-scoped state from the latest run checkpoint
            if let Some(prev_run) = ts
                .latest_run(&thread_id)
                .await
                .map_err(|e| AgentLoopError::StorageError(e.to_string()))?
            {
                if let Some(persisted) = prev_run.state {
                    store
                        .restore_thread_scoped(persisted, crate::error::UnknownKeyPolicy::Skip)
                        .map_err(AgentLoopError::PhaseError)?;
                }
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
            crate::contract::identity::RunOrigin::User,
        );

        // Create channels for external control
        let (handle, cancellation_token, decision_rx) =
            self.create_run_channels(run_id, thread_id.clone(), agent_id.clone());

        // Register active run
        self.register_run(&thread_id, handle.clone())
            .map_err(AgentLoopError::PhaseError)?;

        // Execute the loop
        let checkpoint_store_ref = self.storage.as_deref();
        let result = run_agent_loop_controlled(
            self.resolver.as_ref(),
            &agent_id,
            &phase_runtime,
            sink,
            checkpoint_store_ref,
            messages,
            run_identity,
            Some(cancellation_token),
            Some(decision_rx),
            overrides,
        )
        .await;

        // Unregister active run
        self.unregister_run(&thread_id);

        Ok((handle, result?))
    }

    /// Cancel an active run by thread ID.
    pub fn cancel_by_thread(&self, thread_id: &str) -> bool {
        if let Some(handle) = self.active_runs.get_handle(thread_id) {
            handle.cancel();
            true
        } else {
            false
        }
    }

    /// Send decisions to an active run by thread ID.
    pub fn send_decisions(
        &self,
        thread_id: &str,
        decisions: Vec<(String, ToolCallResume)>,
    ) -> bool {
        if let Some(handle) = self.active_runs.get_handle(thread_id) {
            for (call_id, resume) in decisions {
                if handle.send_decision(call_id, resume).is_err() {
                    return false;
                }
            }
            true
        } else {
            false
        }
    }

    /// Create a run handle pair (handle + internal channels).
    ///
    /// Returns (RunHandle for caller, CancellationToken for loop, decision_rx for loop).
    pub(crate) fn create_run_channels(
        &self,
        run_id: String,
        thread_id: String,
        agent_id: String,
    ) -> (
        RunHandle,
        CancellationToken,
        mpsc::UnboundedReceiver<(String, ToolCallResume)>,
    ) {
        let token = CancellationToken::new();
        let (tx, rx) = mpsc::unbounded();

        let handle = RunHandle {
            run_id,
            thread_id,
            agent_id,
            cancellation_token: token.clone(),
            decision_tx: tx,
        };

        (handle, token, rx)
    }

    /// Register an active run. Returns error if thread already has one.
    pub(crate) fn register_run(
        &self,
        thread_id: &str,
        handle: RunHandle,
    ) -> Result<(), StateError> {
        if self.active_runs.has_active_run(thread_id) {
            return Err(StateError::ThreadAlreadyRunning {
                thread_id: thread_id.to_string(),
            });
        }
        self.active_runs.insert(
            thread_id.to_string(),
            RunEntry {
                run_id: handle.run_id.clone(),
                agent_id: handle.agent_id.clone(),
                handle,
            },
        );
        Ok(())
    }

    /// Unregister an active run when it completes.
    pub(crate) fn unregister_run(&self, thread_id: &str) {
        self.active_runs.remove(thread_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::config::AgentConfig;
    use crate::agent::loop_runner::build_agent_env;
    use crate::contract::content::ContentBlock;
    use crate::contract::event_sink::NullEventSink;
    use crate::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
    use crate::contract::inference::{StopReason, StreamResult};
    use crate::contract::message::Message;
    use crate::contract::storage::ThreadRunStore;
    use crate::contract::storage_mem::InMemoryThreadRunStore;
    use crate::contract::suspension::ResumeDecisionAction;
    use crate::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult};
    use crate::runtime::ResolvedAgent;
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
    }

    impl AgentResolver for FixedResolver {
        fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, StateError> {
            let env = build_agent_env(&[], &self.agent)?;
            Ok(ResolvedAgent {
                config: self.agent.clone(),
                env,
            })
        }
    }

    #[tokio::test]
    async fn run_request_overrides_are_forwarded_to_inference() {
        let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
            content: vec![ContentBlock::text("ok")],
            tool_calls: vec![],
            usage: Some(crate::contract::inference::TokenUsage {
                prompt_tokens: Some(11),
                completion_tokens: Some(7),
                ..Default::default()
            }),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        }]));
        let resolver = Arc::new(FixedResolver {
            agent: AgentConfig::new("agent", "m", "sys", llm.clone()),
        });
        let runtime = AgentRuntime::new(resolver);
        let sink = NullEventSink;
        let override_req = InferenceOverride {
            temperature: Some(0.3),
            max_tokens: Some(77),
            ..Default::default()
        };

        let (_handle, result) = runtime
            .run(
                RunRequest::new("thread-ovr", vec![Message::user("hi")])
                    .with_agent_id("agent")
                    .with_overrides(override_req.clone()),
                &sink,
            )
            .await
            .expect("run should succeed");

        assert_eq!(
            result.termination,
            crate::contract::lifecycle::TerminationReason::NaturalEnd
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
                tool_calls: vec![crate::contract::message::ToolCall::new(
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
        });
        let runtime = Arc::new(AgentRuntime::new(resolver));

        let run_task = {
            let runtime = Arc::clone(&runtime);
            tokio::spawn(async move {
                let sink = NullEventSink;
                runtime
                    .run(
                        RunRequest::new("thread-live", vec![Message::user("go")])
                            .with_agent_id("agent"),
                        &sink,
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

        let (_handle, result) = run_task
            .await
            .expect("join should succeed")
            .expect("run should succeed");
        assert_eq!(
            result.termination,
            crate::contract::lifecycle::TerminationReason::NaturalEnd
        );
    }

    #[tokio::test]
    async fn checkpoint_persists_state_and_thread_together() {
        let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
            content: vec![ContentBlock::text("ok")],
            tool_calls: vec![],
            usage: Some(crate::contract::inference::TokenUsage {
                prompt_tokens: Some(11),
                completion_tokens: Some(7),
                ..Default::default()
            }),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        }]));
        let resolver = Arc::new(FixedResolver {
            agent: AgentConfig::new("agent", "m", "sys", llm),
        });
        let store = Arc::new(InMemoryThreadRunStore::new());
        let runtime = AgentRuntime::new(resolver)
            .with_thread_run_store(store.clone() as Arc<dyn ThreadRunStore>);
        let sink = NullEventSink;

        let (_handle, result) = runtime
            .run(
                RunRequest::new("thread-tx", vec![Message::user("hi")]).with_agent_id("agent"),
                &sink,
            )
            .await
            .expect("run should succeed");
        assert_eq!(
            result.termination,
            crate::contract::lifecycle::TerminationReason::NaturalEnd
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
                            crate::contract::executor::InferenceStream,
                            InferenceExecutionError,
                        >,
                    > + Send
                    + '_,
            >,
        > {
            use crate::contract::executor::{InferenceStream, StreamEvent};
            use crate::contract::inference::TokenUsage;

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
                    let events = crate::contract::executor::collected_to_stream_events(result);
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
        });
        let runtime = AgentRuntime::new(resolver);
        let sink = NullEventSink;

        let (_handle, result) = runtime
            .run(
                RunRequest::new("thread-trunc", vec![Message::user("hi")]).with_agent_id("agent"),
                &sink,
            )
            .await
            .expect("run should succeed");

        assert_eq!(
            result.termination,
            crate::contract::lifecycle::TerminationReason::NaturalEnd
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
                                crate::contract::executor::InferenceStream,
                                InferenceExecutionError,
                            >,
                        > + Send
                        + '_,
                >,
            > {
                use crate::contract::executor::{InferenceStream, StreamEvent};
                use crate::contract::inference::TokenUsage;

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
        });
        let runtime = AgentRuntime::new(resolver);
        let sink = NullEventSink;

        let (_handle, result) = runtime
            .run(
                RunRequest::new("thread-trunc-max", vec![Message::user("hi")])
                    .with_agent_id("agent"),
                &sink,
            )
            .await
            .expect("run should succeed");

        // Should give up after 1 initial + 2 retries = 3 calls total
        assert_eq!(llm.call_count.load(Ordering::SeqCst), 3);
        // After giving up, the result has no tools, so it ends naturally
        // with the text from the last truncated response
        assert_eq!(
            result.termination,
            crate::contract::lifecycle::TerminationReason::NaturalEnd
        );
        assert_eq!(result.response, "truncated ");
    }
}
