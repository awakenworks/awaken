//! Run-execution routing (R3/R4): drive a thread's activation on the Native,
//! ACP, or A2A executor selected by its immutable snapshot.
//!
//! Both commit through the thread's coordinator and return a `RunState`, so the
//! caller's `finish_step` projection is identical either way — ACP and A2A brains
//! are peer `RunAttemptExecutor` implementations, not parallel lifecycle paths.

use std::sync::Arc;

use crate::host::{HostError, SessionCtx, SharedHost};
use awaken_agent_contract::agent::run::RunState;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{
    AttemptExecutorRegistry, Error as ExecutionError, Result as ExecutionResult,
    RunAttemptExecutor, RunExecutor,
};
use awaken_runtime_contract::resolved::Backend;
use awaken_runtime_contract::resume::ResumeCommand;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

static LOCAL_ATTEMPT_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct LocalAttemptAuthority {
    session: std::sync::Weak<SessionCtx>,
    run_id: awaken_agent_contract::agent::run::Id,
    bindings: Vec<awaken_runtime_contract::AttemptCredentialBinding>,
}

impl LocalAttemptAuthority {
    fn is_current(&self) -> bool {
        self.session.upgrade().is_some_and(|session| {
            session
                .active_run
                .lock()
                .expect("active run mutex poisoned")
                .as_ref()
                == Some(&self.run_id)
        })
    }
}

#[async_trait::async_trait]
impl awaken_runtime_contract::AttemptOwnershipVerifier for LocalAttemptAuthority {
    async fn verify_current(&self) -> Result<(), awaken_runtime_contract::AttemptOwnershipError> {
        self.is_current()
            .then_some(())
            .ok_or(awaken_runtime_contract::AttemptOwnershipError::Lost)
    }
}

#[async_trait::async_trait]
impl awaken_runtime_contract::CredentialRealizationRecorder for LocalAttemptAuthority {
    async fn record(
        &self,
        receipt: awaken_runtime_contract::CredentialRealizationReceipt,
    ) -> Result<(), awaken_runtime_contract::CredentialRealizationRecordError> {
        if !self.is_current() {
            return Err(awaken_runtime_contract::CredentialRealizationRecordError(
                "process-local attempt lost ownership before receipt".into(),
            ));
        }
        awaken_runtime_contract::verify_credential_realization_receipt(&self.bindings, &receipt)
            .map_err(|error| {
                awaken_runtime_contract::CredentialRealizationRecordError(error.to_string())
            })
    }
}

struct ActivationOptions {
    supersede: bool,
    sink: Option<Arc<dyn StreamSink>>,
    cancellation_mirror:
        Option<Arc<std::sync::Mutex<Option<awaken_runtime_contract::CancellationToken>>>>,
}

/// The one executor router owned by a Session. Backend identity comes only from
/// the immutable activation snapshot, so foreground, durable, recovery, and a
/// cold replacement worker make the same choice without a process-local route.
pub(crate) struct SessionAttemptExecutor {
    registry: AttemptExecutorRegistry,
}

/// Host composition adapter for the ordinary `RunExecutor` port. It binds one
/// Thread's backend routing, durable ingress, and live-context construction;
/// Runtime extensions still submit an ordinary `RunActivation` and remain
/// independent of `SharedHost`.
pub(crate) struct BoundRunExecutor<'a> {
    host: &'a SharedHost,
    ctx: Arc<SessionCtx>,
    supersede: bool,
    sink: Option<Arc<dyn StreamSink>>,
    cancellation_mirror:
        Option<Arc<std::sync::Mutex<Option<awaken_runtime_contract::CancellationToken>>>>,
}

impl<'a> BoundRunExecutor<'a> {
    pub(crate) fn new(host: &'a SharedHost, ctx: Arc<SessionCtx>) -> Self {
        Self {
            host,
            ctx,
            supersede: false,
            sink: None,
            cancellation_mirror: None,
        }
    }

    pub(crate) fn with_supersede(mut self, supersede: bool) -> Self {
        self.supersede = supersede;
        self
    }

    pub(crate) fn with_stream_sink(mut self, sink: Option<Arc<dyn StreamSink>>) -> Self {
        self.sink = sink;
        self
    }

    pub(crate) fn with_cancellation_mirror(
        mut self,
        mirror: Arc<std::sync::Mutex<Option<awaken_runtime_contract::CancellationToken>>>,
    ) -> Self {
        self.cancellation_mirror = Some(mirror);
        self
    }
}

