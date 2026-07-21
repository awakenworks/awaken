//! Run-execution routing (R3/R4): drive a thread's activation on the Native,
//! ACP, or A2A executor selected by its immutable snapshot.
//!
//! Both commit through the thread's coordinator and return a `RunState`, so the
//! caller's `finish_step` projection is identical either way — ACP and A2A brains
//! are peer `RunAttemptExecutor` implementations, not parallel lifecycle paths.

use std::sync::Arc;

use crate::host::{HostError, SessionCtx, SharedHost};
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{
    Error as ExecutionError, Result as ExecutionResult, RunAttemptExecutor, RunExecutor,
};
use awaken_runtime_contract::resolved::Backend;
use awaken_runtime_contract::resume::ResumeCommand;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum RunPurpose {
    UserTurn,
    OutcomeWorker,
    OutcomeGrader,
    Memory,
    Compact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum Continuity {
    Continue,
    Fresh,
}

pub(crate) struct SnapshotRunRequest {
    pub(crate) run_id: Option<RunId>,
    pub(crate) thread_id: ThreadId,
    pub(crate) snapshot: ExecutableAgentSnapshot,
    pub(crate) input: Vec<Message>,
    pub(crate) continuity: Continuity,
    pub(crate) purpose: RunPurpose,
    pub(crate) model_ref_override: Option<String>,
    pub(crate) supersede: bool,
    pub(crate) sink: Option<Arc<dyn StreamSink>>,
}

#[derive(Debug)]
pub(crate) struct SnapshotRunResult {
    pub(crate) run_id: RunId,
    pub(crate) state: RunState,
    pub(crate) new_messages: Vec<Message>,
    pub(crate) before: usize,
}

/// The one executor router owned by a Session. Backend identity comes only from
/// the immutable activation snapshot, so foreground, durable, recovery, and a
/// cold replacement worker make the same choice without a process-local route.
pub(crate) struct SessionAttemptExecutor {
    native: Arc<dyn RunAttemptExecutor>,
    acp: Option<Arc<dyn RunAttemptExecutor>>,
    a2a: Option<Arc<dyn RunAttemptExecutor>>,
}

impl SessionAttemptExecutor {
    pub(crate) fn new(
        native: Arc<awaken_runtime::Runtime>,
        acp: Option<Arc<awaken_run_executor_acp::AcpRunExecutor>>,
        remote: Option<Arc<dyn RunAttemptExecutor>>,
    ) -> Self {
        let native: Arc<dyn RunAttemptExecutor> = native;
        let acp = acp.map(|executor| executor as Arc<dyn RunAttemptExecutor>);
        Self::from_executors(native, acp, remote)
    }

    fn from_executors(
        native: Arc<dyn RunAttemptExecutor>,
        acp: Option<Arc<dyn RunAttemptExecutor>>,
        a2a: Option<Arc<dyn RunAttemptExecutor>>,
    ) -> Self {
        Self { native, acp, a2a }
    }

    fn executor(&self, activation: &RunActivation) -> ExecutionResult<&dyn RunAttemptExecutor> {
        match Backend::from_ref(&activation.snapshot.resolved_spec.model_binding.backend_ref) {
            Backend::Native => Ok(self.native.as_ref()),
            Backend::Acp { .. } => self.acp.as_deref().ok_or_else(|| {
                ExecutionError::Execution(
                    "Run snapshot selects ACP but this worker has no ACP executor".to_string(),
                )
            }),
            Backend::Remote { endpoint } => self.a2a.as_deref().ok_or_else(|| {
                ExecutionError::Execution(format!(
                    "Run snapshot selects remote Agent {endpoint:?}, but A2A attempt routing is not installed"
                ))
            }),
        }
    }
}

#[async_trait::async_trait]
impl RunExecutor for SessionAttemptExecutor {
    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> ExecutionResult<RunState> {
        self.executor(&activation)?
            .execute(activation, context)
            .await
    }
}

#[async_trait::async_trait]
impl RunAttemptExecutor for SessionAttemptExecutor {
    async fn resume(
        &self,
        activation: RunActivation,
        command: ResumeCommand,
        context: RuntimeRunContext,
    ) -> ExecutionResult<RunState> {
        self.executor(&activation)?
            .resume(activation, command, context)
            .await
    }

    async fn cancel(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> ExecutionResult<()> {
        self.executor(&activation)?
            .cancel(activation, context)
            .await
    }
}

impl SharedHost {
    pub(crate) async fn execute_snapshot(
        &self,
        ctx: &Arc<SessionCtx>,
        request: SnapshotRunRequest,
    ) -> Result<SnapshotRunResult, HostError> {
        if request.thread_id != ctx.thread_id {
            return Err(HostError::bad_request(
                "snapshot Run thread does not match its Session context",
            ));
        }
        self.enforce_purpose_policy(request.purpose, &request.snapshot)?;
        let _ = self.snapshot_capabilities(&request.snapshot)?;

        let before = ctx.commit.committed_messages(&ctx.thread_id).len();
        let (generated_run_id, mut activation) = ctx.runtime.prepare(
            &request.snapshot,
            request.thread_id.0.clone(),
            request.input,
        );
        let run_id = request.run_id.unwrap_or_else(|| {
            if ctx.durable {
                RunId(format!(
                    "run-{}-{}",
                    crate::host::now_ms(),
                    crate::host::BASE_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                ))
            } else {
                generated_run_id
            }
        });
        activation.run_id = run_id.clone();
        activation.model_ref_override = request.model_ref_override;
        match request.continuity {
            Continuity::Continue | Continuity::Fresh => {}
        }

        *ctx.active_run.lock().expect("active run mutex poisoned") = Some(run_id.clone());
        let state = self
            .execute_activation(ctx, activation, request.supersede, request.sink)
            .await;
        {
            let mut active_run = ctx.active_run.lock().expect("active run mutex poisoned");
            if active_run.as_ref() == Some(&run_id) {
                *active_run = None;
            }
        }
        let state = state?;
        let all = ctx.commit.committed_messages(&ctx.thread_id);
        Ok(SnapshotRunResult {
            run_id,
            state,
            new_messages: all[before.min(all.len())..].to_vec(),
            before,
        })
    }

    pub(crate) fn snapshot_capabilities(
        &self,
        snapshot: &ExecutableAgentSnapshot,
    ) -> Result<awaken_runtime_contract::execution::ExecutorCapabilities, HostError> {
        match Backend::from_ref(&snapshot.resolved_spec.model_binding.backend_ref) {
            Backend::Native => Ok(awaken_runtime_contract::execution::ExecutorCapabilities::NATIVE),
            Backend::Acp { .. } => self
                .acp
                .as_ref()
                .map(
                    |_| awaken_runtime_contract::execution::ExecutorCapabilities {
                        cancellation: awaken_runtime_contract::execution::Cancellation::RemoteAbort,
                        wait: awaken_runtime_contract::execution::Wait::Auth,
                    },
                )
                .ok_or_else(|| HostError::bad_request("ACP backend is not configured")),
            Backend::Remote { .. } => Err(HostError::bad_request(
                "root A2A snapshot execution is not configured on this host",
            )),
        }
    }

    fn enforce_purpose_policy(
        &self,
        purpose: RunPurpose,
        snapshot: &ExecutableAgentSnapshot,
    ) -> Result<(), HostError> {
        if purpose == RunPurpose::OutcomeGrader
            && !snapshot.resolved_spec.tool_descriptors.is_empty()
        {
            return Err(HostError::bad_request(
                "Outcome Grader snapshots must not declare tools",
            ));
        }
        Ok(())
    }

    /// Execute `activation`: the ACP executor when the session chose
    /// an ACP runtime, else the native ingress (direct / durable / superseding).
    ///
    /// `sink`, when set, receives the engine's best-effort live progress — only
    /// the in-process direct path wires it (the durable/ACP paths run elsewhere
    /// and simply omit live events, degrading to the committed projection).
    pub(crate) async fn execute_activation(
        &self,
        ctx: &Arc<SessionCtx>,
        activation: RunActivation,
        supersede: bool,
        sink: Option<Arc<dyn StreamSink>>,
    ) -> Result<RunState, HostError> {
        // Runtime selection is already pinned in the Session snapshot before the
        // SessionCtx and ingress are built. Every first attempt, resume, durable
        // claim and replacement therefore routes through this exact same binding.
        // The run's effective model — its per-run override (R5), else the model its
        // snapshot binding names. Resolved to an executor per attempt at this seam
        // (the direct path here; the durable path re-resolves on the claiming worker),
        // so the runtime only ever receives an executor, never a model identity to
        // look up — the provider owns how the model is reached (local credentials or a
        // gateway offering).
        if supersede {
            // Durable + superseding: enqueue (marking prior pending superseded) and
            // let the process pool drive it on this session's worker (O2).
            self.submit_durable_foreground(ctx, activation, true).await
        } else if ctx.durable {
            // Durable: enqueue and await the pool driving it to a settled state. The
            // session's own worker must not claim (it would grab foreign threads'
            // runs on the shared queue); the pool is the sole claimer.
            self.submit_durable_foreground(ctx, activation, false).await
        } else {
            // Native direct turn: the only path whose engine drains a live
            // inbox in-process, so it is the only path that opens one. The
            // inbox closes when the attempt returns — success or error — and
            // unconsumed messages carry over to the thread's next attempt.
            let mut context = ctx
                .context_for(&activation)
                .await
                .map_err(|e| HostError::internal(e.to_string()))?
                .with_live_inbox(ctx.open_live_inbox());
            // Route this attempt's inference through the run's effective model,
            // resolved through the host's InferenceExecutorMaterializer. `None` leaves the
            // runtime's bound (host default) executor — a single-model deployment is
            // unaffected.
            if let Some(exec) = self
                .inference_routing
                .executor_for_activation(&activation)
                .map_err(HostError::bad_request)?
            {
                context = context.with_model_executor(exec);
            }
            if let Some(sink) = sink {
                context = context.with_stream_sink(sink);
            }
            let result = ctx
                .ingress
                .start(activation, context)
                .await
                .map_err(|e| HostError::internal(e.to_string()));
            ctx.close_live_inbox();
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::resume::ResumeResult;
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RecordingExecutor {
        executes: AtomicUsize,
        resumes: AtomicUsize,
        cancels: AtomicUsize,
        cause: &'static str,
    }

    impl RecordingExecutor {
        fn new(cause: &'static str) -> Self {
            Self {
                executes: AtomicUsize::new(0),
                resumes: AtomicUsize::new(0),
                cancels: AtomicUsize::new(0),
                cause,
            }
        }

        fn state(&self) -> RunState {
            RunState::Ended(EndCause::Stopped(self.cause.to_string()))
        }
    }

    #[async_trait::async_trait]
    impl RunExecutor for RecordingExecutor {
        async fn execute(
            &self,
            _activation: RunActivation,
            _context: RuntimeRunContext,
        ) -> ExecutionResult<RunState> {
            self.executes.fetch_add(1, Ordering::SeqCst);
            Ok(self.state())
        }
    }

    #[async_trait::async_trait]
    impl RunAttemptExecutor for RecordingExecutor {
        async fn resume(
            &self,
            _activation: RunActivation,
            _command: ResumeCommand,
            _context: RuntimeRunContext,
        ) -> ExecutionResult<RunState> {
            self.resumes.fetch_add(1, Ordering::SeqCst);
            Ok(self.state())
        }

        async fn cancel(
            &self,
            _activation: RunActivation,
            _context: RuntimeRunContext,
        ) -> ExecutionResult<()> {
            self.cancels.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn activation(backend_ref: &str) -> RunActivation {
        RunActivation {
            run_id: RunId("run-router".into()),
            thread_id: ThreadId("thread-router".into()),
            snapshot: ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("snapshot-router".into()),
                metadata: Default::default(),
                root_agent_id: AgentId("agent-router".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("catalog-router".into()),
                    instructions: String::new(),
                    max_steps: 1,
                    delegation_limits: Default::default(),
                    model_binding: ModelBinding::new("provider", "model", backend_ref),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("snapshot-fingerprint".into()),
            },
            input: vec![Message::text(MessageId("input".into()), Role::User, "go")],
            delegation_origin: None,
            model_ref_override: None,
        }
    }

    fn resume(activation: &RunActivation) -> ResumeCommand {
        ResumeCommand {
            correlation_id: "correlation".into(),
            run_id: activation.run_id.clone(),
            thread_id: activation.thread_id.clone(),
            snapshot_id: activation.snapshot.id.clone(),
            catalog_fingerprint: activation
                .snapshot
                .resolved_spec
                .catalog_fingerprint
                .clone(),
            result: ResumeResult::Input("continue".into()),
            now_ms: 1,
        }
    }

    #[tokio::test]
    async fn snapshot_backend_routes_fresh_and_resume_attempts() {
        let native = Arc::new(RecordingExecutor::new("native"));
        let acp = Arc::new(RecordingExecutor::new("acp"));
        let a2a = Arc::new(RecordingExecutor::new("a2a"));
        let router = SessionAttemptExecutor::from_executors(
            native.clone(),
            Some(acp.clone() as Arc<dyn RunAttemptExecutor>),
            Some(a2a.clone() as Arc<dyn RunAttemptExecutor>),
        );

        let native_activation = activation("awaken");
        assert_eq!(
            router
                .execute(native_activation, RuntimeRunContext::new())
                .await
                .unwrap(),
            RunState::Ended(EndCause::Stopped("native".into()))
        );
        let acp_activation = activation("acp:claude");
        assert_eq!(
            router
                .resume(
                    acp_activation.clone(),
                    resume(&acp_activation),
                    RuntimeRunContext::new(),
                )
                .await
                .unwrap(),
            RunState::Ended(EndCause::Stopped("acp".into()))
        );

        assert_eq!(native.executes.load(Ordering::SeqCst), 1);
        assert_eq!(native.resumes.load(Ordering::SeqCst), 0);
        assert_eq!(acp.executes.load(Ordering::SeqCst), 0);
        assert_eq!(acp.resumes.load(Ordering::SeqCst), 1);
        assert_eq!(
            router
                .execute(
                    activation("a2a:https://agent.example"),
                    RuntimeRunContext::new(),
                )
                .await
                .unwrap(),
            RunState::Ended(EndCause::Stopped("a2a".into()))
        );
        assert_eq!(a2a.executes.load(Ordering::SeqCst), 1);

        router
            .cancel(
                activation("a2a:https://agent.example"),
                RuntimeRunContext::new(),
            )
            .await
            .unwrap();
        assert_eq!(a2a.cancels.load(Ordering::SeqCst), 1);
        assert_eq!(native.cancels.load(Ordering::SeqCst), 0);
        assert_eq!(acp.cancels.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unavailable_snapshot_backend_fails_closed() {
        let native = Arc::new(RecordingExecutor::new("native"));
        let router = SessionAttemptExecutor::from_executors(native, None, None);

        let error = router
            .execute(activation("acp:claude"), RuntimeRunContext::new())
            .await
            .expect_err("missing ACP backend must not fall back to Native");
        assert!(error.to_string().contains("no ACP executor"));

        let error = router
            .execute(
                activation("a2a:https://agent.example"),
                RuntimeRunContext::new(),
            )
            .await
            .expect_err("unwired A2A backend must fail closed");
        assert!(error.to_string().contains("A2A attempt routing"));
    }
}
