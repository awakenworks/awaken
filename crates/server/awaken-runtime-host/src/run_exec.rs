//! Run-execution routing (R3/R4): drive a thread's activation on the native
//! ingress or, when the session selected an ACP runtime, on the ACP executor.
//!
//! Both commit through the thread's coordinator and return a `RunState`, so the
//! caller's `finish_step` projection is identical either way — the ACP brain is a
//! peer `RunExecutor`, not a parallel code path.

use std::sync::Arc;

use crate::host::{HostError, SessionCtx, SharedHost};
use awaken_agent_contract::agent::run::RunState;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{
    Error as ExecutionError, Result as ExecutionResult, RunAttemptExecutor, RunExecutor,
};
use awaken_runtime_contract::resolved::Backend;
use awaken_runtime_contract::resume::ResumeCommand;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

/// The one executor router owned by a Session. Backend identity comes only from
/// the immutable activation snapshot, so foreground, durable, recovery, and a
/// cold replacement worker make the same choice without a process-local route.
pub(crate) struct SessionAttemptExecutor {
    native: Arc<dyn RunAttemptExecutor>,
    acp: Option<Arc<dyn RunAttemptExecutor>>,
}

impl SessionAttemptExecutor {
    pub(crate) fn new(
        native: Arc<awaken_runtime::Runtime>,
        acp: Option<Arc<awaken_run_executor_acp::AcpRunExecutor>>,
    ) -> Self {
        let native: Arc<dyn RunAttemptExecutor> = native;
        let acp = acp.map(|executor| executor as Arc<dyn RunAttemptExecutor>);
        Self::from_executors(native, acp)
    }

    fn from_executors(
        native: Arc<dyn RunAttemptExecutor>,
        acp: Option<Arc<dyn RunAttemptExecutor>>,
    ) -> Self {
        Self { native, acp }
    }

    fn executor(&self, activation: &RunActivation) -> ExecutionResult<&dyn RunAttemptExecutor> {
        match Backend::from_ref(&activation.snapshot.resolved_spec.model_binding.backend_ref) {
            Backend::Native => Ok(self.native.as_ref()),
            Backend::Acp { .. } => self.acp.as_deref().ok_or_else(|| {
                ExecutionError::Execution(
                    "Run snapshot selects ACP but this worker has no ACP executor".to_string(),
                )
            }),
            Backend::Remote { endpoint } => Err(ExecutionError::Execution(format!(
                "Run snapshot selects remote Agent {endpoint:?}, but A2A attempt routing is not installed"
            ))),
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
}

impl SharedHost {
    /// Execute `activation` for `thread`: the ACP executor when the session chose
    /// an ACP runtime, else the native ingress (direct / durable / superseding).
    ///
    /// `sink`, when set, receives the engine's best-effort live progress — only
    /// the in-process direct path wires it (the durable/ACP paths run elsewhere
    /// and simply omit live events, degrading to the committed projection).
    pub(crate) async fn execute_activation(
        &self,
        ctx: &Arc<SessionCtx>,
        thread: &str,
        mut activation: RunActivation,
        supersede: bool,
        sink: Option<Arc<dyn StreamSink>>,
    ) -> Result<RunState, HostError> {
        // Resolve the Session-level runtime selection into the activation BEFORE
        // delivery. From here onward direct and durable ingress share the same
        // snapshot-pinned executor router; ACP never bypasses enqueue/fencing.
        if let Some(acp) = &self.acp
            && let Some(adapter) = acp.adapter_for(thread)
        {
            activation.snapshot.resolved_spec.model_binding.backend_ref = adapter;
        }
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
        cause: &'static str,
    }

    impl RecordingExecutor {
        fn new(cause: &'static str) -> Self {
            Self {
                executes: AtomicUsize::new(0),
                resumes: AtomicUsize::new(0),
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
        let router = SessionAttemptExecutor::from_executors(
            native.clone(),
            Some(acp.clone() as Arc<dyn RunAttemptExecutor>),
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
    }

    #[tokio::test]
    async fn unavailable_snapshot_backend_fails_closed() {
        let native = Arc::new(RecordingExecutor::new("native"));
        let router = SessionAttemptExecutor::from_executors(native, None);

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
