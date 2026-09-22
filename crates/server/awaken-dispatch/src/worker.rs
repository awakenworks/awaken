//! The dispatch worker: claim one runnable dispatch, run it, settle it.
//!
//! The worker is the only place that converts a durable dispatch into a Runtime
//! attempt. It is additive over runtime control (G6): it never reaches into the
//! loop, it calls the same `RunExecutor`/`Runtime::resume` a direct caller would,
//! and it decides execute-vs-resume from *committed truth* — the awaiting ticket
//! and the run record — not from a duplicated status in the queue. That keeps the
//! DispatchQueue aggregate free of run-outcome truth.

use awaken_dispatch_contract::ControlDispatch;
use std::sync::Arc;
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
use awaken_runtime_contract::execution::{AttemptRecoveryMode, RunAttemptExecutor};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::{
    AttemptOwnershipError, AttemptOwnershipVerifier, RuntimeRunContext,
};
use awaken_runtime_contract::{
    AttemptCredentialBinding, AttemptCredentialDelivery, AttemptCredentialDeliveryRequest,
    AttemptCredentialDeliverySource, AttemptCredentialRealization, CredentialRealizationReceipt,
    CredentialRealizationRecordError, CredentialRealizationRecorder,
};
use awaken_session_contract::{SessionRunActivityAdmission, SessionRunActivityAdmissionMode};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::Error;
use crate::clock::Clock;
use crate::commit_fence::{ClaimedCommitCoordinator, ClaimedRunCommit, GuardedRunCommit};
use crate::dispatch::{
    Claimed, DispatchOutcome, PendingInput, RunClaim, SessionRunReservationResolution,
    SettleOutcome,
};
use crate::execution_capacity::ExecutionCapacity;
use crate::worker_context::WorkerContext;

mod claim_renewal;
mod physical_attempt;
mod terminal_recovery;

use physical_attempt::{PhysicalAttemptOutcome, PhysicalAttemptRequest};

pub(crate) use claim_renewal::{
    ClaimLeaseRenewal, combine_attempt_cancellation, renew_claim_while_active,
};

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
    terminal_scan: crate::TerminalReconciliationScan,
    settlement_observer: Option<Arc<dyn crate::DispatchSettlementObserver>>,
    session_admission_observer: Option<Arc<dyn crate::SessionRunAdmissionObserver>>,
    owner: String,
    execution_binding: Option<crate::dispatch::AttemptExecutionBinding>,
    lease_ms: u64,
    cancellation: Option<CancellationToken>,
    local_credential_capabilities: awaken_runtime_contract::CredentialRealizationCapabilities,
    worker_credential_resolver:
        Option<Arc<dyn awaken_runtime_contract::WorkerLocalCredentialResolver>>,
}

struct ClaimBoundOwnershipVerifier {
    dispatch: Arc<dyn crate::DispatchQueue>,
    claim: RunClaim,
}

struct ClaimBoundCredentialRecorder {
    dispatch: Arc<dyn crate::DispatchQueue>,
    claim: RunClaim,
}

struct ClaimBoundCredentialDeliveryRefresh {
    dispatch: Arc<dyn crate::DispatchQueue>,
    claim: RunClaim,
}

fn claim_credential_refresh_is_available(
    binding_count: usize,
    delivery_count: usize,
    transport_available: bool,
) -> bool {
    binding_count > 0 && delivery_count == binding_count && transport_available
}

/// Process-local proof that this drive selected its committed read authority
/// for the exact claim before any physical executor can be entered. It carries
/// no second lifecycle state: the durable claim and recovery source remain the
/// authorities, while this value makes their required call ordering structural.
struct ClaimedSnapshotAuthority {
    run_id: RunId,
    thread_id: ThreadId,
    epoch: u64,
}

#[must_use]
const fn claimed_snapshot_execution_admission(
    run_matches: bool,
    thread_matches: bool,
    epoch_matches: bool,
) -> bool {
    run_matches && thread_matches && epoch_matches
}

impl ClaimedSnapshotAuthority {
    fn installed(claim: &RunClaim, thread_id: &ThreadId) -> Self {
        Self {
            run_id: claim.run_id.clone(),
            thread_id: thread_id.clone(),
            epoch: claim.epoch,
        }
    }

