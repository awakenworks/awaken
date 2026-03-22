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
    /// Target agent ID. `None` = use default or infer from thread state.
    pub agent_id: Option<String>,
    /// Thread ID. Existing → load history; new → create.
    pub thread_id: String,
    /// New messages to append before running.
    pub messages: Vec<Message>,
    /// Runtime parameter overrides for this run.
    pub overrides: Option<InferenceOverride>,
    /// Resume decisions for suspended tool calls. Empty = fresh run.
    pub decisions: Vec<(String, ToolCallResume)>,
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
        let RunRequest {
            agent_id,
            thread_id,
            messages: request_messages,
            overrides,
            decisions,
        } = request;
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

        // Load existing thread messages
        let mut messages = if let Some(ref ts) = self.storage {
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
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
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
                RunRequest {
                    agent_id: Some("agent".into()),
                    thread_id: "thread-ovr".into(),
                    messages: vec![Message::user("hi")],
                    overrides: Some(override_req.clone()),
                    decisions: vec![],
                },
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
            },
            StreamResult {
                content: vec![ContentBlock::text("finished")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
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
                        RunRequest {
                            agent_id: Some("agent".into()),
                            thread_id: "thread-live".into(),
                            messages: vec![Message::user("go")],
                            overrides: None,
                            decisions: vec![],
                        },
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
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
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
                RunRequest {
                    agent_id: Some("agent".into()),
                    thread_id: "thread-tx".into(),
                    messages: vec![Message::user("hi")],
                    overrides: None,
                    decisions: vec![],
                },
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

        let msgs = store
            .load_messages("thread-tx")
            .await
            .expect("load messages")
            .expect("thread should exist");
        assert!(!msgs.is_empty());
    }
}
