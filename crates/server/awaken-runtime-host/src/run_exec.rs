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
    associate_prepared_session: bool,
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

/// Persist Sandbox outputs after every completed attempt, independently of
/// whether delivery was direct, durable, or recovered by another claim.
pub(crate) struct ArtifactHarvestAttemptExecutor {
    inner: Arc<dyn RunAttemptExecutor>,
    harvester: crate::provisioning::ArtifactHarvester,
}

impl ArtifactHarvestAttemptExecutor {
    pub(crate) fn new(
        inner: Arc<dyn RunAttemptExecutor>,
        harvester: crate::provisioning::ArtifactHarvester,
    ) -> Self {
        Self { inner, harvester }
    }

    async fn finish<T>(
        &self,
        thread: &str,
        claim: Option<awaken_run_ingress::RunClaim>,
        result: ExecutionResult<T>,
    ) -> ExecutionResult<T> {
        match result {
            Ok(value) => {
                self.harvester
                    .harvest_with_claim(thread, claim)
                    .await
                    .map_err(|error| ExecutionError::Execution(error.to_string()))?;
                Ok(value)
            }
            Err(error) => {
                // Preserve the execution failure while still retaining any bytes
                // written before the failed model/tool step.
                let _ = self.harvester.harvest_with_claim(thread, claim).await;
                Err(error)
            }
        }
    }
}

#[async_trait::async_trait]
impl RunExecutor for ArtifactHarvestAttemptExecutor {
    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> ExecutionResult<RunState> {
        let thread = activation.thread_id.0.clone();
        // Capture before execution: a stale attempt must never borrow a newer
        // replacement claim for its post-attempt Resource effects.
        let claim = self.harvester.current_claim(&thread);
        let result = self.inner.execute(activation, context).await;
        self.finish(&thread, claim, result).await
    }
}

#[async_trait::async_trait]
impl RunAttemptExecutor for ArtifactHarvestAttemptExecutor {
    async fn resume(
        &self,
        activation: RunActivation,
        command: ResumeCommand,
        context: RuntimeRunContext,
    ) -> ExecutionResult<RunState> {
        let thread = activation.thread_id.0.clone();
        let claim = self.harvester.current_claim(&thread);
        let result = self.inner.resume(activation, command, context).await;
        self.finish(&thread, claim, result).await
    }

    async fn cancel(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> ExecutionResult<()> {
        self.inner.cancel(activation, context).await
    }
}

/// The one privacy-context decorator shared by direct and durable attempts.
///
/// Delivery topology may replace commit, cancellation, and ownership wiring,
/// but it must not replace the request's subject attribution or the deployment
/// capture policy. Keeping this at the `RunAttemptExecutor` boundary means a
/// queued/recovered attempt and an inline attempt resolve consent identically.
pub(crate) struct CaptureContextAttemptExecutor {
    inner: Arc<dyn RunAttemptExecutor>,
    decision: awaken_runtime_contract::CaptureDecision,
    sink: Option<Arc<dyn awaken_runtime_contract::CaptureSink>>,
    consent: Arc<dyn awaken_runtime_contract::DataSubjectConsentSource>,
}

impl CaptureContextAttemptExecutor {
    pub(crate) fn new(
        inner: Arc<dyn RunAttemptExecutor>,
        decision: awaken_runtime_contract::CaptureDecision,
        sink: Option<Arc<dyn awaken_runtime_contract::CaptureSink>>,
        consent: Arc<dyn awaken_runtime_contract::DataSubjectConsentSource>,
    ) -> Self {
        Self {
            inner,
            decision,
            sink,
            consent,
        }
    }