    fn admits(&self, claim: &RunClaim, thread_id: &ThreadId) -> bool {
        claimed_snapshot_execution_admission(
            self.run_id == claim.run_id,
            self.thread_id == *thread_id,
            self.epoch == claim.epoch,
        )
    }
}

/// Select whether the executor may prove that this claim can continue one
/// already-committed remote operation. A recovered execution claim and a fresh
/// durable-cancellation claim both inherit an earlier physical responsibility;
/// ordinary fresh execution must not inspect continuation state.
fn select_physical_attempt_recovery(
    recovered: bool,
    cancellation_requested: bool,
    continuation: impl FnOnce() -> AttemptRecoveryMode,
) -> AttemptRecoveryMode {
    if recovered || cancellation_requested {
        continuation()
    } else {
        AttemptRecoveryMode::RequirePredecessorQuiescence
    }
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

#[async_trait::async_trait]
impl AttemptCredentialDeliverySource for ClaimBoundCredentialDeliveryRefresh {
    async fn deliver(
        &self,
        request: AttemptCredentialDeliveryRequest,
    ) -> Result<AttemptCredentialDelivery, String> {
        self.dispatch
            .claimed_credential_delivery_transport()
            .ok_or_else(|| "claim credential delivery transport is unavailable".to_string())?
            .refresh_credential_delivery(&self.claim, request)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "dispatch claim was superseded during credential delivery".to_string())
    }
}

/// Build the neutral ownership verifier for one exact dispatch claim.
///
/// Runtime hosts use this before Session construction; [`DispatchWorker`] uses
/// the same adapter during attempt execution. Keeping construction here avoids a
/// second claim interpretation in an embedding application. It used to take a
/// clock for the same reason, and no longer needs one: liveness is judged by the
/// store that owns the row, so there is no second clock for an embedder to
/// supply and nothing left to disagree with.
pub fn claim_bound_ownership_verifier(
    dispatch: Arc<dyn crate::DispatchQueue>,
    claim: RunClaim,
) -> Arc<dyn AttemptOwnershipVerifier> {
    Arc::new(ClaimBoundOwnershipVerifier { dispatch, claim })
}

#[async_trait::async_trait]
impl AttemptOwnershipVerifier for ClaimBoundOwnershipVerifier {
    async fn verify_current(&self) -> Result<(), AttemptOwnershipError> {
        match self.dispatch.claim_is_current(&self.claim).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(AttemptOwnershipError::Lost),
            Err(error) => Err(AttemptOwnershipError::Unavailable(error.to_string())),
        }
    }
}

