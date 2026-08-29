//! The dispatch worker: claim one runnable dispatch, run it, settle it.
//!
//! The worker is the only place that converts a durable dispatch into a Runtime
//! attempt. It is additive over runtime control (G6): it never reaches into the
//! loop, it calls the same `RunExecutor`/`Runtime::resume` a direct caller would,
//! and it decides execute-vs-resume from *committed truth* — the awaiting ticket
//! and the run record — not from a duplicated status in the queue. That keeps the
//! DispatchQueue aggregate free of run-outcome truth.

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use awaken_agent_contract::ThreadCommit;
use awaken_agent_contract::agent::awaiting::AwaitReason;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::Coordinator as CommitCoordinator;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_runtime::Runtime;
use awaken_runtime_contract::execution::RunAttemptExecutor;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::{
    AttemptOwnershipError, AttemptOwnershipVerifier, RuntimeRunContext,
};
use awaken_runtime_contract::terminal::redeliver_committed_terminal;
use awaken_runtime_contract::{
    AttemptCredentialBinding, AttemptCredentialRealization, CredentialRealizationReceipt,
    CredentialRealizationRecordError, CredentialRealizationRecorder,
};
use awaken_session_contract::{SessionRunActivityAdmission, SessionRunActivityAdmissionMode};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::Error;
use crate::clock::Clock;
use crate::commit_fence::{ClaimedCommitCoordinator, ClaimedRunCommit, GuardedRunCommit};
use crate::dispatch::{
    Claimed, Dispatch, DispatchOutcome, DispatchState, PendingInput, RunClaim,
    SessionRunReservationResolution, SettleOutcome,
};
use crate::worker_context::WorkerContext;

/// Default lease: how long a claimed dispatch is owned before it is reclaimable.
pub const DEFAULT_LEASE_MS: u64 = 30_000;

/// Runs durable dispatches against a runtime. Generic over the store so the same
/// engine drives the in-memory reference and the Postgres backend unchanged.
pub struct DispatchWorker<S> {
    runtime: Arc<Runtime>,
    attempt_executor: std::sync::RwLock<Arc<dyn RunAttemptExecutor>>,
    store: Arc<S>,
    exec: WorkerContext,
    reader: Arc<dyn CommittedThreadView>,
    claimed_commit: Arc<dyn ClaimedRunCommit>,
    recovery_projection: Option<Arc<crate::RecoveryProjection>>,
    recovery_source: Option<Arc<dyn RunRecoverySource>>,
    settlement_observer: Option<Arc<dyn crate::DispatchSettlementObserver>>,
    owner: String,
    lease_ms: u64,
    cancellation: Option<CancellationToken>,
    local_credential_capabilities: awaken_runtime_contract::CredentialRealizationCapabilities,
    worker_credential_resolver:
        Option<Arc<dyn awaken_runtime_contract::WorkerLocalCredentialResolver>>,
}

struct ClaimBoundOwnershipVerifier {
    dispatch: Arc<dyn crate::DispatchQueue>,
    claim: RunClaim,
    clock: Arc<dyn Clock>,
}

struct ClaimBoundCredentialRecorder {
    dispatch: Arc<dyn crate::DispatchQueue>,
    claim: RunClaim,
}

#[async_trait::async_trait]
impl CredentialRealizationRecorder for ClaimBoundCredentialRecorder {
    async fn record(
        &self,
        receipt: CredentialRealizationReceipt,
    ) -> Result<(), CredentialRealizationRecordError> {
        match self
            .dispatch
            .record_credential_realization(&self.claim, receipt)
            .await
        {
            Ok(SettleOutcome::Applied) => Ok(()),
            Ok(SettleOutcome::Fenced) => Err(CredentialRealizationRecordError(
                "dispatch claim was superseded before receipt commit".to_string(),
            )),
            Err(error) => Err(CredentialRealizationRecordError(error.to_string())),
        }
    }
}

/// Build the neutral ownership verifier for one exact dispatch claim.
///
/// Runtime hosts use this before Session construction; [`DispatchWorker`] uses
/// the same adapter during attempt execution. Keeping construction here avoids a
/// second claim/clock interpretation in an embedding application.
pub fn claim_bound_ownership_verifier(
    dispatch: Arc<dyn crate::DispatchQueue>,
    claim: RunClaim,
    clock: Arc<dyn Clock>,
) -> Arc<dyn AttemptOwnershipVerifier> {
    Arc::new(ClaimBoundOwnershipVerifier {
        dispatch,
        claim,
        clock,
    })
}

#[async_trait::async_trait]
impl AttemptOwnershipVerifier for ClaimBoundOwnershipVerifier {
    async fn verify_current(&self) -> Result<(), AttemptOwnershipError> {
        match self
            .dispatch
            .claim_is_current(&self.claim, self.clock.now_ms())
            .await
        {
            Ok(true) => Ok(()),
            Ok(false) => Err(AttemptOwnershipError::Lost),
            Err(error) => Err(AttemptOwnershipError::Unavailable(error.to_string())),
        }
    }
}

/// One exact claim's renewal lifecycle. Renewal belongs beside the drive that
/// owns the claim, rather than to each caller (pool, daemon, or foreground child),
/// so every execution path has the same lease behavior.
pub(crate) struct ClaimLeaseRenewal {
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ClaimLeaseRenewal {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.task.abort();
    }
}

enum CommittedTerminalSettlement {
    Applied(RunId, RunState),
    Fenced,
}

impl CommittedTerminalSettlement {
    fn into_processed(self) -> Option<(RunId, RunState)> {
        match self {
            Self::Applied(run_id, state) => Some((run_id, state)),
            Self::Fenced => None,
        }
    }
}

/// Keep an exact claim live across any owned work, including the potentially
/// slow Worker/Session resolution that precedes [`DispatchWorker::drive_claimed`].
pub(crate) fn renew_claim_while_active<S: Dispatch + 'static>(
    store: Arc<S>,
    claim: &RunClaim,
    lease_ms: u64,
    clock: Arc<dyn Clock>,
) -> ClaimLeaseRenewal {
    let run_id = claim.run_id.clone();
    let owner = claim.owner.clone();
    let interval = Duration::from_millis((lease_ms / 3).max(1));
    let shutdown = CancellationToken::new();
    let task_shutdown = shutdown.clone();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = task_shutdown.cancelled() => break,
                _ = tokio::time::sleep(interval) => {
                    match store.renew_lease(&run_id, &owner, lease_ms, clock.now_ms()).await {
                        Ok(true) => {}
                        Ok(false) => {
                            tracing::warn!(
                                run_id = %run_id.0,
                                %owner,
                                "dispatch lease renewal lost exact claim ownership"
                            );
                            break;
                        }
                        Err(error) => tracing::warn!(
                            run_id = %run_id.0,
                            %owner,
                            %error,
                            "dispatch lease renewal failed"
                        ),
                    }
                }
            }
        }
    });
    ClaimLeaseRenewal { shutdown, task }
}