    async fn context_for(
        &self,
        activation: &RunActivation,
        mut context: RuntimeRunContext,
    ) -> RuntimeRunContext {
        context = context.with_capture(self.decision.clone());
        if let Some(subject) = activation.data_subject_id.clone() {
            let consent = self
                .consent
                .consent_ceiling(&subject, awaken_runtime_contract::Purpose::TelemetryContent)
                .await;
            context.capture.decision.level = context.capture.decision.level.meet(consent);
            context = match self.sink.clone() {
                Some(sink) => context.with_capture_sink(subject, sink),
                None => context.with_data_subject(subject),
            };
        }
        context
    }
}

#[async_trait::async_trait]
impl RunExecutor for CaptureContextAttemptExecutor {
    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> ExecutionResult<RunState> {
        let context = self.context_for(&activation, context).await;
        self.inner.execute(activation, context).await
    }
}

#[async_trait::async_trait]
impl RunAttemptExecutor for CaptureContextAttemptExecutor {
    async fn resume(
        &self,
        activation: RunActivation,
        command: ResumeCommand,
        context: RuntimeRunContext,
    ) -> ExecutionResult<RunState> {
        let context = self.context_for(&activation, context).await;
        self.inner.resume(activation, command, context).await
    }

    async fn cancel(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> ExecutionResult<()> {
        self.inner.cancel(activation, context).await
    }
}

/// Host startup adapter for the ordinary `RunExecutor` port. It binds one
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
    release_active_on_return: bool,
    associate_prepared_session: bool,
}

impl<'a> BoundRunExecutor<'a> {
    pub(crate) fn new(host: &'a SharedHost, ctx: Arc<SessionCtx>) -> Self {
        Self {
            host,
            ctx,
            supersede: false,
            sink: None,
            cancellation_mirror: None,
            release_active_on_return: true,
            associate_prepared_session: true,
        }
    }