#[async_trait::async_trait]
impl RunExecutor for BoundRunExecutor<'_> {
    async fn execute(
        &self,
        activation: RunActivation,
        _context: RuntimeRunContext,
    ) -> ExecutionResult<RunState> {
        if activation.thread_id != self.ctx.thread_id {
            return Err(ExecutionError::Execution(
                "Run activation does not match its bound Thread".to_string(),
            ));
        }
        let run_id = activation.run_id.clone();
        *self
            .ctx
            .active_run
            .lock()
            .expect("active run mutex poisoned") = Some(run_id.clone());
        let result = self
            .host
            .execute_activation(
                &self.ctx,
                activation,
                ActivationOptions {
                    supersede: self.supersede,
                    sink: self.sink.clone(),
                    cancellation_mirror: self.cancellation_mirror.clone(),
                },
            )
            .await
            .map_err(|error| ExecutionError::Execution(error.to_string()));
        let mut active_run = self
            .ctx
            .active_run
            .lock()
            .expect("active run mutex poisoned");
        if active_run.as_ref() == Some(&run_id) {
            *active_run = None;
        }
        result
    }
}

impl SessionAttemptExecutor {
    pub(crate) fn new(
        native: Arc<awaken_runtime::Runtime>,
        acp: Option<Arc<awaken_run_executor_acp::AcpRunExecutor>>,
        remote: Option<Arc<dyn RunAttemptExecutor>>,
        worker: &awaken_runtime_contract::resolved::ResolvedSpec,
        grader: Option<&awaken_runtime_contract::resolved::ResolvedSpec>,
    ) -> Self {
        let native: Arc<dyn RunAttemptExecutor> = native;
        let acp = acp.map(|executor| executor as Arc<dyn RunAttemptExecutor>);
        let mut snapshots = vec![worker];
        snapshots.extend(grader);
        Self::from_executors(native, acp, remote, &snapshots)
    }

    pub(crate) fn from_executors(
        native: Arc<dyn RunAttemptExecutor>,
        acp: Option<Arc<dyn RunAttemptExecutor>>,
        a2a: Option<Arc<dyn RunAttemptExecutor>>,
        resolved: &[&awaken_runtime_contract::resolved::ResolvedSpec],
    ) -> Self {
        let mut registry = AttemptExecutorRegistry::new();
        registry
            .register_native(native)
            .expect("fresh Session registry has one native slot");
        for binding in resolved
            .iter()
            .flat_map(|resolved| resolved.candidate_bindings())
        {
            let executor = match Backend::from_ref(&binding.backend_ref) {
                Backend::Native => continue,
                Backend::Acp { .. } => acp.clone(),
                Backend::Remote { .. } => a2a.clone(),
            };
            if let Some(executor) = executor
                && !registry.supports(&binding.backend_ref)
            {
                registry
                    .register(binding.backend_ref.clone(), executor)
                    .expect("resolved backend_ref is an exact ACP/A2A route");
            }
        }
        Self { registry }
    }
}

#[async_trait::async_trait]
impl RunExecutor for SessionAttemptExecutor {
    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> ExecutionResult<RunState> {
        self.registry.execute(activation, context).await
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
        self.registry.resume(activation, command, context).await
    }

    async fn cancel(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> ExecutionResult<()> {
        self.registry.cancel(activation, context).await
    }
}

impl SharedHost {
    /// Build the exact per-attempt runtime context shared by a fresh direct run and
    /// an awaiting direct run resumed later. Brokered grants and expiring provider
    /// credentials are attempt-scoped, so a resume must rematerialize from the
    /// immutable activation instead of falling back to the host's placeholder model.
    pub(crate) async fn native_attempt_context(
        &self,
        ctx: &Arc<SessionCtx>,
        activation: &RunActivation,
    ) -> Result<RuntimeRunContext, HostError> {
        let mut context = ctx
            .context_for(activation)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        let candidates = activation
            .snapshot
            .resolved_spec
            .execution_candidates(activation.model_ref_override.as_deref());
        let holder = self.inference_plaintext_holder(activation)?;
        let epoch = LOCAL_ATTEMPT_EPOCH
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .max(1);
        let now_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();
        let mut bindings = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for candidate in &candidates {
            let backend = Backend::from_ref(&candidate.binding.backend_ref);
            // Each route is admitted only by the resolver/provider that will
            // execute that exact candidate. Unioning evidence across fallback
            // candidates would let one ACP route authorize another route.
            let installed = match &backend {
                Backend::Native => self.inference_routing.credential_realization_capabilities(),
                Backend::Acp { .. } => self
                    .acp
                    .as_ref()
                    .ok_or_else(|| HostError::bad_request("ACP backend is not installed"))?
                    .credential_realization_capabilities(&backend)
                    .map_err(HostError::bad_request)?,
                Backend::Remote { .. } => self.remote_credential_realization.clone(),
            };
            let compiled = awaken_runtime_contract::compile_candidate_credential_bindings(
                &[*candidate],
                holder.as_ref(),
                &installed,
                epoch,
                now_unix_ms,
            )
            .map_err(|error| HostError::bad_request(error.to_string()))?;
            for binding in compiled {
                if !seen.insert(binding.candidate_fingerprint.clone()) {
                    return Err(HostError::bad_request(
                        "published model candidate is duplicated in the selected fallback set",
                    ));
                }
                bindings.push(binding);
            }
        }
        // Local attempt authority cause graph: C1 the Session still owns this
        // run; C2 a candidate needs local credential material. E1 expose an
        // ownership fence to every side-effecting executor (including brokered
        // grants); E2 additionally install credential bindings/receipts.
        // Decision table: C1=N -> fail at verifier; C1=Y,C2=N -> E1 only;
        // C1=Y,C2=Y -> E1+E2. A resumed attempt applies the same table again.
        let authority = Arc::new(LocalAttemptAuthority {
            session: Arc::downgrade(ctx),
            run_id: activation.run_id.clone(),
            bindings: bindings.clone(),
        });
        context = context.with_ownership(authority.clone());
        if !bindings.is_empty() {
            context = context.with_credential_realization(
                awaken_runtime_contract::AttemptCredentialRealization::new(bindings, authority),
            );
        }
        // Route this attempt's inference through the run's effective model,
        // resolved through the host's InferenceExecutorMaterializer. `None` leaves
        // the runtime's bound host-default executor.
        if let Some(executor) = self
            .inference_routing
            .executor_for_activation(activation, &context)
            .map_err(HostError::bad_request)?
        {
            context = context.with_model_executor(executor);
        }
        Ok(context)
    }