impl<S: Dispatch + 'static> DispatchWorker<S> {
    /// Rebuild the sole durable causal scope from the exact claimed instruction.
    /// Every claim entry uses this helper so recovery, pre-execution failure, and
    /// ordinary execution cannot drift into separate trace carrier rules.
    fn claimed_dispatch_span(claimed: &Claimed) -> tracing::Span {
        awaken_observability::dispatch_span(claimed.request.traceparent.as_deref())
    }

    fn renew_claim_while_driving(
        &self,
        claim: &RunClaim,
        clock: Arc<dyn Clock>,
    ) -> ClaimLeaseRenewal {
        renew_claim_while_active(self.store.clone(), claim, self.lease_ms, clock)
    }

    async fn install_claimed_recovery_projection(
        &self,
        claim: &RunClaim,
        thread_id: &ThreadId,
    ) -> Result<(), Error> {
        if let Some(projection) = &self.recovery_projection {
            let snapshot = match &self.recovery_source {
                Some(source) => source
                    .recovery_snapshot(thread_id, &claim.run_id)
                    .await
                    .map_err(|error| {
                        crate::Error::Dispatch(crate::DispatchError::Rejected(error.to_string()))
                    })?,
                None => self.store.load_recovery_snapshot(claim).await?,
            };
            projection
                .install(&claim.run_id, snapshot)
                .map_err(|error| {
                    crate::Error::Dispatch(crate::DispatchError::Rejected(error.to_string()))
                })?;
        }
        Ok(())
    }

    async fn refresh_local_recovery_projection(
        &self,
        thread_id: &ThreadId,
        run_id: &RunId,
    ) -> Result<(), Error> {
        let (Some(projection), Some(source)) = (&self.recovery_projection, &self.recovery_source)
        else {
            return Ok(());
        };
        let snapshot = source
            .recovery_snapshot(thread_id, run_id)
            .await
            .map_err(|error| {
                crate::Error::Dispatch(crate::DispatchError::Rejected(error.to_string()))
            })?;
        projection.install(run_id, snapshot).map_err(|error| {
            crate::Error::Dispatch(crate::DispatchError::Rejected(error.to_string()))
        })
    }

    /// Persist the queue-authorized retry edge before any replacement attempt
    /// can reach an execution dependency.
    ///
    /// The dispatch row remains the only recovery/attempt authority. This writes
    /// its `Claimed.recovered` receipt through the same claim-fenced Thread
    /// commit used by the Run, so a failed marker commit prevents unobservable
    /// re-execution and is retried by the existing lease recovery path.
    async fn commit_recovered_attempt(
        &self,
        claimed: &Claimed,
        clock: Arc<dyn Clock>,
    ) -> Result<(), Error> {
        let run_id = claimed.request.run_id();
        let disposition = recovered_attempt_disposition(
            claimed.recovered,
            run_id,
            self.reader.run_state(run_id).as_ref(),
            self.reader.resume_ticket(run_id),
            &claimed.pending,
        )?;
        let Some(disposition) = disposition else {
            return Ok(());
        };
        let claim = RunClaim::from(&claimed.lease);
        let context = self.execution_context_with(
            &claim,
            claimed.request.execution_scope.as_ref(),
            &None,
            &[],
            clock,
        );
        let coordinator = context.commit.ok_or_else(|| {
            Error::Execution(awaken_runtime_contract::execution::Error::Execution(
                "recovered dispatch has no claim-fenced commit coordinator".to_string(),
            ))
        })?;
        coordinator
            .commit(ThreadCommit::rescheduled(
                claimed.request.thread_id().clone(),
                disposition,
                claimed.lease.epoch,
            ))
            .await
            .map_err(|error| {
                Error::Execution(awaken_runtime_contract::execution::Error::Execution(
                    format!("commit recovered dispatch lifecycle: {error}"),
                ))
            })?;
        Ok(())
    }

    /// Wire a worker to its runtime, dispatch store, and durable commit boundary.
    /// The `commit` handle is the single source of durable truth: it is the
    /// commit coordinator the runtime writes through *and* the read port the
    /// worker consults for the ticket and run record (G6 same-source wiring).
    pub fn new<C>(
        runtime: Arc<Runtime>,
        store: Arc<S>,
        commit: Arc<C>,
        owner: impl Into<String>,
    ) -> Self
    where
        C: CommitCoordinator + CommittedThreadView + RunRecoverySource + Send + Sync + 'static,
    {
        // `commit` must back two independently erased ports; this clone only
        // increments the Arc count and does not clone coordinator state.
        let base_commit: Arc<dyn CommitCoordinator> = commit.clone();
        let recovery_source: Arc<dyn RunRecoverySource> = commit;
        let projection = Arc::new(crate::RecoveryProjection::new());
        // Keep the concrete Arc for refresh installs while exposing the same
        // projection through the read port.
        let reader: Arc<dyn CommittedThreadView> = projection.clone();
        let mut worker = Self::from_parts(runtime, store, base_commit, reader, owner);
        worker.recovery_projection = Some(projection);
        worker.recovery_source = Some(recovery_source);
        worker
    }

    /// Build a worker when the write and read sides are already erased behind
    /// their domain interfaces. This is the normal composition for a child Run:
    /// it inherits the parent's durable commit/history authority but owns a
    /// distinct dispatch row, claim, lease, and recovery lifecycle.
    pub fn from_parts(
        runtime: Arc<Runtime>,
        store: Arc<S>,
        commit: Arc<dyn CommitCoordinator>,
        reader: Arc<dyn CommittedThreadView>,
        owner: impl Into<String>,
    ) -> Self {
        let dispatch: Arc<dyn crate::dispatch::DispatchQueue> = store.clone();
        let attempt_executor: Arc<dyn RunAttemptExecutor> = runtime.clone();
        Self {
            runtime,
            attempt_executor: std::sync::RwLock::new(attempt_executor),
            store,
            exec: WorkerContext::new(commit.clone()).with_reader(reader.clone()),
            reader,
            claimed_commit: Arc::new(GuardedRunCommit::new(commit, dispatch)),
            recovery_projection: None,
            recovery_source: None,
            settlement_observer: None,
            owner: owner.into(),
            lease_ms: DEFAULT_LEASE_MS,
            cancellation: None,
            local_credential_capabilities: Default::default(),
            worker_credential_resolver: None,
        }
    }

    /// Inherit the same attempt capabilities as direct execution. The worker
    /// still replaces commit authority with the exact claim fence; a supplied
    /// cancellation token is preserved so parent cancellation reaches a live
    /// child Run.
    #[must_use]
    pub fn with_context(mut self, context: RuntimeRunContext) -> Self {
        self.cancellation = context.cancellation.clone();
        self.exec = self.exec.with_context(context);
        self
    }

    /// Install the session's selected executor before its first claim. The lock
    /// is read only long enough to clone the `Arc`; no executor call holds it.
    pub fn install_attempt_executor(&self, executor: Arc<dyn RunAttemptExecutor>) {
        *self
            .attempt_executor
            .write()
            .expect("attempt executor lock poisoned") = executor;
    }

    pub(crate) fn attempt_executor(&self) -> Arc<dyn RunAttemptExecutor> {
        self.attempt_executor
            .read()
            .expect("attempt executor lock poisoned")
            .clone()
    }

    /// Override how a claimed worker commit is applied. Database-less workers
    /// inject the server-side atomic implementation; local workers keep the
    /// guarded store implementation installed by [`new`](Self::new).
    #[must_use]
    pub fn with_claimed_commit(mut self, commit: Arc<dyn ClaimedRunCommit>) -> Self {
        self.claimed_commit = commit;
        self
    }

    /// Install the non-authoritative read cache used by a database-independent
    /// Worker. Every claimed drive must load it before executor entry.
    #[must_use]
    pub fn with_recovery_projection(mut self, projection: Arc<crate::RecoveryProjection>) -> Self {
        // `new` creates a local projection so every Worker has a coherent read
        // boundary before topology is known. Replacing that boundary for a
        // database-independent Worker must replace every reader as well as the
        // install target; otherwise the fetched snapshot lands in one cache
        // while resume selection and the Runtime read a second empty cache.
        let reader: Arc<dyn CommittedThreadView> = projection.clone();
        self.reader = reader.clone();
        self.exec = self.exec.with_reader(reader);
        self.recovery_projection = Some(projection);
        self.recovery_source = None;
        self
    }

    /// Install the one fallible hook that must complete after committed
    /// Awaiting/Ended truth and before dispatch settlement. Its error leaves the
    /// queue row intact so crash/transport recovery redelivers the same boundary.
    #[must_use]
    pub fn with_settlement_observer(
        mut self,
        observer: Arc<dyn crate::DispatchSettlementObserver>,
    ) -> Self {
        self.settlement_observer = Some(observer);
        self
    }

    /// Attach a best-effort live stream sink to every attempt this worker runs.
    #[must_use]
    pub fn with_stream_sink(
        mut self,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Self {
        self.exec = self.exec.with_stream_sink(sink);
        self
    }

    /// Attach the claim-aware publisher used by a database-less worker to relay
    /// live progress over its authenticated Coordinator transport.
    #[must_use]
    pub fn with_claimed_stream_publisher(
        mut self,
        publisher: Arc<dyn crate::ClaimedStreamPublisher>,
    ) -> Self {
        self.exec = self.exec.with_claimed_stream_publisher(publisher);
        self
    }

    /// Attach the durable interrupted-stream checkpoint store to every attempt, so
    /// a dispatch re-executed after a crash resumes its in-flight step (Phase 3).
    #[must_use]
    pub fn with_stream_checkpoint(
        mut self,
        store: Arc<dyn awaken_agent_contract::stream::checkpoint::StreamCheckpointStore>,
    ) -> Self {
        self.exec = self.exec.with_stream_checkpoint(store);
        self
    }

    /// Provide the per-session live inbox so this worker's runs drain mid-run
    /// steer at their safe loop boundaries (ADR-0054 P2).
    #[must_use]
    pub fn with_live_inbox(
        mut self,
        inbox: awaken_runtime_contract::live_inbox::LiveInbox,
    ) -> Self {
        self.exec = self.exec.with_live_inbox(inbox);
        self
    }

    /// Install the model→executor resolver (R1), so a worker-driven run resolves its
    /// effective model ref to the configured executor without a config service.
    #[must_use]
    pub fn with_inference_materializer(
        mut self,
        resolve: crate::worker_context::InferenceMaterializerFn,
    ) -> Self {
        self.exec = self.exec.with_inference_materializer(resolve);
        self
    }

    /// Override the lease duration.
    #[must_use]
    pub fn with_lease_ms(mut self, lease_ms: u64) -> Self {
        self.lease_ms = lease_ms;
        self
    }

    #[must_use]
    pub fn with_local_credential_capabilities(
        mut self,
        capabilities: awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Self {
        self.local_credential_capabilities = capabilities;
        self
    }

    /// Install the Worker-local adapter that owns opaque provider login state.
    /// It is consulted only for exact references already pinned by placement.
    #[must_use]
    pub fn with_worker_credential_resolver(
        mut self,
        resolver: Arc<dyn awaken_runtime_contract::WorkerLocalCredentialResolver>,
    ) -> Self {
        self.worker_credential_resolver = Some(resolver);
        self
    }

    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.runtime
    }

    pub(crate) fn committed_reader(&self) -> Arc<dyn CommittedThreadView> {
        Arc::clone(&self.reader)
    }

    pub fn store(&self) -> &Arc<S> {
        &self.store
    }

    /// A runtime context bound to this worker's commit boundary, for an
    /// out-of-band commit such as a durable cancel.
    pub(crate) fn execution_context(&self) -> RuntimeRunContext {
        self.exec
            .runtime_context(self.cancellation.clone().unwrap_or_default(), None)
    }

    /// A run context for the attempt on `run_id` claimed under lease `epoch`, whose
    /// commit boundary is FENCED by that epoch (a stale owner cannot double-apply side
    /// effects — the commit twin of the settle fence) and whose inference routes
    /// through `model_executor` when the run resolved one (its provider-resolved
    /// model), else the runtime's bound (host default) executor.
    fn execution_context_with(
        &self,
        claim: &RunClaim,
        execution_scope: Option<&crate::ExecutionScopeRef>,
        model_executor: &Option<Arc<dyn awaken_runtime_contract::llm::LlmExecutor>>,
        credential_bindings: &[AttemptCredentialBinding],
        clock: Arc<dyn Clock>,
    ) -> RuntimeRunContext {
        // The base commit boundary, wrapped per drive because the fence epoch is per
        // claim. `self.store` (the dispatch queue) reports the run's current epoch, so
        // a superseded owner's per-step commits are rejected.
        let mut coordinator =
            ClaimedCommitCoordinator::new(self.claimed_commit.clone(), claim.clone());
        if let Some(projection) = &self.recovery_projection {
            coordinator = coordinator.with_recovery_projection(projection.clone());
        }
        let fenced: Arc<dyn CommitCoordinator> = Arc::new(coordinator);
        let dispatch: Arc<dyn crate::DispatchQueue> = self.store.clone();
        let ownership = claim_bound_ownership_verifier(dispatch.clone(), claim.clone(), clock);
        let mut ctx = self
            .exec
            .runtime_context(self.cancellation.clone().unwrap_or_default(), Some(claim))
            .with_commit(fenced)
            .with_ownership(ownership);
        // Durable ingress owns terminal delivery after committed truth is visible
        // and before the dispatch is settled. Keep observers on `self.exec` for
        // that replay, but do not let Runtime's best-effort finalizer create a
        // parallel delivery path for this claimed attempt. Direct Runtime callers
        // do not use this context builder and retain their existing behavior.
        ctx.terminal_observers.clear();
        if let Some(scope) = execution_scope {
            ctx = ctx.with_execution_scope(scope.clone());
        }
        if !credential_bindings.is_empty() {
            ctx = ctx.with_credential_realization(AttemptCredentialRealization::new(
                credential_bindings.to_vec(),
                Arc::new(ClaimBoundCredentialRecorder {
                    dispatch: dispatch.clone(),
                    claim: claim.clone(),
                }),
            ));
        }
        // Always install the claim-fenced adapter. A database-less Worker has no
        // local checkpoint authority (`inner=None`); the adapter then delegates
        // through the authenticated dispatch transport. Local SQLite/Postgres
        // workers retain their explicitly paired durable inner store.
        let inner_checkpoint = ctx.stream_checkpoint.clone();
        ctx = ctx.with_stream_checkpoint(Arc::new(crate::FencedStreamCheckpointStore::new(
            inner_checkpoint,
            dispatch,
            claim.clone(),
        )));
        match model_executor {
            Some(exec) => ctx.with_model_executor(exec.clone()),
            None => ctx,
        }
    }

    /// Perform a committed ScheduledAction in-process (ADR-0020), routing any
    /// inference it triggers through the run's resolved model executor when one applies
    /// and fencing its commits by the claim's lease `epoch`.
    async fn perform_scheduled(
        &self,
        claim: &RunClaim,
        now_ms: u64,
        context: RuntimeRunContext,
    ) -> Result<RunState, Error> {
        Ok(self
            .runtime
            .perform_scheduled_action(&claim.run_id, self.reader.as_ref(), context, now_ms)
            .await?)
    }

    /// Claim at most one runnable dispatch for this worker's owner, without
    /// driving it. Returns the claimed work (request, lease, pending input) or
    /// `None` when the queue is idle. A process-level pool claims here and then
    /// drives the result on the *owning session's* worker via
    /// [`drive_claimed`](Self::drive_claimed), so a run always executes on the
    /// runtime carrying its thread's model/tools/config (not whichever worker
    /// happened to claim it).
    pub async fn claim_one(&self, now_ms: u64) -> Result<Option<Claimed>, Error> {
        Ok(self
            .store
            .claim(
                &self.owner,
                self.lease_ms,
                now_ms,
                &self.local_credential_capabilities,
            )
            .await?)
    }

    /// Claim one known Run under the same lease/fence policy without taking work
    /// belonging to another session. Parent-mediated child execution uses this
    /// after scheduling the child's stable identity.
    pub async fn claim_run(&self, run_id: &RunId, now_ms: u64) -> Result<Option<Claimed>, Error> {
        Ok(self
            .store
            .claim_run(
                run_id,
                &self.owner,
                self.lease_ms,
                now_ms,
                &self.local_credential_capabilities,
            )
            .await?)
    }

    /// Atomically admit, claim, and drive a newly created Run. This closes the
    /// enqueue/claim race with the process pool while preserving the same lease,
    /// fencing, recovery, and settlement path as every other dispatch.
    pub async fn start_run(
        &self,
        request: awaken_run_ingress_contract::RunDispatch,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let now_ms = clock.now_ms();
        let unclaimed_request = request.clone();
        let claimed = self
            .store
            .claim_new_run(
                request,
                &self.owner,
                self.lease_ms,
                now_ms,
                &self.local_credential_capabilities,
            )
            .await?;
        let Some(claimed) = claimed else {
            // A parent may mediate a child whose frozen placement belongs to a
            // registered remote Worker. Local exact-claim admission deliberately
            // returns `None` for that request, but `None` must not discard the
            // child: idempotently enqueue the same stable Run so the compatible
            // process pool can claim it while the parent observes committed truth.
            self.store.enqueue(unclaimed_request).await?;
            return Ok(None);
        };
        self.drive_claimed(claimed, clock).await
    }

    /// Atomically deliver one durable input, claim its exact awaiting Run, and
    /// drive the resume boundary. This is the resume-side counterpart of
    /// [`start_run`](Self::start_run).
    pub async fn resume_run(
        &self,
        input: PendingInput,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let now_ms = clock.now_ms();
        let claimed = self
            .store
            .deliver_and_claim(
                input,
                &self.owner,
                self.lease_ms,
                now_ms,
                &self.local_credential_capabilities,
            )
            .await?;
        let Some(claimed) = claimed else {
            return Ok(None);
        };
        self.drive_claimed(claimed, clock).await
    }

    /// Claim and process at most one runnable dispatch. Returns the processed
    /// run's id and resulting state, or `None` when the queue is idle.
    pub async fn tick(&self, clock: Arc<dyn Clock>) -> Result<Option<(RunId, RunState)>, Error> {
        let now_ms = clock.now_ms();
        let Some(claimed) = self.claim_one(now_ms).await? else {
            return Ok(None);
        };
        self.drive_claimed(claimed, clock).await
    }

    /// Claim and drive one exact Run. This preserves queue isolation for callers
    /// that synchronously await a child boundary while the process pool remains
    /// free to drain every other Run.
    pub async fn tick_run(
        &self,
        run_id: &RunId,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let now_ms = clock.now_ms();
        let Some(claimed) = self.claim_run(run_id, now_ms).await? else {
            return Ok(None);
        };
        self.drive_claimed(claimed, clock).await
    }

    /// Drive an already-[`claim_one`](Self::claim_one)ed dispatch to a settled
    /// outcome on *this* worker's runtime and commit boundary, then settle it.
    /// Splitting claim from drive lets a process-level pool claim centrally and
    /// route each run to its owning session's worker, so the run executes with
    /// its thread's model/tools/config. Returns the run's id and resulting state.
    pub async fn drive_claimed(
        &self,
        claimed: Claimed,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let claim = RunClaim::from(&claimed.lease);
        let lease_renewal = self.renew_claim_while_driving(&claim, clock.clone());
        self.drive_claimed_with_renewal(claimed, clock, lease_renewal)
            .await
    }

    /// Continue one exact drive with the renewal guard that already covered
    /// pre-drive resolution. The process pool transfers its guard here instead
    /// of starting an overlapping Tokio task at the Worker boundary.
    pub(crate) async fn drive_claimed_with_renewal(
        &self,
        claimed: Claimed,
        clock: Arc<dyn Clock>,
        lease_renewal: ClaimLeaseRenewal,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        if claimed.session_activity_admission_required {
            return self
                .drive_claimed_inner(claimed, clock, lease_renewal)
                .await;
        }

        // One executable claim owns one durable trace relay. Keep execution,
        // committed-terminal replay, observer delivery, and fenced settlement
        // inside the same `wake.dispatch` span; fragmenting the span around only
        // the executor future disconnects post-commit background effects.
        let dispatch = Self::claimed_dispatch_span(&claimed);
        self.drive_claimed_inner(claimed, clock, lease_renewal)
            .instrument(dispatch)
            .await
    }

    async fn drive_claimed_inner(
        &self,
        claimed: Claimed,
        clock: Arc<dyn Clock>,
        _lease_renewal: ClaimLeaseRenewal,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        // One drive receives one edge-owned clock. Claim eligibility, lease
        // renewal, pre-effect ownership checks, and settlement fencing must all
        // read this same source; the Worker never substitutes a private clock.
        let now_ms = clock.now_ms();
        let run_id = claimed.request.run_id().clone();
        let activation = claimed.request.activation.clone();
        let thread_id = claimed.request.thread_id().clone();
        // The fence token this drive holds. Every settle below carries it so a stale
        // owner (whose lease lapsed and was re-claimed under a higher epoch) is
        // rejected and abandons instead of clobbering the reclaimer's dispatch.
        let claim = RunClaim::from(&claimed.lease);

        // Cause/effect decision table for an expired Session Run reservation:
        // C1=claim still current; C2=cancelled; C3=Session authority decision.
        // E1=delete unstarted intent; E2=bind epoch and publish ordinary Pending;
        // E3=return to Reserved for retry; E4=stale owner changes nothing.
        //
        // | Rule | C1 | C2 | C3 | Queue effect | Executor |
        // |---|---|---|---|---|---|
        // | R1 | T | T | receipt absent | E1 Rejected | never entered |
        // | R2 | T | T | receipt present | E2 Pending(cancelled) | later cancellation claim |
        // | R3 | T | F | admitted | E2 Pending | later ordinary claim |
        // | R4 | T | F | rejected | E1 Rejected | never entered |
        // | R5 | T | any | unavailable | E3 Reserved | never entered |
        // | R6 | F | any | any | E4 Fenced | never entered |
        if claimed.session_activity_admission_required {
            let observer = self.settlement_observer.as_ref();
            let observer_owns_fence =
                observer.is_some_and(|observer| observer.owns_session_run_reservation_fence());
            let local_guard = if observer.is_some() && !observer_owns_fence {
                self.store
                    .lock_session_run_reservation_epoch(&claim)
                    .await?
            } else {
                None
            };
            let cancellation_requested = local_guard
                .as_ref()
                .map_or(claimed.cancellation_requested, |guard| {
                    guard.cancellation_requested()
                });
            let mode = if cancellation_requested {
                SessionRunActivityAdmissionMode::RecoverOnly
            } else {
                SessionRunActivityAdmissionMode::RecoverOrAdmit
            };
            let has_local_guard = local_guard.is_some();
            let resolution = match observer {
                Some(observer) if observer_owns_fence || has_local_guard => {
                    observer
                        .admit_session_run_activity(&claimed.request, &claim, mode)
                        .await
                }
                Some(_) => Err(crate::DispatchSettlementError(
                    "Session Run reservation claim is stale".to_string(),
                )),
                None => Err(crate::DispatchSettlementError(
                    "Session Run activity admission observer is not installed".to_string(),
                )),
            };
            // The guard spans only the Session CAS. Queue resolution acquires
            // its own claim-fenced transaction after this lock is released.
            drop(local_guard);
            match resolution {
                Ok(SessionRunActivityAdmission::Admitted {
                    session_activity_epoch,
                }) => {
                    self.store
                        .resolve_claimed_session_run_reservation(
                            &claim,
                            SessionRunReservationResolution::Admitted {
                                session_activity_epoch,
                            },
                        )
                        .await?;
                }
                Ok(SessionRunActivityAdmission::Rejected) => {
                    self.store
                        .resolve_claimed_session_run_reservation(
                            &claim,
                            SessionRunReservationResolution::Rejected,
                        )
                        .await?;
                }
                Err(error) => {
                    self.store
                        .resolve_claimed_session_run_reservation(
                            &claim,
                            SessionRunReservationResolution::Retry {
                                reservation_ttl_ms: self.lease_ms,
                            },
                        )
                        .await?;
                    tracing::warn!(
                        run_id = %run_id.0,
                        %error,
                        "Session Run reservation admission deferred"
                    );
                }
            }
            return Ok(None);
        }

        // Only an executable claim enters Run metrics. ReservationLeased is a
        // claim-fenced Session admission repair and returns above without model,
        // tool, or Run settlement effects; counting it as a driven Run would
        // inflate execution concurrency and recovery telemetry.
        self.runtime.metrics().record_dispatch_claimed();
        if claimed.recovered {
            self.runtime.metrics().record_dispatch_recovered();
        }
        self.runtime.metrics().record_dispatch_in_flight(1);
        let _drive_timer = DriveTimer::new(self.runtime.metrics());

        // Operations-only query: a metrics backend failure cannot reject a claim.
        // Native authorities return an exact claimable count; remote/composed stores
        // may report `None` until their transport exposes the query.
        if let Ok(Some(depth)) = self.store.runnable_depth(now_ms).await {
            self.runtime.metrics().record_dispatch_queue_depth(depth);
        }
        self.install_claimed_recovery_projection(&claim, &thread_id)
            .await?;
        let mut all_pending: Vec<String> = claimed
            .pending
            .iter()
            .map(|p| p.message_id.clone())
            .collect();

        // Cancellation is itself a durable claimed attempt. It deliberately runs
        // before model/credential/sandbox materialization: terminal control must
        // remain possible when the execution dependency being cancelled is down.
        if claimed.cancellation_requested {
            let attempt_executor = self.attempt_executor();
            let context = self.execution_context_with(
                &claim,
                claimed.request.execution_scope.as_ref(),
                &None,
                &[],
                clock.clone(),
            );
            if let Err(error) = attempt_executor
                .cancel(activation.clone(), context.clone())
                .await
            {
                return self
                    .settle_if_terminal_or_raise(&claimed, &all_pending, error, &clock)
                    .await;
            }
            let ticket = self.reader.resume_ticket(&run_id);
            let awaiting_tool_interrupt = cancellation_uses_tool_interruption(
                claimed.request.session_activity_epoch.is_some(),
                self.reader.run_state(&run_id).as_ref(),
                ticket.as_ref(),
            );
            let result = if awaiting_tool_interrupt {
                self.runtime
                    .interrupt_awaiting_tools(run_id.clone(), activation.thread_id.clone(), context)
                    .await
            } else {
                self.runtime.cancel_activation(activation, context).await
            };
            let state = match result {
                Ok(state) => state,
                Err(error) => {
                    return self
                        .settle_if_terminal_or_raise(&claimed, &all_pending, error, &clock)
                        .await;
                }
            };
            if matches!(&state, RunState::Ended(_)) {
                self.redeliver_terminal_observers(&run_id, &thread_id)
                    .await?;
            }
            return Ok(self
                .settle(
                    &claimed,
                    Some(&state),
                    DispatchOutcome::Done,
                    &all_pending,
                    &clock,
                )
                .await?
                .applied()
                .then_some((run_id, state)));
        }

        // Committed terminal truth dominates backend-specific recovery. This
        // also owns cleanup of idle-thread input proven to have been delivered
        // before the prior owner crashed, so the ACP fail-closed rule below
        // cannot accidentally make that input visible to a later Run.
        if let Some(settlement) = self
            .settle_from_committed_terminal(&claimed, &mut all_pending, &clock)
            .await?
        {
            return Ok(settlement.into_processed());
        }

        // Cause/effect recovery rule A2: a reclaimed ACP Run is opaque. The
        // previous Worker may have dispatched the prompt or an MCP effect but
        // failed before committing the reply. Re-executing here would duplicate
        // an external effect; the existing neutral Indeterminate terminal is the
        // only sound committed truth until ACP exposes an idempotent Run receipt.
        if claimed.recovered
            && awaken_runtime_contract::resolved::Backend::from_ref(
                &activation
                    .snapshot
                    .resolved_spec
                    .model_binding
                    .binding()
                    .backend_ref,
            )
            .is_acp()
        {
            return self
                .end_claimed_before_execution(
                    &claimed,
                    awaken_agent_contract::agent::run::EndCause::Indeterminate,
                    clock.clone(),
                )
                .await;
        }

        // A native replacement attempt may execute only after its durable
        // reschedule receipt crossed the exact claim epoch. Committed terminal
        // recovery and opaque ACP terminalization returned above and therefore
        // never fabricate a retry edge.
        self.commit_recovered_attempt(&claimed, clock.clone())
            .await?;

        let attempt_executor = self.attempt_executor();
        if !claimed.request.placement.required_credentials.is_empty() {
            let resolver = self.worker_credential_resolver.as_ref().ok_or_else(|| {
                Error::Execution(awaken_runtime_contract::execution::Error::Execution(
                    "worker-local credential resolver is not installed".to_string(),
                ))
            })?;
            let ownership =
                claim_bound_ownership_verifier(self.store.clone(), claim.clone(), clock.clone());
            ownership.verify_current().await.map_err(|error| {
                Error::Execution(awaken_runtime_contract::execution::Error::Execution(
                    format!("dispatch ownership was lost before credential revalidation: {error}"),
                ))
            })?;
            for required in &claimed.request.placement.required_credentials {
                resolver
                    .revalidate_worker_reference(&awaken_runtime_contract::CredentialRef {
                        id: required.id.clone(),
                        revision: required.revision,
                    })
                    .await
                    .map_err(|error| {
                        Error::Execution(
                            awaken_runtime_contract::execution::Error::Execution(format!(
                                "worker-local credential {} revision {} failed use-time revalidation: {error}",
                                required.id, required.revision
                            )),
                        )
                    })?;
                ownership.verify_current().await.map_err(|error| {
                    Error::Execution(awaken_runtime_contract::execution::Error::Execution(
                        format!(
                            "dispatch ownership was lost during credential revalidation: {error}"
                        ),
                    ))
                })?;
            }
        }
        // Resolve this run's model to an executor once, before the activation is
        // consumed, and route every inference in this drive through it: the run's
        // effective model (its per-run override, else its snapshot binding) resolved
        // through the injected provider — which owns how the model is reached (local
        // credentials or a gateway offering). `None` leaves the runtime's bound (host
        // default) executor, so a single-model deployment is unaffected.
        let mut execution_context = self.execution_context_with(
            &claim,
            claimed.request.execution_scope.as_ref(),
            &None,
            &claimed.credential_bindings,
            clock.clone(),
        );
        let model_executor = self
            .exec
            .materialize_inference(&claimed.request.activation, &execution_context)?;
        if let Some(executor) = &model_executor {
            execution_context = execution_context.with_model_executor(executor.clone());
        }
        let _attempt_tracking = self.runtime.track_active_attempt(
            &run_id,
            claimed.request.thread_id(),
            &execution_context,
        );
        let mut state = match self.reader.resume_ticket(&run_id) {
            // A committed ScheduledAction (ADR-0020): the system performs the
            // deferred action, not waits for external input. This also covers a
            // crash recovery of a scheduled await (no pending input is expected).
            Some(ticket) if ticket.reason() == AwaitReason::ScheduledAction => {
                match self
                    .perform_scheduled(&claim, now_ms, execution_context.clone())
                    .await
                {
                    Ok(state) => state,
                    Err(err) => {
                        return self
                            .settle_if_terminal_or_raise(&claimed, &all_pending, err, &clock)
                            .await;
                    }
                }
            }
            // The run is awaiting. Deliver only input whose correlation matches the
            // committed ticket; input for a superseded ticket (stale) is dropped
            // without delivery. Input that already drove a committed resume left a
            // ticket with a different correlation (or none), so it is never
            // re-applied (ADR-0010).
            Some(ticket) => {
                let matched = claimed
                    .pending
                    .iter()
                    .find(|p| p.correlation_id == ticket.correlation_id)
                    .cloned();
                match matched {
                    Some(input) => {
                        let command = ResumeCommand::from_ticket(&ticket, input.result, now_ms)
                            .with_operation_id(input.message_id.clone())
                            .with_context_messages(input.context_messages);
                        match attempt_executor
                            .resume(activation.clone(), command, execution_context.clone())
                            .await
                        {
                            Ok(state) => state,
                            Err(err) => {
                                return self
                                    .settle_if_terminal_or_raise(
                                        &claimed,
                                        &all_pending,
                                        err,
                                        &clock,
                                    )
                                    .await;
                            }
                        }
                    }
                    None => {
                        // No input answers the current ticket; drop stale input
                        // and leave the run awaiting for a later wake.
                        return Ok(self
                            .settle(
                                &claimed,
                                Some(&RunState::Awaiting),
                                DispatchOutcome::Awaiting,
                                &all_pending,
                                &clock,
                            )
                            .await?
                            .applied()
                            .then_some((run_id, RunState::Awaiting)));
                    }
                }
            }
            // No ticket: a fresh run, or a recovered run that already finished.
            // The committed run record disambiguates so recovery never re-runs a
            // terminal run, and any orphan pending is dropped on settle.
            None => match self
                .settle_from_committed_terminal(&claimed, &mut all_pending, &clock)
                .await?
            {
                Some(settlement) => return Ok(settlement.into_processed()),
                None => {
                    // A continuation admission carries input bound to this exact
                    // fresh Run. Generic idle-Thread input remains unbound and is
                    // still consumed by the next fresh Run (ADR-0021). Never scan
                    // another Run's bound input from the Thread inbox: two queued
                    // continuations on one Thread must remain one-to-one.
                    let thread = claimed.request.thread_id().clone();
                    let unbound: Vec<PendingInput> = self
                        .store
                        .list(&thread)
                        .await?
                        .into_iter()
                        .map(|r| r.input)
                        .filter(|input| input.run_id.0.is_empty())
                        .collect();
                    let mut activation = activation;
                    // Prepend each immediate fresh *input*. Claimed input with a
                    // correlation belongs to an awaiting ticket and is deliberately
                    // not reinterpreted as fresh input. The insert position tracks
                    // how many values were actually inserted, so a non-`Input` row
                    // cannot desync the index and cause a panic.
                    let mut at = 0;
                    for input in claimed
                        .pending
                        .iter()
                        .filter(|input| input.correlation_id.is_empty())
                        .chain(unbound.iter())
                    {
                        if let ResumeResult::Input(text) = &input.result {
                            activation.input.insert(
                                at,
                                Message {
                                    id: MessageId(input.message_id.clone()),
                                    role: Role::User,
                                    content: vec![ContentBlock::text(text)],
                                },
                            );
                            at += 1;
                        }
                    }
                    // Consumed on settle, not on read, so a crash re-delivers them.
                    all_pending.extend(unbound.into_iter().map(|input| input.message_id));
                    match attempt_executor
                        .execute(activation, execution_context.clone())
                        .await
                    {
                        Ok(state) => state,
                        Err(err) => {
                            return self
                                .settle_if_terminal_or_raise(&claimed, &all_pending, err, &clock)
                                .await;
                        }
                    }
                }
            },
        };

        // Drive any further scheduled actions to completion in-process: a run that
        // ends a step by committing a ScheduledAction is performed immediately,
        // until it ends or awaits on a wait that needs external input.
        while state == RunState::Awaiting {
            match self.reader.resume_ticket(&run_id) {
                Some(ticket) if ticket.reason() == AwaitReason::ScheduledAction => {
                    state = match self
                        .perform_scheduled(&claim, now_ms, execution_context.clone())
                        .await
                    {
                        Ok(state) => state,
                        Err(err) => {
                            return self
                                .settle_if_terminal_or_raise(&claimed, &all_pending, err, &clock)
                                .await;
                        }
                    };
                }
                _ => break,
            }
        }

        let outcome = settle_outcome(&state)?;
        if matches!(&state, RunState::Ended(_)) {
            self.redeliver_terminal_observers(&run_id, &thread_id)
                .await?;
        }
        Ok(self
            .settle(&claimed, Some(&state), outcome, &all_pending, &clock)
            .await?
            .applied()
            .then_some((run_id, state)))
    }

    /// Settle one claim from committed terminal truth without invoking an
    /// executor. This is the single owner of the crash-window inbox cleanup used
    /// both before backend recovery and after an awaiting-ticket race.
    async fn settle_from_committed_terminal(
        &self,
        claimed: &Claimed,
        all_pending: &mut Vec<String>,
        clock: &Arc<dyn Clock>,
    ) -> Result<Option<CommittedTerminalSettlement>, Error> {
        let run_id = claimed.request.run_id().clone();
        let Some(state @ RunState::Ended(_)) = self.reader.run_state(&run_id) else {
            return Ok(None);
        };
        let thread = claimed.request.thread_id().clone();
        self.redeliver_terminal_observers(&run_id, &thread).await?;

        // A recovered fresh run that already committed a terminal record may
        // have drained unbound idle-thread input before dying prior to settle.
        // Consume exactly rows whose ids exist in committed transcript truth;
        // later, undelivered input remains available to the next Run.
        let delivered: std::collections::HashSet<String> = self
            .reader
            .committed_messages(&thread)
            .into_iter()
            .map(|message| message.id.0)
            .collect();
        let consumed_unbound = self
            .store
            .list(&thread)
            .await?
            .into_iter()
            .map(|record| record.input)
            .filter(|input| input.run_id.0.is_empty() && delivered.contains(&input.message_id))
            .map(|input| input.message_id);
        all_pending.extend(consumed_unbound);
        let settled = self
            .settle(
                claimed,
                Some(&state),
                DispatchOutcome::Done,
                all_pending,
                clock,
            )
            .await?;
        Ok(Some(if settled.applied() {
            CommittedTerminalSettlement::Applied(run_id, state)
        } else {
            CommittedTerminalSettlement::Fenced
        }))
    }

    /// Settle a claimed dispatch and record the operational `runs.settled` counter
    /// (labelled by outcome) on the runtime's metrics recorder. The single choke all
    /// of `drive_claimed`'s settle arms route through, so every settle is metered
    /// exactly once regardless of which branch reached it.
    async fn settle(
        &self,
        claimed: &Claimed,
        expected_committed_state: Option<&RunState>,
        outcome: DispatchOutcome,
        consumed: &[String],
        clock: &Arc<dyn Clock>,
    ) -> Result<SettleOutcome, Error> {
        let run_id = claimed.request.run_id();
        let claim = RunClaim::from(&claimed.lease);
        let coordinated_claim_is_stale = if claimed.request.session_activity_epoch.is_some()
            && let Some(expected) = expected_committed_state
        {
            let committed = self.reader.run_state(run_id).ok_or_else(|| {
                Error::Dispatch(crate::DispatchError::Rejected(format!(
                    "coordinated child Run {} has no committed settlement state",
                    run_id.0
                )))
            })?;
            if &committed != expected || settle_outcome(&committed)? != outcome {
                return Err(Error::Dispatch(crate::DispatchError::Rejected(format!(
                    "coordinated child Run {} settlement does not match committed truth",
                    run_id.0
                ))));
            }
            if !self.store.claim_is_current(&claim, clock.now_ms()).await? {
                true
            } else {
                let observer = self.settlement_observer.as_ref().ok_or_else(|| {
                    Error::Dispatch(crate::DispatchError::Rejected(
                        "coordinated child settlement observer is not installed".to_string(),
                    ))
                })?;
                observer
                    .before_settle(
                        &claimed.request,
                        &claim,
                        &committed,
                        claimed.cancellation_requested,
                    )
                    .await
                    .map_err(|error| {
                        Error::Dispatch(crate::DispatchError::Rejected(error.to_string()))
                    })?;
                false
            }
        } else if claimed.request.session_activity_epoch.is_some()
            && outcome != DispatchOutcome::Awaiting
        {
            return Err(Error::Dispatch(crate::DispatchError::Rejected(
                "coordinated child settlement has no committed boundary".to_string(),
            )));
        } else {
            false
        };
        let label = match outcome {
            DispatchOutcome::Done => "done",
            DispatchOutcome::Awaiting => "awaiting",
        };
        let started = std::time::Instant::now();
        let settlement = if coordinated_claim_is_stale {
            Ok(SettleOutcome::Fenced)
        } else {
            self.store
                .settle(run_id, claimed.lease.epoch, outcome, consumed)
                .await
        };
        match settlement {
            Ok(result) => {
                self.record_settlement_outcome(result, label, started.elapsed());
                Ok(result)
            }
            Err(error) => {
                self.runtime
                    .metrics()
                    .record_dispatch_commit("error", started.elapsed());
                Err(error.into())
            }
        }
    }

    /// Record one authoritative settlement decision. Queue CAS fencing and the
    /// coordinated pre-observer ownership fence both converge here, preventing
    /// transport/topology-specific metric paths from drifting or double-counting.
    fn record_settlement_outcome(
        &self,
        result: SettleOutcome,
        applied_label: &str,
        duration: std::time::Duration,
    ) {
        let commit_outcome = match result {
            SettleOutcome::Applied => {
                self.runtime
                    .metrics()
                    .record_dispatch_settled(applied_label);
                "applied"
            }
            SettleOutcome::Fenced => {
                self.runtime.metrics().record_dispatch_fenced();
                "fenced"
            }
        };
        self.runtime
            .metrics()
            .record_dispatch_commit(commit_outcome, duration);
    }

    /// Convert a runtime-drive failure into a benign already-done when committed
    /// truth shows the run has reached a terminal state.
    ///
    /// The commit coordinators enforce terminal-is-final: once a run's committed
    /// state is `Ended`, a later commit for that run is rejected. That fence keeps
    /// the committed LOG exactly-once when a *stale* owner (slow-but-alive, its
    /// lease lapsed and superseded by a reclaimer that already drove the run to
    /// `Ended`) re-executes and tries to append a duplicate transcript — its commit
    /// fails. Surfacing that as a fatal error would strand the dispatch: it would
    /// sit un-settled until its lease lapsed, then be re-driven into the same
    /// rejected commit, burning crash-retries until exhaustion handling — even though
    /// the run is already complete. Instead the worker treats the lost race as
    /// already-done and settles the dispatch `Done` from committed truth, exactly
    /// as the "terminal committed record" recovery branch does. Any other error is
    /// a genuine fault and is re-raised unchanged.
    async fn settle_if_terminal_or_raise(
        &self,
        claimed: &Claimed,
        consumed: &[String],
        err: impl Into<Error>,
        clock: &Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let run_id = claimed.request.run_id();
        let thread_id = claimed.request.thread_id();
        let err = err.into();
        self.refresh_local_recovery_projection(thread_id, run_id)
            .await?;
        match self.reader.run_state(run_id) {
            Some(state @ RunState::Ended(_)) => {
                // This path lost a terminal-commit race. Redelivery is expected:
                // the winning attempt may have crashed after commit and observers
                // suppress duplicate effects by `(observer_id, run_id)`.
                self.redeliver_terminal_observers(run_id, thread_id).await?;
                Ok(self
                    .settle(
                        claimed,
                        Some(&state),
                        DispatchOutcome::Done,
                        consumed,
                        clock,
                    )
                    .await?
                    .applied()
                    .then_some((run_id.clone(), state)))
            }
            _ => {
                // A stale attempt can lose ownership before replacement
                // terminal truth is readable in this Worker's projection. The
                // queue already rejected its authority, so meter that exact
                // decision through the same choke as a stale settlement CAS.
                // Ordinary Runs retain their original execution error; a
                // coordinated Run quietly yields to its replacement because it
                // must not manufacture an Error boundary for the parent Session.
                let started = std::time::Instant::now();
                let claim = RunClaim::from(&claimed.lease);
                if !self.store.claim_is_current(&claim, clock.now_ms()).await? {
                    self.record_settlement_outcome(
                        SettleOutcome::Fenced,
                        "done",
                        started.elapsed(),
                    );
                    if claimed.request.session_activity_epoch.is_some() {
                        return Ok(None);
                    }
                }
                Err(err)
            }
        }
    }

    /// Commit and settle a deterministic failure returned while resolving this
    /// exact claimed attempt, before an executor could be constructed. This is
    /// the same terminal commit and claim fence used after execution; it is not a
    /// dead-letter shortcut. If authority was lost, commit/settle fencing leaves
    /// the replacement owner untouched and the error remains recoverable.
    pub async fn fail_claimed_before_execution(
        &self,
        claimed: &Claimed,
        code: impl Into<String>,
        message: impl Into<String>,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        use awaken_agent_contract::agent::run::{EndCause, Failure};

        let cause = EndCause::Error(Failure::Inference {
            code: code.into(),
            message: message.into(),
        });
        let dispatch = Self::claimed_dispatch_span(claimed);
        self.prepare_and_end_claimed_before_execution(claimed, cause, clock)
            .instrument(dispatch)
            .await
    }

    /// Commit the one neutral terminal outcome for an exact retry-exhaustion
    /// claim, then reuse the ordinary observer and fenced `Done` settlement.
    /// The queue command owns only retry eligibility; this Worker method is the
    /// sole bridge into committed Run truth for every local, pooled, and remote
    /// automatic path.
    pub async fn terminalize_retry_exhausted(
        &self,
        claimed: &Claimed,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let dispatch = Self::claimed_dispatch_span(claimed);
        self.prepare_and_end_claimed_before_execution(
            claimed,
            awaken_agent_contract::agent::run::EndCause::Indeterminate,
            clock,
        )
        .instrument(dispatch)
        .await
    }

    /// Install the claim-fenced recovery projection once before either public
    /// pre-execution terminal cause enters the shared commit/observer/settle
    /// owner. Keeping this preparation here prevents resolution failure and
    /// retry exhaustion from growing parallel terminal protocols.
    async fn prepare_and_end_claimed_before_execution(
        &self,
        claimed: &Claimed,
        cause: awaken_agent_contract::agent::run::EndCause,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let claim = RunClaim::from(&claimed.lease);
        self.install_claimed_recovery_projection(&claim, claimed.request.thread_id())
            .await?;
        self.end_claimed_before_execution(claimed, cause, clock)
            .await
    }

    /// Claim and terminalize at most one exhausted dispatch for the per-Session
    /// service. Process pools claim centrally and call
    /// [`terminalize_retry_exhausted`](Self::terminalize_retry_exhausted) on the
    /// resolved boundary Worker instead.
    pub async fn resolve_one_retry_exhausted(
        &self,
        max_attempts: u64,
        clock: Arc<dyn Clock>,
    ) -> Result<bool, Error> {
        let now_ms = clock.now_ms();
        let Some(claimed) = self
            .store
            .claim_retry_exhausted(&self.owner, self.lease_ms, now_ms, max_attempts)
            .await?
        else {
            return Ok(false);
        };
        let claim = RunClaim::from(&claimed.lease);
        let _renewal = self.renew_claim_while_driving(&claim, clock.clone());
        let _ = self.terminalize_retry_exhausted(&claimed, clock).await?;
        Ok(true)
    }

    /// Commit one deterministic terminal outcome before invoking an executor.
    /// Callers install any recovery projection first; this function owns the one
    /// claim-fenced commit/observer/settle sequence shared by resolution failures
    /// and opaque ACP crash recovery.
    async fn end_claimed_before_execution(
        &self,
        claimed: &Claimed,
        cause: awaken_agent_contract::agent::run::EndCause,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        use awaken_agent_contract::thread::commit::{RunDisposition, commit_run};

        let run_id = claimed.lease.run_id.clone();
        let thread_id = claimed.request.thread_id().clone();
        let consumed = claimed
            .pending
            .iter()
            .map(|pending| pending.message_id.clone())
            .collect::<Vec<_>>();
        let state = RunState::Ended(cause.clone());
        let claim = RunClaim::from(&claimed.lease);
        let context = self.execution_context_with(
            &claim,
            claimed.request.execution_scope.as_ref(),
            &None,
            &[],
            clock.clone(),
        );
        let coordinator = context.commit.ok_or_else(|| {
            Error::Execution(awaken_runtime_contract::execution::Error::Execution(
                "claimed pre-execution terminal has no commit coordinator".to_string(),
            ))
        })?;
        if let Err(error) = commit_run(
            coordinator.as_ref(),
            &thread_id,
            RunDisposition::ended(run_id.clone(), cause),
            Vec::new(),
            Vec::new(),
        )
        .await
        {
            return self
                .settle_if_terminal_or_raise(
                    claimed,
                    &consumed,
                    Error::Execution(awaken_runtime_contract::execution::Error::Execution(
                        error.to_string(),
                    )),
                    &clock,
                )
                .await;
        }
        self.redeliver_terminal_observers(&run_id, &thread_id)
            .await?;
        Ok(self
            .settle(
                claimed,
                Some(&state),
                DispatchOutcome::Done,
                &consumed,
                &clock,
            )
            .await?
            .applied()
            .then_some((run_id, state)))
    }

    /// Reconcile one quiescent awaiting dispatch or expired running lease whose
    /// committed Run is already terminal. The committed reader is checked before
    /// the special claim, and
    /// [`settle_claimed_terminal`](Self::settle_claimed_terminal) checks it again
    /// under that claim before reusing the ordinary fenced Done settlement and
    /// completion tombstone.
    pub async fn reconcile_committed_terminal(
        &self,
        run_id: &RunId,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let now_ms = clock.now_ms();
        // A local active-active worker's process projection may predate the peer
        // that committed the terminal fact. Refresh the exact thread snapshot
        // before making the claim/no-claim authority decision.
        if self.recovery_source.is_some()
            && let Some(row) = self
                .store
                .list_dispatches()
                .await?
                .into_iter()
                .find(|row| &row.run_id == run_id)
        {
            self.refresh_local_recovery_projection(&row.thread_id, run_id)
                .await?;
        }
        if !matches!(self.reader.run_state(run_id), Some(RunState::Ended(_))) {
            return Ok(None);
        }
        let Some(claimed) = self
            .store
            .claim_for_terminal_recovery(run_id, &self.owner, self.lease_ms, now_ms)
            .await?
        else {
            return Ok(None);
        };
        self.settle_claimed_terminal(claimed, clock).await
    }

    /// Bounded scan used by both the per-Session daemon and process pool host
    /// adapter. Rows outside this Worker's committed reader naturally return no
    /// state, so one implementation owns selection and settlement semantics.
    pub async fn reconcile_committed_terminals(
        &self,
        clock: Arc<dyn Clock>,
        limit: usize,
    ) -> Result<Vec<(RunId, RunState)>, Error> {
        let rows = self.store.list_dispatches().await?;
        let mut processed = Vec::new();
        for row in rows
            .into_iter()
            .filter(|row| matches!(row.state, DispatchState::Awaiting | DispatchState::Leased))
        {
            if processed.len() >= limit {
                break;
            }
            if let Some(terminal) = self
                .reconcile_committed_terminal(&row.run_id, clock.clone())
                .await?
            {
                processed.push(terminal);
            }
        }
        Ok(processed)
    }

    /// Complete a terminal-recovery claim without executing the Run. If the
    /// caller raced stale or incorrect read evidence, restore the row to
    /// Awaiting; only exact committed `Ended` truth can reach Done.
    pub async fn settle_claimed_terminal(
        &self,
        claimed: Claimed,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let dispatch = Self::claimed_dispatch_span(&claimed);
        self.settle_claimed_terminal_inner(claimed, clock)
            .instrument(dispatch)
            .await
    }

    async fn settle_claimed_terminal_inner(
        &self,
        claimed: Claimed,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let run_id = claimed.lease.run_id.clone();
        // Decision table invariant: every terminal-recovery caller must check
        // the authoritative snapshot bound to this exact claim, never a stale
        // process projection. Keeping the refresh here makes claim + verify +
        // fenced settlement one indivisible protocol for every entry point.
        let claim = RunClaim::from(&claimed.lease);
        self.install_claimed_recovery_projection(&claim, claimed.request.thread_id())
            .await?;
        let Some(state @ RunState::Ended(_)) = self.reader.run_state(&run_id) else {
            let _ = self
                .settle(&claimed, None, DispatchOutcome::Awaiting, &[], &clock)
                .await?;
            return Ok(None);
        };
        let thread_id = claimed.request.thread_id().clone();
        self.redeliver_terminal_observers(&run_id, &thread_id)
            .await?;
        Ok(self
            .settle(&claimed, Some(&state), DispatchOutcome::Done, &[], &clock)
            .await?
            .applied()
            .then_some((run_id, state)))
    }

    async fn redeliver_terminal_observers(
        &self,
        run_id: &RunId,
        thread_id: &ThreadId,
    ) -> Result<(), Error> {
        let context = self.execution_context();
        let Some(failures) = redeliver_committed_terminal(
            self.reader.as_ref(),
            &context.terminal_observers,
            run_id,
            thread_id,
        )
        .await
        else {
            return Ok(());
        };
        if failures.is_empty() {
            return Ok(());
        }

        for failure in &failures {
            tracing::warn!(
                observer.id = %failure.observer_id,
                awaken.run.id = %run_id.0,
                error = %failure.error,
                "terminal observer failed; dispatch settlement withheld for recovery"
            );
        }
        let failures = failures
            .into_iter()
            .map(|failure| format!("{}: {}", failure.observer_id, failure.error))
            .collect::<Vec<_>>()
            .join("; ");
        Err(Error::Execution(
            awaken_runtime_contract::execution::Error::Execution(format!(
                "committed-terminal observer delivery failed; dispatch settlement withheld for recovery: {failures}"
            )),
        ))
    }

    /// Drain the queue until no dispatch is runnable, returning every processed
    /// run and its state. A settled run becomes non-runnable, so this terminates.
    pub async fn run_until_idle(
        &self,
        clock: Arc<dyn Clock>,
    ) -> Result<Vec<(RunId, RunState)>, Error> {
        let mut processed = Vec::new();
        while let Some(result) = self.tick(clock.clone()).await? {
            processed.push(result);
        }
        Ok(processed)
    }
}