impl<S: ControlDispatch + 'static> DispatchWorker<S> {
    /// Rebuild the sole durable causal scope from the exact claimed instruction.
    /// Every claim entry uses this helper so recovery, pre-execution failure, and
    /// ordinary execution cannot drift into separate trace carrier rules.
    fn claimed_dispatch_span(claimed: &Claimed) -> tracing::Span {
        awaken_observability::dispatch_span(claimed.request.traceparent.as_deref())
    }

    fn renew_claim_while_driving(&self, claim: &RunClaim) -> ClaimLeaseRenewal {
        renew_claim_while_active(self.store.clone(), claim, self.lease_ms)
    }

    async fn install_claimed_recovery_projection(
        &self,
        claimed: &Claimed,
        thread_id: &ThreadId,
    ) -> Result<ClaimedSnapshotAuthority, Error> {
        let claim = RunClaim::from(&claimed.lease);
        if let Some(projection) = &self.recovery_projection {
            let snapshot = match &self.recovery_source {
                Some(source) => source
                    .recovery_snapshot(thread_id, &claim.run_id)
                    .await
                    .map_err(|error| {
                        crate::Error::Dispatch(crate::DispatchError::Rejected(error.to_string()))
                    })?,
                None => {
                    self.store
                        .claimed_checkpoint_transport()
                        .ok_or_else(|| {
                            crate::Error::Dispatch(crate::DispatchError::Rejected(
                                "recovery projection has neither a local source nor a claimed remote transport".into(),
                            ))
                        })?
                        .load_recovery_snapshot(&claim, claimed.session_work_authority.as_ref())
                        .await?
                }
            };
            projection
                .install(&claim.run_id, thread_id, snapshot)
                .map_err(|error| {
                    crate::Error::Dispatch(crate::DispatchError::Rejected(error.to_string()))
                })?;
        }
        Ok(ClaimedSnapshotAuthority::installed(&claim, thread_id))
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
        projection
            .install(run_id, thread_id, snapshot)
            .map_err(|error| {
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
    async fn commit_recovered_attempt(&self, claimed: &Claimed) -> Result<(), Error> {
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
            &[],
            None,
        )?;
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
            terminal_scan: Default::default(),
            settlement_observer: None,
            session_admission_observer: None,
            owner: owner.into(),
            execution_binding: None,
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

    /// Install a coordinate obtained by trusted execution-provider composition,
    /// never by an Agent or a caller-supplied stop receipt. Admission persists
    /// it atomically and verifies the exact registered assignment before entry.
    /// Transports without a trusted binding channel reject Some, not downgrade.
    #[must_use]
    pub fn with_execution_binding(
        mut self,
        binding: crate::dispatch::AttemptExecutionBinding,
    ) -> Self {
        self.execution_binding = Some(binding);
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

    #[must_use]
    pub fn with_session_run_admission_observer(
        mut self,
        observer: Arc<dyn crate::SessionRunAdmissionObserver>,
    ) -> Self {
        self.session_admission_observer = Some(observer);
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

    #[cfg(feature = "test-support")]
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
        credential_deliveries: &[AttemptCredentialDelivery],
        attempt_cancellation: Option<&CancellationToken>,
    ) -> Result<RuntimeRunContext, Error> {
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
        let ownership = claim_bound_ownership_verifier(dispatch.clone(), claim.clone());
        let mut ctx = self
            .exec
            .runtime_context(
                attempt_cancellation
                    .cloned()
                    .or_else(|| self.cancellation.clone())
                    .unwrap_or_default(),
                Some(claim),
            )
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
            let mut realization = AttemptCredentialRealization::new_with_deliveries(
                credential_bindings.to_vec(),
                credential_deliveries.to_vec(),
                Arc::new(ClaimBoundCredentialRecorder {
                    dispatch: dispatch.clone(),
                    claim: claim.clone(),
                }),
            )
            .map_err(|error| {
                Error::Execution(awaken_runtime_contract::execution::Error::Execution(
                    format!("claimed credential delivery validation failed: {error}"),
                ))
            })?;
            if claim_credential_refresh_is_available(
                credential_bindings.len(),
                credential_deliveries.len(),
                dispatch.claimed_credential_delivery_transport().is_some(),
            ) {
                realization = realization.with_delivery_refresh(Arc::new(
                    ClaimBoundCredentialDeliveryRefresh {
                        dispatch: dispatch.clone(),
                        claim: claim.clone(),
                    },
                ));
            }
            ctx = ctx.with_credential_realization(realization);
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
        Ok(match model_executor {
            Some(exec) => ctx.with_model_executor(exec.clone()),
            None => ctx,
        })
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
    pub async fn claim_one(&self) -> Result<Option<Claimed>, Error> {
        Ok(self
            .store
            .claim(
                &self.owner,
                self.lease_ms,
                &self.local_credential_capabilities,
            )
            .await?)
    }

    /// Claim one known Run under the same lease/fence policy without taking work
    /// belonging to another session. Parent-mediated child execution uses this
    /// after scheduling the child's stable identity.
    pub async fn claim_run(&self, run_id: &RunId) -> Result<Option<Claimed>, Error> {
        Ok(self
            .store
            .claim_run(
                run_id,
                &self.owner,
                self.lease_ms,
                &self.local_credential_capabilities,
            )
            .await?)
    }

    /// Atomically admit, claim, and drive a newly created Run. This closes the
    /// enqueue/claim race with the process pool while preserving the same lease,
    /// fencing, recovery, and settlement path as every other dispatch.
    pub async fn start_run(
        &self,
        request: awaken_dispatch_contract::RunDispatch,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let unclaimed_request = request.clone();
        let claimed = self
            .store
            .claim_new_run(
                request,
                &self.owner,
                self.lease_ms,
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
        let claimed = self
            .store
            .deliver_and_claim(
                input,
                &self.owner,
                self.lease_ms,
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
        let Some(claimed) = self.claim_one().await? else {
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
        let Some(claimed) = self.claim_run(run_id).await? else {
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
        let lease_renewal = self.renew_claim_while_driving(&claim);
        self.drive_claimed_with_renewal(claimed, clock, lease_renewal, None)
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
        execution_capacity: Option<ExecutionCapacity>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        if claimed.session_activity_admission_required {
            return self
                .drive_claimed_inner(claimed, clock, lease_renewal, execution_capacity)
                .await;
        }

        // One executable claim owns one durable trace relay. Keep execution,
        // committed-terminal replay, observer delivery, and fenced settlement
        // inside the same `wake.dispatch` span; fragmenting the span around only
        // the executor future disconnects post-commit background effects.
        let dispatch = Self::claimed_dispatch_span(&claimed);
        self.drive_claimed_inner(claimed, clock, lease_renewal, execution_capacity)
            .instrument(dispatch)
            .await
    }

    async fn drive_claimed_inner(
        &self,
        claimed: Claimed,
        clock: Arc<dyn Clock>,
        lease_renewal: ClaimLeaseRenewal,
        execution_capacity: Option<ExecutionCapacity>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let (attempt_cancellation, _cancellation_relay) = combine_attempt_cancellation(
            lease_renewal.attempt_cancellation(),
            self.cancellation.as_ref(),
        );
        // One drive receives one edge-owned clock. Claim eligibility, lease
        // renewal, pre-effect ownership checks and settlement fencing no longer
        // read it: the store that owns the row judges those against its own
        // clock. What remains here is the attempt's own reading of now —
        // credential-binding expiry revalidation and the due time of a committed
        // ScheduledAction — and both must come from this one source so a drive
        // never mixes two timelines.
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
        // E1=delete unstarted reserved dispatch; E2=bind epoch and publish ordinary Pending;
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
            let observer = self.session_admission_observer.as_ref();
            let observer_owns_fence = self
                .settlement_observer
                .as_ref()
                .is_some_and(|observer| observer.owns_dispatch_fence());
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
        if let Ok(Some(depth)) = self.store.runnable_depth().await {
            self.runtime.metrics().record_dispatch_queue_depth(depth);
        }
        let claimed_snapshot = self
            .install_claimed_recovery_projection(&claimed, &thread_id)
            .await?;
        let mut all_pending: Vec<String> = claimed
            .pending
            .iter()
            .map(|p| p.message_id.clone())
            .collect();

        // Committed terminal truth dominates every backend-specific path,
        // including a cancellation claim. A pool drainer may win the claim race
        // against the dedicated terminal reconciler after a commit/settle crash;
        // it must reuse the same terminal settlement owner before entering a
        // physical cancel, model, credential, or Environment effect.
        if let Some(settlement) = self
            .settle_from_committed_terminal(&claimed, &mut all_pending)
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
                )
                .await;
        }

        // Cancellation is itself a durable claimed attempt. It deliberately runs
        // before model/sandbox materialization. Only an authenticated remote A2A
        // task may consume credential material here, through the same exact
        // claim binding and receipt boundary as ordinary execution. Provider,
        // advisor, MCP and sandbox credentials remain outside terminal control.
        if claimed.cancellation_requested {
            awaken_dispatch_contract::verify_claim_credential_bindings(
                &claimed.request,
                &self.local_credential_capabilities,
                claimed.lease.epoch,
                now_ms,
                awaken_dispatch_contract::ClaimCredentialOperation::Cancel,
                &claimed.credential_bindings,
            )
            .map_err(|error| {
                Error::Execution(awaken_runtime_contract::execution::Error::Execution(
                    format!("claimed cancellation credential binding revalidation failed: {error}"),
                ))
            })?;
            let attempt_executor = self.attempt_executor();
            let context = self.execution_context_with(
                &claim,
                claimed.request.execution_scope.as_ref(),
                &None,
                &claimed.credential_bindings,
                &claimed.credential_deliveries,
                Some(&attempt_cancellation),
            )?;
            let cancel_executor = attempt_executor.clone();
            let cancel_activation = activation.clone();
            let recovery_mode = select_physical_attempt_recovery(
                claimed.recovered,
                claimed.cancellation_requested,
                || attempt_executor.physical_attempt_recovery(&activation, &context),
            );
            let state = match self
                .run_physical_attempt(
                    PhysicalAttemptRequest {
                        claimed: &claimed,
                        claimed_snapshot: &claimed_snapshot,
                        context: &context,
                        live_input: awaken_runtime_contract::execution::LiveInput::None,
                        recovery_mode,
                        clock: clock.clone(),
                        execution_capacity: execution_capacity.as_ref(),
                    },
                    move |attempt_context| async move {
                        cancel_executor
                            .cancel(cancel_activation.clone(), attempt_context.clone())
                            .await?;
                        // Keep cancellation and its terminal writer inside the
                        // SAME physical Attempt. The writer restores committed
                        // uncertainty on this latch before the owner decides
                        // whether to ACK; a backend no-op cannot clear it early.
                        let ticket = self.reader.resume_ticket(&cancel_activation.run_id);
                        let awaiting_tool_interrupt = cancellation_uses_tool_interruption(
                            claimed.request.session_activity_epoch.is_some(),
                            self.reader.run_state(&cancel_activation.run_id).as_ref(),
                            ticket.as_ref(),
                        );
                        if awaiting_tool_interrupt {
                            self.runtime
                                .interrupt_awaiting_tools(
                                    cancel_activation.run_id,
                                    cancel_activation.thread_id,
                                    attempt_context,
                                )
                                .await
                        } else {
                            self.runtime
                                .cancel_activation(cancel_activation, attempt_context)
                                .await
                        }
                    },
                )
                .await?
            {
                PhysicalAttemptOutcome::Returned(Ok(state)) => state,
                PhysicalAttemptOutcome::Returned(Err(error)) => {
                    return self
                        .settle_if_terminal_or_raise(&claimed, &all_pending, error)
                        .await;
                }
                PhysicalAttemptOutcome::PredecessorUnconfirmed => {
                    return self
                        .end_claimed_before_execution(
                            &claimed,
                            awaken_agent_contract::agent::run::EndCause::Indeterminate,
                        )
                        .await;
                }
                PhysicalAttemptOutcome::Deferred => return Ok(None),
            };
            if matches!(&state, RunState::Ended(_)) {
                self.redeliver_terminal_observers(&run_id, &thread_id)
                    .await?;
            }
            return Ok(self
                .settle(&claimed, Some(&state), DispatchOutcome::Done, &all_pending)
                .await?
                .applied()
                .then_some((run_id, state)));
        }

        // Recompute the exact binding set at the Worker boundary from the
        // immutable dispatch, current claim epoch, and this Worker's installed
        // capability evidence. A stale, substituted, widened, or reordered
        // wire projection cannot reach model materialization. The compiler is
        // the same authority used inside each queue claim transaction.
        awaken_dispatch_contract::verify_attempt_credential_bindings(
            &claimed.request,
            &self.local_credential_capabilities,
            claimed.lease.epoch,
            now_ms,
            &claimed.credential_bindings,
        )
        .map_err(|error| {
            Error::Execution(awaken_runtime_contract::execution::Error::Execution(
                format!("claimed credential binding revalidation failed: {error}"),
            ))
        })?;

        // A native replacement attempt may execute only after its durable
        // reschedule receipt crossed the exact claim epoch. Committed terminal
        // recovery and opaque ACP terminalization returned above and therefore
        // never fabricate a retry edge.
        self.commit_recovered_attempt(&claimed).await?;

        let attempt_executor = self.attempt_executor();
        if !claimed.request.placement.required_credentials.is_empty() {
            let resolver = self.worker_credential_resolver.as_ref().ok_or_else(|| {
                Error::Execution(awaken_runtime_contract::execution::Error::Execution(
                    "worker-local credential resolver is not installed".to_string(),
                ))
            })?;
            let ownership = claim_bound_ownership_verifier(self.store.clone(), claim.clone());
            ownership.verify_current().await.map_err(|error| {
                Error::Execution(awaken_runtime_contract::execution::Error::Execution(
                    format!("dispatch ownership was lost before credential revalidation: {error}"),
                ))
            })?;
            for required in &claimed.request.placement.required_credentials {
                let expected = awaken_runtime_contract::CredentialRef {
                    id: required.id.clone(),
                    revision: required.revision,
                };
                let observation = resolver
                    .revalidate_worker_reference(&expected)
                    .await
                    .map_err(|error| {
                        Error::Execution(
                            awaken_runtime_contract::execution::Error::Execution(format!(
                                "worker-local credential {} revision {} failed use-time revalidation: {error}",
                                required.id, required.revision
                            )),
                        )
                    })?;
                awaken_runtime_contract::require_exact_available_worker_reference(
                    &expected,
                    observation,
                )
                .map_err(|error| {
                    Error::Execution(awaken_runtime_contract::execution::Error::Execution(
                        format!(
                            "worker-local credential {} revision {} returned invalid use-time evidence: {error}",
                            required.id, required.revision
                        ),
                    ))
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
            &claimed.credential_deliveries,
            Some(&attempt_cancellation),
        )?;
        let model_executor = self
            .exec
            .materialize_inference(&claimed.request.activation, &execution_context)?;
        if let Some(executor) = &model_executor {
            execution_context = execution_context.with_model_executor(executor.clone());
        }
        let state = match self.reader.resume_ticket(&run_id) {
            // A committed ScheduledAction (ADR-0020): the system performs the
            // deferred action, not waits for external input. This also covers a
            // crash recovery of a scheduled await (no pending input is expected).
            Some(ticket) if ticket.reason() == AwaitReason::ScheduledAction => {
                match self
                    .run_physical_attempt(
                        PhysicalAttemptRequest {
                            claimed: &claimed,
                            claimed_snapshot: &claimed_snapshot,
                            context: &execution_context,
                            live_input: awaken_runtime_contract::execution::LiveInput::None,
                            recovery_mode: AttemptRecoveryMode::RequirePredecessorQuiescence,
                            clock: clock.clone(),
                            execution_capacity: execution_capacity.as_ref(),
                        },
                        |attempt_context| self.perform_scheduled(&claim, now_ms, attempt_context),
                    )
                    .await?
                {
                    PhysicalAttemptOutcome::Returned(Ok(state)) => state,
                    PhysicalAttemptOutcome::PredecessorUnconfirmed => {
                        return self
                            .end_claimed_before_execution(
                                &claimed,
                                awaken_agent_contract::agent::run::EndCause::Indeterminate,
                            )
                            .await;
                    }
                    PhysicalAttemptOutcome::Deferred => return Ok(None),
                    PhysicalAttemptOutcome::Returned(Err(err)) => {
                        return self
                            .settle_if_terminal_or_raise(&claimed, &all_pending, err)
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
                    .find(|input| {
                        pending_input_admission(false, Some(&ticket), input)
                            == PendingInputAdmission::Deliver
                    })
                    .cloned();
                match matched {
                    Some(input) => {
                        let command = ResumeCommand::from_ticket(&ticket, input.result, now_ms)
                            .with_operation_id(input.message_id.clone())
                            .with_context_messages(input.context_messages);
                        let resume_executor = attempt_executor.clone();
                        let resume_activation = activation.clone();
                        match self
                            .run_physical_attempt(
                                PhysicalAttemptRequest {
                                    claimed: &claimed,
                                    claimed_snapshot: &claimed_snapshot,
                                    context: &execution_context,
                                    live_input: attempt_executor.capabilities().live_input,
                                    recovery_mode:
                                        AttemptRecoveryMode::RequirePredecessorQuiescence,
                                    clock: clock.clone(),
                                    execution_capacity: execution_capacity.as_ref(),
                                },
                                move |attempt_context| async move {
                                    resume_executor
                                        .resume(resume_activation, command, attempt_context)
                                        .await
                                },
                            )
                            .await?
                        {
                            PhysicalAttemptOutcome::Returned(Ok(state)) => state,
                            PhysicalAttemptOutcome::PredecessorUnconfirmed => {
                                return self
                                    .end_claimed_before_execution(
                                        &claimed,
                                        awaken_agent_contract::agent::run::EndCause::Indeterminate,
                                    )
                                    .await;
                            }
                            PhysicalAttemptOutcome::Deferred => return Ok(None),
                            PhysicalAttemptOutcome::Returned(Err(err)) => {
                                return self
                                    .settle_if_terminal_or_raise(&claimed, &all_pending, err)
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
                .settle_from_committed_terminal(&claimed, &mut all_pending)
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
                        .filter(|input| {
                            input.run_id.0.is_empty()
                                && pending_input_admission(false, None, input)
                                    == PendingInputAdmission::Deliver
                        })
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
                        .filter(|input| {
                            pending_input_admission(false, None, input)
                                == PendingInputAdmission::Deliver
                        })
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
                    let recovery_mode = select_physical_attempt_recovery(
                        claimed.recovered,
                        claimed.cancellation_requested,
                        || {
                            attempt_executor
                                .physical_attempt_recovery(&activation, &execution_context)
                        },
                    );
                    let execute_executor = attempt_executor.clone();
                    match self
                        .run_physical_attempt(
                            PhysicalAttemptRequest {
                                claimed: &claimed,
                                claimed_snapshot: &claimed_snapshot,
                                context: &execution_context,
                                live_input: attempt_executor.capabilities().live_input,
                                recovery_mode,
                                clock: clock.clone(),
                                execution_capacity: execution_capacity.as_ref(),
                            },
                            move |attempt_context| async move {
                                execute_executor.execute(activation, attempt_context).await
                            },
                        )
                        .await?
                    {
                        PhysicalAttemptOutcome::Returned(Ok(state)) => state,
                        PhysicalAttemptOutcome::PredecessorUnconfirmed => {
                            return self
                                .end_claimed_before_execution(
                                    &claimed,
                                    awaken_agent_contract::agent::run::EndCause::Indeterminate,
                                )
                                .await;
                        }
                        PhysicalAttemptOutcome::Deferred => return Ok(None),
                        PhysicalAttemptOutcome::Returned(Err(err)) => {
                            return self
                                .settle_if_terminal_or_raise(&claimed, &all_pending, err)
                                .await;
                        }
                    }
                }
            },
        };

        let outcome = settle_outcome(&state)?;
        if matches!(&state, RunState::Ended(_)) {
            self.redeliver_terminal_observers(&run_id, &thread_id)
                .await?;
        }
        Ok(self
            .settle(&claimed, Some(&state), outcome, &all_pending)
            .await?
            .applied()
            .then_some((run_id, state)))
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
    ) -> Result<SettleOutcome, Error> {
        let run_id = claimed.request.run_id();
        let claim = RunClaim::from(&claimed.lease);
        let mut coordinated_committed_state = None;
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
            if !self.store.claim_is_current(&claim).await? {
                true
            } else {
                let observer = self.settlement_observer.as_ref().ok_or_else(|| {
                    Error::Dispatch(crate::DispatchError::Rejected(
                        "coordinated child settlement observer is not installed".to_string(),
                    ))
                })?;
                // Pre-settlement Session effects need the same physical gate as
                // the queue CAS. A remote observer checks its authority-side
                // guard; a local observer consumes the colocated locked facts.
                let guard = if observer.owns_dispatch_fence() {
                    None
                } else {
                    let Some(guard) = self.store.lock_commit_epoch(&claim).await? else {
                        return Ok(SettleOutcome::Fenced);
                    };
                    if !guard.permits_settlement() {
                        guard.release().await.map_err(|error| {
                            Error::Dispatch(crate::DispatchError::Unavailable(error))
                        })?;
                        return Ok(SettleOutcome::Fenced);
                    }
                    Some(guard)
                };
                // Do not hold Dispatch's lock across a Session application
                // callback: report/interrupt handling may re-enter Dispatch.
                // This is the same release boundary as the remote coordinator.
                // The callback is exact/idempotent and final queue CAS still
                // rejects a superseding epoch; logical Ended cannot restart.
                if let Some(guard) = guard {
                    guard.release().await.map_err(|error| {
                        Error::Dispatch(crate::DispatchError::Unavailable(error))
                    })?;
                }
                let observed = observer
                    .before_settle(
                        &claimed.request,
                        &claim,
                        &committed,
                        claimed.cancellation_requested,
                    )
                    .await;
                observed.map_err(|error| Error::SettlementObservationPending(error.to_string()))?;
                coordinated_committed_state = Some(committed);
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
                if result == SettleOutcome::Applied
                    && let Some(committed) = coordinated_committed_state.as_ref()
                {
                    self.settlement_observer
                        .as_ref()
                        .expect("coordinated settlement installed its observer")
                        .after_settle(
                            &claimed.request,
                            &claim,
                            committed,
                            claimed.cancellation_requested,
                        )
                        .await
                        .map_err(|error| {
                            Error::Dispatch(crate::DispatchError::Rejected(error.to_string()))
                        })?;
                }
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
                || pending.iter().any(|input| {
                    pending_input_admission(false, Some(&ticket), input)
                        == PendingInputAdmission::Deliver
                });
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingInputAdmission {
    Deliver,
    ConsumedByCommittedTruth,
}

#[must_use]
const fn classify_pending_input_admission(
    committed_terminal: bool,
    committed_ticket_exists: bool,
    correlation_matches_ticket: bool,
    input_is_unbound: bool,
) -> PendingInputAdmission {
    if committed_terminal {
        PendingInputAdmission::ConsumedByCommittedTruth
    } else if committed_ticket_exists {
        if correlation_matches_ticket {
            PendingInputAdmission::Deliver
        } else {
            PendingInputAdmission::ConsumedByCommittedTruth
        }
    } else if input_is_unbound {
        PendingInputAdmission::Deliver
    } else {
        PendingInputAdmission::ConsumedByCommittedTruth
    }
}

/// One committed-truth rule shared by fresh execution, awaiting resume and
/// recovery. A bound signal remains deliverable only while the current ticket
/// retains its exact correlation; a commit that ends the Run or advances the
/// ticket makes the old signal a settlement-cleanup concern, never executable
/// input. Unbound input is admitted only to a fresh Run.
fn pending_input_admission(
    committed_terminal: bool,
    ticket: Option<&awaken_agent_contract::agent::awaiting::ResumeTicket>,
    input: &PendingInput,
) -> PendingInputAdmission {
    classify_pending_input_admission(
        committed_terminal,
        ticket.is_some(),
        ticket.is_some_and(|ticket| ticket.correlation_id == input.correlation_id),
        input.correlation_id.is_empty(),
    )
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

#[cfg(kani)]
#[kani::proof]
fn pending_signal_is_deliverable_only_without_consuming_commit() {
    // Causes: C1 committed truth is terminal; C2 it retains an awaiting ticket;
    // C3 the signal matches that exact ticket; C4 the input is unbound. Effects:
    // E1 deliver only for a nonterminal exact ticket match or a nonterminal fresh
    // unbound input; E2 every terminal, stale-correlation, and orphan-bound signal
    // is consumed logically by committed truth and can only be deleted by the
    // existing settlement transaction. Rules: R1=C1=>E2; R2=!C1+C2+C3=>E1;
    // R3=!C1+C2+!C3=>E2; R4=!C1+!C2+C4=>E1;
    // R5=!C1+!C2+!C4=>E2. C4 is irrelevant while C2 is true.
    let terminal: bool = kani::any();
    let ticket: bool = kani::any();
    let correlation_matches: bool = kani::any();
    let unbound: bool = kani::any();
    let disposition =
        classify_pending_input_admission(terminal, ticket, correlation_matches, unbound);
    assert_eq!(
        disposition == PendingInputAdmission::Deliver,
        !terminal && ((ticket && correlation_matches) || (!ticket && unbound))
    );
}

#[cfg(kani)]
#[kani::proof]
fn claimed_snapshot_execution_requires_exact_identity() {
    // Causes: C1 Run identity matches; C2 Thread identity matches; C3 claim
    // epoch matches. Effect E1 permits the already claim-fenced physical
    // executor boundary. Every other combination is E2 rejection. Decision
    // rule R1=C1+C2+C3=>E1; the seven complementary Boolean combinations all
    // imply E2. Token construction remains private to the successful recovery
    // installation path, so no caller can bypass this production reducer.
    let run_matches: bool = kani::any();
    let thread_matches: bool = kani::any();
    let epoch_matches: bool = kani::any();
    assert_eq!(
        claimed_snapshot_execution_admission(run_matches, thread_matches, epoch_matches),
        run_matches && thread_matches && epoch_matches
    );
}

#[cfg(test)]
mod tests {
    include!("worker/tests.rs");
}