    /// Execute `activation`: the ACP executor when the session chose
    /// an ACP runtime, else the native ingress (direct / durable / superseding).
    ///
    /// `sink`, when set, receives the engine's best-effort live progress — only
    /// the in-process direct path wires it (the durable/ACP paths run elsewhere
    /// and simply omit live events, degrading to the committed projection).
    async fn execute_activation(
        &self,
        ctx: &Arc<SessionCtx>,
        activation: RunActivation,
        options: ActivationOptions,
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
        if options.supersede {
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
            let mut context = self
                .native_attempt_context(ctx, &activation)
                .await?
                .with_live_inbox(ctx.open_live_inbox());
            if let (Some(mirror), Some(token)) = (
                options.cancellation_mirror.as_ref(),
                context.cancellation.clone(),
            ) {
                *mirror.lock().expect("cancel mirror mutex poisoned") = Some(token);
            }
            if let Some(sink) = options.sink {
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
                    model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                        ModelBinding::new("provider", "model", backend_ref),
                    ),
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
            data_subject_id: None,
            tool_capability_narrowing: Default::default(),
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

    fn resolved_with(backend_refs: &[&str]) -> awaken_runtime_contract::resolved::ResolvedSpec {
        let mut resolved = activation(backend_refs[0]).snapshot.resolved_spec;
        for backend_ref in &backend_refs[1..] {
            let mut candidate = resolved.model_binding.clone();
            candidate.binding.backend_ref = (*backend_ref).to_string();
            resolved.model_candidates.push(candidate);
        }
        resolved
    }

    #[tokio::test]
    async fn session_backend_routes_fresh_and_resume_attempts() {
        // Cause graph: C1=backend is named by the Worker snapshot,
        // C2=backend is named only by the frozen Outcome Grader snapshot,
        // C3=matching topology adapter is installed. One exact registry consumes
        // both authorities; it never substitutes Native for a missing adapter.
        //
        // | Rule | C1 | C2 | C3 | result |
        // | R1 | T | F | T | route Worker backend |
        // | R2 | F | T | T | route Grader backend |
        // | R3 | T/F | T/F | F | fail closed (covered below) |
        let native = Arc::new(RecordingExecutor::new("native"));
        let acp = Arc::new(RecordingExecutor::new("acp"));
        let a2a = Arc::new(RecordingExecutor::new("a2a"));
        let worker = resolved_with(&["awaken", "a2a:https://agent.example"]);
        let grader = resolved_with(&["acp:claude"]);
        let router = SessionAttemptExecutor::from_executors(
            native.clone(),
            Some(acp.clone() as Arc<dyn RunAttemptExecutor>),
            Some(a2a.clone() as Arc<dyn RunAttemptExecutor>),
            &[&worker, &grader],
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
        let resolved = resolved_with(&["acp:claude", "a2a:https://agent.example"]);
        let router = SessionAttemptExecutor::from_executors(native, None, None, &[&resolved]);

        let error = router
            .execute(activation("acp:claude"), RuntimeRunContext::new())
            .await
            .expect_err("missing ACP backend must not fall back to Native");
        assert!(error.to_string().contains("acp:claude"));

        let error = router
            .execute(
                activation("a2a:https://agent.example"),
                RuntimeRunContext::new(),
            )
            .await
            .expect_err("unwired A2A backend must fail closed");
        assert!(error.to_string().contains("a2a:https://agent.example"));
    }
}