/// Select the existing batch-aware interruption only for a managed Session's
/// externally blocked Run. A missing ticket is included deliberately: storage
/// has isolated a damaged reply token, and Runtime must inspect the durable
/// ToolBatch to reconstruct or terminally quarantine that wait. Other valid
/// await reasons retain ordinary cancellation semantics.
fn cancellation_uses_tool_interruption(
    managed_session_run: bool,
    state: Option<&RunState>,
    ticket: Option<&awaken_agent_contract::agent::awaiting::ResumeTicket>,
) -> bool {
    managed_session_run
        && matches!(state, Some(RunState::Awaiting))
        && ticket.is_none_or(|ticket| {
            matches!(
                ticket.reason(),
                AwaitReason::ToolPermission | AwaitReason::ExternalEvent
            )
        })
}

/// Records the `awaken.dispatch.drive.duration` histogram on drop, so the wall
/// time of `drive_claimed` is metered on every exit path — the happy return, an
/// early `?`, and each terminal-recovery arm — without threading a timer through
/// them. Borrows the runtime's metrics recorder for the lifetime of one drive.
struct DriveTimer<'a> {
    start: Instant,
    metrics: &'a dyn awaken_runtime_contract::metrics::MetricsRecorder,
}

impl<'a> DriveTimer<'a> {
    fn new(metrics: &'a dyn awaken_runtime_contract::metrics::MetricsRecorder) -> Self {
        Self {
            start: Instant::now(),
            metrics,
        }
    }
}