    /// Keep an extension-owned ordinary Run on its shared Thread without
    /// manufacturing a second Session-root admission. Frozen execution inputs
    /// still flow through the one dispatch decorator.
    pub(crate) fn for_thread_extension(mut self) -> Self {
        self.associate_prepared_session = false;
        self
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

    /// Keep the foreground identity installed until the caller has projected the
    /// committed terminal/awaiting step. Terminal quiescence treats an empty slot
    /// as proof that no parent delegation commit remains in flight.
    pub(crate) fn retain_active_until_settled(mut self) -> Self {
        self.release_active_on_return = false;
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
                    associate_prepared_session: self.associate_prepared_session,
                    sink: self.sink.clone(),
                    cancellation_mirror: self.cancellation_mirror.clone(),
                },
            )
            .await
            .map_err(|error| ExecutionError::Execution(error.to_string()));
        if self.release_active_on_return {
            let mut active_run = self
                .ctx
                .active_run
                .lock()
                .expect("active run mutex poisoned");
            if active_run.as_ref() == Some(&run_id) {
                *active_run = None;
            }
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
        for binding in resolved.iter().flat_map(|resolved| {
            resolved
                .attempt_candidates(None)
                .into_iter()
                .map(|candidate| candidate.binding())
        }) {
            let executor = match Backend::from_ref(&binding.backend_ref) {
                Backend::Native => continue,
                Backend::Acp(_) => acp.clone(),
                Backend::Remote(_) => a2a.clone(),
                Backend::Invalid(_) => continue,
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
        let mut context = ctx.context();
        let candidates = activation
            .snapshot
            .resolved_spec
            .attempt_candidates(activation.model_ref_override.as_deref());
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
            let backend = Backend::from_ref(&candidate.binding().backend_ref);
            // Each route is admitted only by the resolver/provider that will
            // execute that exact candidate. Unioning evidence across fallback
            // candidates would let one ACP route authorize another route.
            let installed = match &backend {
                Backend::Native => self.inference_routing.credential_realization_capabilities(),
                Backend::Acp(_) => self
                    .acp
                    .as_ref()
                    .ok_or_else(|| HostError::bad_request("ACP backend is not installed"))?
                    .credential_realization_capabilities(&backend)
                    .map_err(HostError::bad_request)?,
                Backend::Remote(_) => self.remote_credential_realization.clone(),
                Backend::Invalid(invalid) => {
                    return Err(HostError::bad_request(format!(
                        "invalid backend_ref {}",
                        invalid.as_str()
                    )));
                }
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
    /// `sink`, when set, receives the engine's best-effort live progress. Direct
    /// execution wires it into the attempt context; durable foreground execution
    /// registers it with the Host relay installed on the Session worker. A Worker
    /// in another process still degrades to the committed projection until the
    /// authenticated Worker transport carries the same neutral stream port.
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
            self.submit_durable_foreground(
                ctx,
                activation,
                true,
                options.sink,
                options.associate_prepared_session,
            )
            .await
        } else if ctx.durable {
            // Durable: enqueue and await the pool driving it to a settled state. The
            // session's own worker must not claim (it would grab foreign threads'
            // runs on the shared queue); the pool is the sole claimer.
            self.submit_durable_foreground(
                ctx,
                activation,
                false,
                options.sink,
                options.associate_prepared_session,
            )
            .await
        } else {
            // Native direct Run: the only path whose engine drains a live
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
    use awaken_run_ingress::{Clock as _, DispatchQueue as _};
    use awaken_runtime_contract::llm::{ChatRequest, ChatResponse};
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::resume::ResumeResult;
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct NoLlm;

    #[async_trait::async_trait]
    impl awaken_runtime_contract::llm::LlmExecutor for NoLlm {
        async fn infer(
            &self,
            _request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            unreachable!("attempt-boundary tests never call inference")
        }
    }

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

    struct FailingExecutor;

    struct RecordingArtifactPublisher {
        inner: Arc<dyn awaken_resource_contract::ArtifactPublisher<awaken_run_ingress::RunClaim>>,
        claims: Arc<std::sync::Mutex<Vec<Option<awaken_run_ingress::RunClaim>>>>,
        dispatch: Arc<awaken_run_ingress::AnyDispatchStore>,
    }

    struct ReplacingClaimExecutor {
        slots: crate::session_slot::SessionRuntimeSlots,
        thread: String,
        replacement: awaken_run_ingress::RunClaim,
        failure: Option<&'static str>,
    }

    impl ReplacingClaimExecutor {
        fn succeeding(
            slots: crate::session_slot::SessionRuntimeSlots,
            thread: &str,
            replacement: awaken_run_ingress::RunClaim,
        ) -> Self {
            Self {
                slots,
                thread: thread.into(),
                replacement,
                failure: None,
            }
        }

        fn failing(
            slots: crate::session_slot::SessionRuntimeSlots,
            thread: &str,
            replacement: awaken_run_ingress::RunClaim,
            failure: &'static str,
        ) -> Self {
            Self {
                slots,
                thread: thread.into(),
                replacement,
                failure: Some(failure),
            }
        }
    }

    fn assert_recorded_claims_since(
        claims: &std::sync::Mutex<Vec<Option<awaken_run_ingress::RunClaim>>>,
        from: usize,
        expected: Option<&awaken_run_ingress::RunClaim>,
        rule: &str,
    ) {
        let claims = claims.lock().expect("recorded claims mutex poisoned");
        assert!(claims.len() > from, "{rule}: no artifact was published");
        assert!(
            claims
                .iter()
                .skip(from)
                .all(|claim| claim.as_ref() == expected),
            "{rule}: every artifact must use the attempt-start claim snapshot"
        );
    }

    #[async_trait::async_trait]
    impl RunExecutor for ReplacingClaimExecutor {
        async fn execute(
            &self,
            _activation: RunActivation,
            _context: RuntimeRunContext,
        ) -> ExecutionResult<RunState> {
            self.slots.update(&self.thread, |slot| {
                slot.dispatch_claim = Some(self.replacement.clone());
            });
            match self.failure {
                Some(message) => Err(ExecutionError::Execution(message.into())),
                None => Ok(RunState::Ended(EndCause::Stopped("replaced".into()))),
            }
        }
    }

    #[async_trait::async_trait]
    impl RunAttemptExecutor for ReplacingClaimExecutor {
        async fn resume(
            &self,
            activation: RunActivation,
            _command: ResumeCommand,
            context: RuntimeRunContext,
        ) -> ExecutionResult<RunState> {
            self.execute(activation, context).await
        }

        async fn cancel(
            &self,
            _activation: RunActivation,
            _context: RuntimeRunContext,
        ) -> ExecutionResult<()> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl awaken_resource_contract::ArtifactPublisher<awaken_run_ingress::RunClaim>
        for RecordingArtifactPublisher
    {
        async fn publish(
            &self,
            publication: awaken_resource_contract::ArtifactPublication<
                awaken_run_ingress::RunClaim,
            >,
        ) -> Result<
            awaken_resource_contract::ArtifactPublicationReceipt,
            awaken_resource_contract::ArtifactPublicationError,
        > {
            let _guard = if let Some(claim) = publication.fence.as_ref() {
                Some(
                    awaken_run_ingress::DispatchQueue::lock_commit_epoch(
                        self.dispatch.as_ref(),
                        claim,
                    )
                    .await
                    .map_err(|error| {
                        awaken_resource_contract::ArtifactPublicationError::new(error.to_string())
                    })?
                    .ok_or_else(|| {
                        awaken_resource_contract::ArtifactPublicationError::new(
                            "artifact publication lost its local dispatch claim",
                        )
                    })?,
                )
            } else {
                None
            };
            self.claims
                .lock()
                .expect("recorded claims mutex poisoned")
                .push(publication.fence.clone());
            self.inner.publish(publication).await
        }
    }

    #[async_trait::async_trait]
    impl RunExecutor for FailingExecutor {
        async fn execute(
            &self,
            _activation: RunActivation,
            _context: RuntimeRunContext,
        ) -> ExecutionResult<RunState> {
            Err(ExecutionError::Execution("inner attempt failed".into()))
        }
    }

    #[async_trait::async_trait]
    impl RunAttemptExecutor for FailingExecutor {
        async fn resume(
            &self,
            _activation: RunActivation,
            _command: ResumeCommand,
            _context: RuntimeRunContext,
        ) -> ExecutionResult<RunState> {
            Err(ExecutionError::Execution("inner resume failed".into()))
        }

        async fn cancel(
            &self,
            _activation: RunActivation,
            _context: RuntimeRunContext,
        ) -> ExecutionResult<()> {
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
            operation_id: None,
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
            context_messages: Vec::new(),
            now_ms: 1,
        }
    }

    fn resolved_with(backend_refs: &[&str]) -> awaken_runtime_contract::resolved::ResolvedSpec {
        let mut resolved = activation(backend_refs[0]).snapshot.resolved_spec;
        for backend_ref in &backend_refs[1..] {
            let mut binding = resolved.model_binding.binding().clone();
            binding.backend_ref = (*backend_ref).to_string();
            resolved
                .model_candidates
                .push(awaken_runtime_contract::resolved::ResolvedModelCandidate::host(binding));
        }
        resolved
    }

    #[tokio::test]
    async fn attempt_output_harvest_decision_table() {
        // Cause/effect graph: C1=execute/resume/cancel; C2=inner success/failure;
        // C3=Sandbox output absent/present/changed; C4=artifact publisher available;
        // C5=current dispatch claim absent/present.
        // Effects: E1 one Session-scoped immutable File per unique content; E2
        // successful attempt waits for durable publication; E3 failed attempt keeps
        // its original error while best-effort publishing partial output; E4 cancel
        // does not invent a completed-step boundary; E5 publication failure blocks a
        // successful step; E6 every effect uses the claim snapshotted before the
        // attempt; E7 a settled/replaced local claim publishes nothing; E8 an
        // attempt that began without a claim never borrows one installed in-flight.
        // The same decorator is used by direct and durable ingress.
        //
        // | Rule | operation | inner | output | publisher/claim | effect |
        // | R1 | execute | ok | present | yes/current | E1+E2; exact claim forwarded |
        // | R2 | resume | ok | changed | yes | new version + E2 |
        // | R3 | execute | error | present | yes | E1+E3 |
        // | R4 | cancel | ok | any | yes | E4 |
        // | R5 | execute | ok | present | no | E5 |
        // | R6 | execute | ok | present | claim replaced in-flight | E6 |
        // | R7 | execute | ok | present | stale local claim | E7 |
        // | R8 | resume | ok | present | claim replaced in-flight | E6 |
        // | R9 | execute | error | present | claim replaced in-flight | E3+E6 |
        // | R10 | execute | ok | present | absent then installed | E8 |
        // R8 and R9 provide MC/DC for the resume and failure branches around the
        // shared `finish` path; enumerating resume+failure would be redundant.
        let thread = "thread-router";
        let storage = tempfile::tempdir().unwrap();
        let dispatch = Arc::new(
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory()
                .expect("artifact test dispatch"),
        );
        dispatch
            .enqueue(awaken_run_ingress::RunDispatch::new(activation("awaken")))
            .await
            .expect("enqueue artifact attempt");
        let claimed = dispatch
            .claim(
                "worker-a",
                60_000,
                awaken_run_ingress::SystemClock.now_ms(),
                &Default::default(),
            )
            .await
            .expect("claim artifact attempt")
            .expect("artifact attempt available");
        let expected_claim = awaken_run_ingress::RunClaim::from(&claimed.lease);
        let mut raw_host =
            SharedHost::new(Arc::new(NoLlm), "test").with_dispatch_store(dispatch.clone());
        let claims = Arc::new(std::sync::Mutex::new(Vec::new()));
        raw_host.artifact_publisher = Arc::new(RecordingArtifactPublisher {
            inner: raw_host.artifact_publisher.clone(),
            claims: claims.clone(),
            dispatch: dispatch.clone(),
        });
        raw_host.session_slots.update(thread, |slot| {
            slot.dispatch_claim = Some(expected_claim.clone());
        });
        let host = Arc::new(raw_host);
        host.register_thread_workspace(thread, "workspace-a");
        let spec = crate::provisioning::agent_run_sandbox_spec(thread);
        assert_eq!(
            spec.outputs_path, "/mnt/session/outputs",
            "Managed Agents writes deliverables at its documented path"
        );
        let environment = Arc::new(crate::session_environment::SessionEnvironment::workdir(
            awaken_sandbox_local::LocalProvider::new(storage.path())
                .create_sandbox(&spec)
                .await
                .unwrap(),
        ));
        host.install_test_resident_session_environment(thread, environment);
        let output_dir = storage
            .path()
            .join(thread)
            .join(spec.outputs_path.trim_start_matches('/'));
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::write(output_dir.join("result.txt"), b"revision one").unwrap();

        let inner = Arc::new(RecordingExecutor::new("harvested"));
        let executor =
            ArtifactHarvestAttemptExecutor::new(inner.clone(), host.artifact_harvester());
        executor
            .execute(activation("awaken"), RuntimeRunContext::new())
            .await
            .expect("R1");
        let files = host
            .file_application()
            .unwrap()
            .list("workspace-a", Some(thread))
            .await
            .unwrap();
        assert_eq!(files.len(), 1, "R1");
        assert_eq!(files[0].filename, "result.txt", "R1");
        assert_eq!(
            claims
                .lock()
                .expect("recorded claims mutex poisoned")
                .first()
                .cloned()
                .flatten(),
            Some(expected_claim.clone()),
            "R1 exact claim captured before execution reaches publication"
        );

        std::fs::write(output_dir.join("result.txt"), b"revision two").unwrap();
        let resumed = activation("awaken");
        executor
            .resume(resumed.clone(), resume(&resumed), RuntimeRunContext::new())
            .await
            .expect("R2");
        assert_eq!(
            host.file_application()
                .unwrap()
                .list("workspace-a", Some(thread))
                .await
                .unwrap()
                .len(),
            2,
            "R2 creates a content version, not an overwrite"
        );

        std::fs::write(output_dir.join("partial.txt"), b"partial").unwrap();
        let error = ArtifactHarvestAttemptExecutor::new(
            Arc::new(FailingExecutor),
            host.artifact_harvester(),
        )
        .execute(activation("awaken"), RuntimeRunContext::new())
        .await
        .expect_err("R3");
        assert!(error.to_string().contains("inner attempt failed"), "R3");
        assert_eq!(
            host.file_application()
                .unwrap()
                .list("workspace-a", Some(thread))
                .await
                .unwrap()
                .len(),
            3,
            "R3"
        );

        executor
            .cancel(activation("awaken"), RuntimeRunContext::new())
            .await
            .expect("R4");
        assert_eq!(inner.cancels.load(Ordering::SeqCst), 1, "R4");

        let replacement = awaken_run_ingress::RunClaim {
            run_id: expected_claim.run_id.clone(),
            owner: "worker-new".into(),
            epoch: expected_claim.epoch + 1,
        };
        std::fs::write(output_dir.join("claim-race.txt"), b"claim race").unwrap();
        let recorded_before = claims.lock().expect("recorded claims mutex poisoned").len();
        ArtifactHarvestAttemptExecutor::new(
            Arc::new(ReplacingClaimExecutor::succeeding(
                host.session_slots.clone(),
                thread,
                replacement,
            )),
            host.artifact_harvester(),
        )
        .execute(activation("awaken"), RuntimeRunContext::new())
        .await
        .expect("R6");
        assert_recorded_claims_since(&claims, recorded_before, Some(&expected_claim), "R6");
        assert_eq!(
            host.file_application()
                .unwrap()
                .list("workspace-a", Some(thread))
                .await
                .unwrap()
                .len(),
            4,
            "R6"
        );

        host.session_slots.update(thread, |slot| {
            slot.dispatch_claim = Some(expected_claim.clone());
        });

        let replacement = awaken_run_ingress::RunClaim {
            run_id: expected_claim.run_id.clone(),
            owner: "worker-resume-new".into(),
            epoch: expected_claim.epoch + 1,
        };
        std::fs::write(
            output_dir.join("resume-claim-race.txt"),
            b"resume claim race",
        )
        .unwrap();
        let recorded_before = claims.lock().expect("recorded claims mutex poisoned").len();
        let resumed = activation("awaken");
        ArtifactHarvestAttemptExecutor::new(
            Arc::new(ReplacingClaimExecutor::succeeding(
                host.session_slots.clone(),
                thread,
                replacement,
            )),
            host.artifact_harvester(),
        )
        .resume(resumed.clone(), resume(&resumed), RuntimeRunContext::new())
        .await
        .expect("R8");
        assert_recorded_claims_since(&claims, recorded_before, Some(&expected_claim), "R8");
        assert_eq!(
            host.file_application()
                .unwrap()
                .list("workspace-a", Some(thread))
                .await
                .unwrap()
                .len(),
            5,
            "R8"
        );

        host.session_slots.update(thread, |slot| {
            slot.dispatch_claim = Some(expected_claim.clone());
        });
        let replacement = awaken_run_ingress::RunClaim {
            run_id: expected_claim.run_id.clone(),
            owner: "worker-failing-new".into(),
            epoch: expected_claim.epoch + 1,
        };
        std::fs::write(
            output_dir.join("failed-claim-race.txt"),
            b"failed claim race",
        )
        .unwrap();
        let recorded_before = claims.lock().expect("recorded claims mutex poisoned").len();
        let error = ArtifactHarvestAttemptExecutor::new(
            Arc::new(ReplacingClaimExecutor::failing(
                host.session_slots.clone(),
                thread,
                replacement,
                "in-flight replacement failed",
            )),
            host.artifact_harvester(),
        )
        .execute(activation("awaken"), RuntimeRunContext::new())
        .await
        .expect_err("R9");
        assert!(
            error.to_string().contains("in-flight replacement failed"),
            "R9"
        );
        assert_recorded_claims_since(&claims, recorded_before, Some(&expected_claim), "R9");
        assert_eq!(
            host.file_application()
                .unwrap()
                .list("workspace-a", Some(thread))
                .await
                .unwrap()
                .len(),
            6,
            "R9"
        );

        host.session_slots
            .update(thread, |slot| slot.dispatch_claim = None);
        let replacement = awaken_run_ingress::RunClaim {
            run_id: expected_claim.run_id.clone(),
            owner: "worker-late".into(),
            epoch: expected_claim.epoch + 1,
        };
        std::fs::write(output_dir.join("no-claim-race.txt"), b"no claim race").unwrap();
        let recorded_before = claims.lock().expect("recorded claims mutex poisoned").len();
        ArtifactHarvestAttemptExecutor::new(
            Arc::new(ReplacingClaimExecutor::succeeding(
                host.session_slots.clone(),
                thread,
                replacement,
            )),
            host.artifact_harvester(),
        )
        .execute(activation("awaken"), RuntimeRunContext::new())
        .await
        .expect("R10");
        assert_recorded_claims_since(&claims, recorded_before, None, "R10");
        assert_eq!(
            host.file_application()
                .unwrap()
                .list("workspace-a", Some(thread))
                .await
                .unwrap()
                .len(),
            7,
            "R10"
        );

        host.session_slots.update(thread, |slot| {
            slot.dispatch_claim = Some(expected_claim.clone());
        });

        dispatch
            .settle(
                &claimed.lease.run_id,
                claimed.lease.epoch,
                awaken_run_ingress::DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("settle artifact claim");
        std::fs::write(output_dir.join("stale.txt"), b"must not publish").unwrap();
        let stale_error = executor
            .execute(activation("awaken"), RuntimeRunContext::new())
            .await
            .expect_err("R7 stale local claim");
        assert!(
            stale_error
                .to_string()
                .contains("lost its local dispatch claim"),
            "R7: {stale_error}"
        );
        assert_eq!(
            host.file_application()
                .unwrap()
                .list("workspace-a", Some(thread))
                .await
                .unwrap()
                .len(),
            7,
            "R7"
        );

        let failed_storage = tempfile::tempdir().unwrap();
        let mut raw_host = SharedHost::new(Arc::new(NoLlm), "test");
        raw_host.file_application = None;
        raw_host.artifact_publisher =
            Arc::new(awaken_resource_contract::UnavailableArtifactPublisher);
        let failed_host = Arc::new(raw_host);
        failed_host.register_thread_workspace(thread, "workspace-a");
        let failed_environment = Arc::new(crate::session_environment::SessionEnvironment::workdir(
            awaken_sandbox_local::LocalProvider::new(failed_storage.path())
                .create_sandbox(&spec)
                .await
                .unwrap(),
        ));
        failed_host.install_test_resident_session_environment(thread, failed_environment);
        let failed_output = failed_storage
            .path()
            .join(thread)
            .join(spec.outputs_path.trim_start_matches('/'));
        std::fs::create_dir_all(&failed_output).unwrap();
        std::fs::write(failed_output.join("blocked.txt"), b"blocked").unwrap();
        let error = ArtifactHarvestAttemptExecutor::new(
            Arc::new(RecordingExecutor::new("must-not-escape")),
            failed_host.artifact_harvester(),
        )
        .execute(activation("awaken"), RuntimeRunContext::new())
        .await
        .expect_err("R5");
        assert!(
            error
                .to_string()
                .contains("artifact publisher is not configured"),
            "R5: {error}"
        );
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