impl Drop for DriveTimer<'_> {
    fn drop(&mut self) {
        self.metrics.record_dispatch_drive(self.start.elapsed());
        self.metrics.record_dispatch_in_flight(-1);
    }
}

/// Decide whether a reclaimed lease will actually execute and preserve the
/// Run's current committed disposition in its reschedule receipt.
///
/// A crash after an Awaiting/Ended commit but before queue settlement is a
/// settlement-repair window, not automatically another execution attempt.
/// Scheduled waits and an Awaiting ticket with its exact delivered reply do
/// execute; an unmatched ticket is simply returned to Awaiting.
fn recovered_attempt_disposition(
    recovered: bool,
    run_id: &RunId,
    state: Option<&RunState>,
    ticket: Option<awaken_agent_contract::agent::awaiting::ResumeTicket>,
    pending: &[PendingInput],
) -> Result<Option<RunDisposition>, Error> {
    if !recovered || matches!(state, Some(RunState::Ended(_))) {
        return Ok(None);
    }
    match (state, ticket) {
        (Some(RunState::Awaiting), Some(ticket)) => {
            let will_execute = ticket.reason() == AwaitReason::ScheduledAction
                || pending
                    .iter()
                    .any(|input| input.correlation_id == ticket.correlation_id);
            Ok(will_execute.then(|| RunDisposition::awaiting(ticket)))
        }
        (Some(RunState::Awaiting), None) => {
            Err(Error::Dispatch(crate::DispatchError::Rejected(format!(
                "recovered Awaiting Run {} has no committed resume ticket",
                run_id.0
            ))))
        }
        (Some(RunState::Running) | None, None) => Ok(Some(RunDisposition::running(run_id.clone()))),
        (Some(RunState::Running) | None, Some(_)) => {
            Err(Error::Dispatch(crate::DispatchError::Rejected(format!(
                "recovered non-Awaiting Run {} has a committed resume ticket",
                run_id.0
            ))))
        }
        (Some(RunState::Ended(_)), _) => unreachable!("terminal returned above"),
    }
}

/// Map a settled executor state to the dispatch outcome the worker commits.
///
/// `execute`/`resume` only ever return an awaiting or ended state; `Running` exists
/// as durable mid-flight truth, never as an executor result. A `Running` result
/// therefore signals a broken executor, and the worker fails loudly rather than
/// settle a live run to a terminal (`Done`) or awaiting outcome — committed truth,
/// not a bogus return, decides a run's fate (G1/G13).
fn settle_outcome(state: &RunState) -> Result<DispatchOutcome, Error> {
    match state {
        RunState::Awaiting => Ok(DispatchOutcome::Awaiting),
        RunState::Ended(_) => Ok(DispatchOutcome::Done),
        RunState::Running => Err(Error::Execution(
            awaken_runtime_contract::execution::Error::Execution(
                "executor returned a non-settled Running state".to_string(),
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    include!("worker/tests.rs");
}
