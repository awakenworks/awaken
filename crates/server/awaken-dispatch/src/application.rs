// @formal entrypoint ingress.commit_epoch_atomic_remote_custom
// @formal entrypoint remote_worker.committed_truth_is_coordinator_owned
// @formal entrypoint sandbox.k8s_process_observation_requires_complete_exit
// @formal evidence session_execution.one_physical_attempt_per_thread
//! Protocol-neutral application ports used by Coordinator HTTP adapters.

use awaken_session_execution_contract::ApplicationError;
use std::sync::Arc;

use awaken_agent_contract::thread::commit::coordinator::OperationCoordinator;
use awaken_agent_contract::thread::commit::operation::{
    CommitReceipt, require_commit_operation_payload_hash,
};

use crate::ClaimedCommitRequest;
use awaken_dispatch_execution::validate_commit_dispatch_binding;

/// Trusted Coordinator composition resolves an exact running execution domain
/// before physical admission. Worker request JSON is never an implementation.
/// Implementations must verify provider provenance and the full incarnation;
/// returning a coordinate is not a stop receipt or permission to clear a slot.
/// Lookup is read-only and retryable: it must not launch or stop execution.
#[async_trait::async_trait]
pub trait WorkerExecutionBindingSource: Send + Sync {
    /// Deny a fresh, provably unbound bootstrap through its original provider
    /// ledger. This returns that original anchor only; it is not Registry
    /// absence, process-exit evidence or Session cleanup acknowledgement.
    /// A provider may retire that exact unadmitted process domain after its
    /// absorbing original-ledger CAS excludes first enrollment and every
    /// Create ticket. Failures retain the seal for the same exact replay.
    async fn seal_unattempted_worker(
        &self,
        _worker_id: &str,
        _expected: &crate::WorkerCreateLedger,
    ) -> Result<crate::WorkerCreateLedger, ApplicationError> {
        Err(ApplicationError::unavailable(
            "Worker bootstrap sealing is unavailable",
        ))
    }
    /// Prepare provider-owned physical evidence before the Registry's first
    /// insert. The caller atomically freezes a Create ledger UID when present;
    /// the process binding must already be durable before any registration
    /// could become Ready. A retry may only return the same evidence.
    async fn prepare_registration_physical_evidence(
        &self,
        _registration: &crate::WorkerRegistration,
    ) -> Result<Option<crate::WorkerCreateLedger>, ApplicationError> {
        Ok(None)
    }
    /// Retain process metadata after durable registration and before Ready.
    /// It does not bind an attempt or grant execution authority.
    async fn retain_registered_worker_evidence(
        &self,
        _worker: &crate::WorkerIdentity,
        _create_ledger: Option<&crate::WorkerCreateLedger>,
    ) -> Result<(), ApplicationError> {
        Ok(())
    }
    async fn binding_for(
        &self,
        worker: &crate::WorkerIdentity,
    ) -> Result<crate::AttemptExecutionBinding, ApplicationError>;
}

/// Coordinator-authorized whole-Worker stop, not user cancellation of one Run.
/// Only the application below can construct it, after exact Registry closure.
/// It carries the original persisted binding, never a newly observed replacement.
///
/// ```compile_fail
/// use awaken_dispatch::{AttemptExecutionBinding, ClosedWorkerExecution};
/// fn forge(binding: AttemptExecutionBinding) {
///     let _ = ClosedWorkerExecution { binding };
/// }
/// ```
pub struct ClosedWorkerExecution {
    binding: crate::AttemptExecutionBinding,
    create_ledger: Option<crate::WorkerCreateLedger>,
}

impl ClosedWorkerExecution {
    pub fn binding(&self) -> &crate::AttemptExecutionBinding {
        &self.binding
    }

    /// Original Registry anchor, never a current-name lookup or a newly
    /// initialized replacement. Historical absence grants no Create closure.
    pub fn original_create_ledger(&self) -> Option<&crate::WorkerCreateLedger> {
        self.create_ledger.as_ref()
    }

    /// Trusted adapters call this only after observing exact local process exit.
    /// This provider reference supports audit/reobservation, not remote replay.
    pub fn observed_exit(
        &self,
        evidence_ref: impl Into<String>,
    ) -> Result<WorkerProcessStopStatus, ApplicationError> {
        let evidence_ref = evidence_ref.into();
        if evidence_ref.trim().is_empty() {
            return Err(ApplicationError::invalid(
                "process exit requires provider evidence",
            ));
        }
        Ok(WorkerProcessStopStatus::Exited(ObservedWorkerProcessExit {
            binding: self.binding.clone(),
            evidence_ref,
        }))
    }
}

/// Subject-bound LOCAL exit evidence. No wire deserialization and no conversion
/// to whole-attempt acknowledgement. The provider still owns the physical fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedWorkerProcessExit {
    binding: crate::AttemptExecutionBinding,
    evidence_ref: String,
}

impl ObservedWorkerProcessExit {
    pub fn binding(&self) -> &crate::AttemptExecutionBinding {
        &self.binding
    }
    pub fn evidence_ref(&self) -> &str {
        &self.evidence_ref
    }
}

/// Local process observation only. Neither value authorizes Dispatch retirement
/// or replay: independent model, MCP and Sandbox participants may remain active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerProcessStopStatus {
    Unknown,
    Exited(ObservedWorkerProcessExit),
}

/// One exact predecessor, after independent local exit and irreversible Worker
/// closure. Only the Coordinator application constructs this observation subject;
/// neither a Worker request nor a Run's logical terminal status can construct it.
///
/// ```compile_fail
/// use awaken_dispatch::{ClosedAttemptExecution, PhysicalAttempt, ObservedWorkerProcessExit};
/// fn forge(attempt: PhysicalAttempt, process_exit: ObservedWorkerProcessExit) {
///     let _ = ClosedAttemptExecution { attempt, process_exit };
/// }
/// ```
pub struct ClosedAttemptExecution {
    attempt: crate::PhysicalAttempt,
    process_exit: Option<ObservedWorkerProcessExit>,
}

impl ClosedAttemptExecution {
    pub fn attempt(&self) -> &crate::PhysicalAttempt {
        &self.attempt
    }

    pub fn process_exit(&self) -> Option<&ObservedWorkerProcessExit> {
        self.process_exit.as_ref()
    }

    pub fn local_evidence_ref(&self) -> String {
        self.process_exit.as_ref().map_or_else(
            || {
                format!(
                    "dispatch-entry-closed:{}:{}",
                    self.attempt.claim.run_id.0, self.attempt.claim.epoch
                )
            },
            |receipt| receipt.evidence_ref().to_owned(),
        )
    }

    /// Trusted provider assertion, NOT a test for local completion. Call only
    /// after proving complete pre-effect enrollment AND terminal/no-restart
    /// evidence for ALL work belonging to this exact attempt, including SDK
    /// retries, hooks, nested tools and independently hosted Sandboxes. Missing
    /// records, latest-value projections and a closed HTTP connection are not
    /// evidence of absence. Unsupported/legacy execution must remain Unknown.
    /// The reference identifies repeatably queryable provider evidence; it is
    /// not a signature and this constructor cannot verify provider honesty.
    pub fn observed_all_execution_stopped(
        &self,
        evidence_ref: impl Into<String>,
    ) -> Result<AttemptStopStatus, ApplicationError> {
        self.observed_closure(AttemptClosureKind::AllExecutionStopped, evidence_ref.into())
    }

    /// Prove closure of all governed effects after exact Worker closure, with
    /// complete pre-effect scope enrollment. Response-only provider computation
    /// may remain unknown; no business outcome or provider stop is fabricated.
    pub fn observed_execution_domain_closed(
        &self,
        evidence_ref: impl Into<String>,
    ) -> Result<AttemptStopStatus, ApplicationError> {
        self.observed_closure(
            AttemptClosureKind::ControlledExecutionClosed,
            evidence_ref.into(),
        )
    }
    fn observed_closure(
        &self,
        kind: AttemptClosureKind,
        evidence_ref: String,
    ) -> Result<AttemptStopStatus, ApplicationError> {
        if !self.attempt.closing || evidence_ref.trim().is_empty() {
            return Err(ApplicationError::invalid(
                "complete closure requires a durably closed entry and provider evidence",
            ));
        }
        let record = awaken_dispatch_contract::AttemptClosureRecord::try_new(
            self.attempt.clone(),
            kind,
            evidence_ref,
        )
        .map_err(|error| ApplicationError::invalid(error.to_string()))?;
        Ok(AttemptStopStatus::Stopped(Box::new(ObservedAttemptStop {
            record,
        })))
    }
}

pub use awaken_dispatch_contract::AttemptClosureKind;

/// Subject-bound assertion from the trusted stop source, never deserialized
/// from Worker/user JSON. Covers this physical attempt only, not Session cleanup,
/// business outcome, side-effect replay safety, or a successor's authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedAttemptStop {
    record: awaken_dispatch_contract::AttemptClosureRecord,
}

impl ObservedAttemptStop {
    pub fn kind(&self) -> AttemptClosureKind {
        self.record.kind()
    }
    pub fn attempt(&self) -> &crate::PhysicalAttempt {
        self.record.attempt()
    }

    pub fn evidence_ref(&self) -> &str {
        self.record.evidence_ref()
    }

    /// Retained data from this private trusted observation; never a grant
    /// reconstructed from Worker/user JSON.
    pub fn record(&self) -> &awaken_dispatch_contract::AttemptClosureRecord {
        &self.record
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptStopStatus {
    Unknown,
    Stopped(Box<ObservedAttemptStop>),
}

/// Trusted, independent infrastructure control, selected by Coordinator startup.
/// Implementations must validate the provider and exact frozen process identity,
/// stop idempotently and preserve evidence. Worker exit, complete execution
/// closure and Create admission remain distinct; a local-only implementation
/// must retain the default Unknown for complete execution below.
#[async_trait::async_trait]
pub trait WorkerExecutionStopSource: Send + Sync {
    /// Whether this provider owns retention for this exact incarnation.
    fn owns_worker_evidence(&self, _identity: &crate::WorkerIdentity) -> bool {
        false
    }
    /// Fence the closed incarnation's original provider Create admission.
    /// This independent operation supplies no process-exit or disposal receipt.
    /// Providers without a Create ledger have no additional owner to seal;
    /// a retained anchor requires an implementation rather than silent success.
    async fn seal_closed_worker_admission(
        &self,
        _identity: &crate::WorkerIdentity,
        create_ledger: Option<&crate::WorkerCreateLedger>,
    ) -> Result<(), ApplicationError> {
        if create_ledger.is_some() {
            Err(ApplicationError::unavailable(
                "original Worker Create admission sealing is unsupported",
            ))
        } else {
            Ok(())
        }
    }
    /// Observe one closed Worker using provider-retained pre-exit coordinates.
    /// With `may_stop_active == false`, an occupied or uninspected physical slot
    /// permits only natural exit observation and closure of that exited launch
    /// lifecycle. Active stop additionally requires the caller's closed-idle
    /// admission. Unsupported providers cannot manufacture a process-exit fact.
    async fn observe_closed_idle_worker(
        &self,
        _identity: &crate::WorkerIdentity,
        _may_stop_active: bool,
        _create_ledger: Option<&crate::WorkerCreateLedger>,
    ) -> Result<Option<crate::WorkerProcessExit>, ApplicationError> {
        Ok(None)
    }
    /// Retire an exact closed-idle provider domain and release its retention only
    /// after Coordinator proves no physical slots remain. This is not exit
    /// evidence and cannot settle an attempt or authorize Session replay.
    async fn release_closed_idle_worker_evidence(
        &self,
        _identity: &crate::WorkerIdentity,
        _original_create_ledger: Option<&crate::WorkerCreateLedger>,
    ) -> Result<(), ApplicationError> {
        Ok(())
    }
    async fn stop_closed_worker(
        &self,
        execution: &ClosedWorkerExecution,
    ) -> Result<WorkerProcessStopStatus, ApplicationError>;

    /// Release provider-retained process history only after the Registry has
    /// durably stored this exact exit receipt. Implementations must target the
    /// original binding idempotently; failure leaves Dispatch occupied and is
    /// retried through the same blocked-successor path.
    async fn release_closed_worker_evidence(
        &self,
        _execution: &ClosedWorkerExecution,
    ) -> Result<(), ApplicationError> {
        Ok(())
    }

    /// Close/observe the exact governed domains after durable local entry
    /// closure. The persisted closing phase prevents handoff and ordinary ACK
    /// from releasing them during IO. Adapters must target frozen bindings;
    /// absent enrollment/capability remains Unknown, with no initiating replay.
    async fn close_attempt_execution(
        &self,
        _execution: &ClosedAttemptExecution,
        _recovery: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
    ) -> Result<AttemptStopStatus, ApplicationError> {
        Ok(AttemptStopStatus::Unknown)
    }
}

/// The Coordinator's original implementation-isolation adapter. One selected
/// StopSource Arc implements the unchanged idle leaf and the explicit trusted
/// terminal role; no Host, reader, recovery source or closure state is retained.
pub struct WorkerEvidenceReleaseAdapter {
    source: Arc<dyn WorkerExecutionStopSource>,
}

impl WorkerEvidenceReleaseAdapter {
    #[must_use]
    pub fn new(source: Arc<dyn WorkerExecutionStopSource>) -> Self {
        Self { source }
    }
}

#[async_trait::async_trait]
impl awaken_dispatch_execution::WorkerEvidenceRelease for WorkerEvidenceReleaseAdapter {
    fn owns_worker_evidence(&self, identity: &crate::WorkerIdentity) -> bool {
        self.source.owns_worker_evidence(identity)
    }

    async fn release_closed_idle_worker_evidence(
        &self,
        dispatch: &dyn PhysicalAttemptReads,
        directory: &dyn crate::WorkerDirectory,
        identity: &crate::WorkerIdentity,
        now_ms: u64,
    ) -> Result<(), ApplicationError> {
        reconcile_closed_idle_worker_evidence(
            dispatch,
            directory,
            self.source.as_ref(),
            identity,
            now_ms,
        )
        .await
    }
}

#[async_trait::async_trait]
impl awaken_dispatch_execution::WorkerTerminalReconciliation for WorkerEvidenceReleaseAdapter {
    async fn reconcile_terminal_attempt(
        &self,
        dispatch: &awaken_dispatch_execution::AnyCoordinatorDispatch,
        directory: &dyn crate::WorkerDirectory,
        reader: &dyn awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView,
        summary: &crate::DispatchSummary,
        recovery: Option<&awaken_dispatch_execution::PhysicalAttemptRecoveryRead<'_>>,
        now_ms: u64,
    ) -> Result<bool, ApplicationError> {
        stop_committed_terminal_attempt(
            dispatch,
            Some((directory, self.source.as_ref())),
            recovery,
            reader,
            summary,
            now_ms,
        )
        .await
    }
}

/// Progress the existing closed-idle maintenance admission. Registry closure
/// applies before optional provider ownership; an unowned incarnation still
/// expires, but receives no physical observation or evidence-release action.
/// The HTTP/provider direct-release kernel below keeps its original purpose.
pub async fn reconcile_closed_idle_worker_evidence(
    dispatch: &dyn PhysicalAttemptReads,
    directory: &dyn crate::WorkerDirectory,
    source: &dyn WorkerExecutionStopSource,
    identity: &crate::WorkerIdentity,
    now_ms: u64,
) -> Result<(), ApplicationError> {
    if !ensure_worker_closed(directory, identity, now_ms).await?
        || !source.owns_worker_evidence(identity)
    {
        return Ok(());
    }
    release_closed_idle_worker_evidence(dispatch, directory, source, identity, now_ms).await
}

/// Retire only the original closed-idle domain admitted by Registry and the
/// physical-slot authority. HTTP withdrawal and maintenance share this operation;
/// no provider result here becomes a process-exit or Session cleanup receipt.
pub async fn release_closed_idle_worker_evidence(
    dispatch: &dyn PhysicalAttemptReads,
    directory: &dyn crate::WorkerDirectory,
    source: &dyn WorkerExecutionStopSource,
    identity: &crate::WorkerIdentity,
    now_ms: u64,
) -> Result<(), ApplicationError> {
    let Some(record) = directory
        .incarnation(identity)
        .await
        .map_err(|e| ApplicationError::unavailable(e.to_string()))?
    else {
        return Ok(());
    };
    if !matches!(
        record.snapshot.state,
        crate::WorkerState::Quiesced | crate::WorkerState::Dead
    ) {
        return Ok(());
    }
    // Exit is an infrastructure fact, independent of permission to release
    // retention. An occupied, inconsistent or unavailable slot scan must not
    // suppress a natural exit; it still blocks active stop and release below.
    // Both independent owners are attempted even when their peer fails. Exit
    // observation persists its fact before this join; a failed Create seal can
    // withhold retention release but cannot suppress that infrastructure fact.
    // Already-retained exits still retry the original Create seal.
    let (admission, exited) = tokio::join!(
        source.seal_closed_worker_admission(identity, record.create_ledger.as_ref()),
        async {
            if record.process_exit.is_some() {
                Ok(true)
            } else {
                observe_closed_worker_exit(
                    directory,
                    source,
                    identity,
                    record.create_ledger.as_ref(),
                    false,
                )
                .await
            }
        }
    );
    let exited = exited?;
    admission?;
    if has_active_physical_attempt_for_owner(dispatch, &identity.lease_owner()).await? {
        return Ok(());
    }
    // Deregistration can precede its own RPC response. Only an empty exact
    // slot scan plus the original Registry grace deadline permits stop.
    if !exited
        && (record.snapshot.expires_at_ms > now_ms
            || !observe_closed_worker_exit(
                directory,
                source,
                identity,
                record.create_ledger.as_ref(),
                true,
            )
            .await?)
    {
        return Ok(());
    }
    source
        .release_closed_idle_worker_evidence(identity, record.create_ledger.as_ref())
        .await
}

async fn observe_closed_worker_exit(
    directory: &dyn crate::WorkerDirectory,
    source: &dyn WorkerExecutionStopSource,
    identity: &crate::WorkerIdentity,
    create_ledger: Option<&crate::WorkerCreateLedger>,
    may_stop_active: bool,
) -> Result<bool, ApplicationError> {
    let Some(receipt) = source
        .observe_closed_idle_worker(identity, may_stop_active, create_ledger)
        .await?
    else {
        return Ok(false);
    };
    persist_worker_exit(
        directory,
        identity,
        receipt,
        ApplicationError::unavailable("idle Worker process exit could not be committed"),
        ApplicationError::unavailable("idle Worker process exit readback does not match"),
    )
    .await?;
    Ok(true)
}

/// One exact-incarnation receipt writer for occupied and idle observations.
/// Caller-specific errors retain their existing wire classification and text.
async fn persist_worker_exit(
    directory: &dyn crate::WorkerDirectory,
    identity: &crate::WorkerIdentity,
    receipt: crate::WorkerProcessExit,
    commit_failure: ApplicationError,
    readback_failure: ApplicationError,
) -> Result<(), ApplicationError> {
    receipt
        .validate()
        .map_err(|error| ApplicationError::internal(error.to_string()))?;
    if directory
        .record_process_exit(identity, receipt.clone())
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?
        != crate::RegistryMutation::Applied
    {
        return Err(commit_failure);
    }
    let committed = directory
        .incarnation(identity)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?
        .and_then(|record| record.process_exit);
    if committed.as_ref() != Some(&receipt) {
        return Err(readback_failure);
    }
    Ok(())
}

/// Read the original Dispatch physical-slot authority for a complete Worker
/// incarnation owner. The closed-idle release and Coordinator cutover
/// observation share this one scan. A concurrent or inconsistent read fails
/// closed; it never manufactures a process-exit or whole-attempt receipt.
pub async fn has_active_physical_attempt_for_owner(
    dispatch: &dyn PhysicalAttemptReads,
    owner: &str,
) -> Result<bool, ApplicationError> {
    for summary in dispatch
        .list_dispatches()
        .await
        .map_err(|e| ApplicationError::unavailable(e.to_string()))?
    {
        if summary.physical_attempt_active {
            let physical = dispatch
                .load_physical_attempt(&summary.run_id)
                .await
                .map_err(|e| ApplicationError::unavailable(e.to_string()))?;
            let physical = physical.ok_or_else(|| {
                ApplicationError::unavailable(
                    "Dispatch reported an active physical slot without its exact attempt",
                )
            })?;
            if physical.claim.owner == owner {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Progress a blocked successor through the existing authorities, without a
/// recovery queue or a second copy of slot state. The caller bounds provider IO
/// and retries through ordinary admission. `None` means no stop was authorized.
/// The returned observation remains LOCAL only. Retirement additionally requires
/// a separate complete assertion from `close_attempt_execution`; the existing
/// Dispatch slot is the sole retirement authority, not this return value.
pub async fn stop_expired_predecessor(
    dispatch: &dyn AttemptControlAuthority,
    directory: &dyn crate::WorkerDirectory,
    source: &dyn WorkerExecutionStopSource,
    recovery: Option<&awaken_dispatch_execution::PhysicalAttemptRecoveryRead<'_>>,
    successor: &crate::RunClaim,
    now_ms: u64,
) -> Result<Option<WorkerProcessStopStatus>, ApplicationError> {
    if !dispatch
        .claim_is_current(successor)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?
    {
        return Ok(None);
    }
    let Some(predecessor) = dispatch
        .load_physical_attempt(&successor.run_id)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?
    else {
        return Ok(None);
    };
    if !predecessor.closing && predecessor.claim.owner == successor.owner {
        return Ok(None);
    }
    close_original_attempt(dispatch, directory, source, recovery, predecessor, now_ms).await
}

/// Read trusted committed Thread truth and the current original physical slot
/// inside the one Ingress stop-admission owner. The returned bool permits only
/// attempting the existing terminal-recovery claim; it is not a closure receipt
/// or a Run execution grant. No caller-supplied terminal DTO grants a stop.
pub async fn stop_committed_terminal_attempt(
    dispatch: &dyn AttemptControlAuthority,
    evidence: Option<(&dyn crate::WorkerDirectory, &dyn WorkerExecutionStopSource)>,
    recovery: Option<&awaken_dispatch_execution::PhysicalAttemptRecoveryRead<'_>>,
    reader: &dyn awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView,
    summary: &crate::DispatchSummary,
    now_ms: u64,
) -> Result<bool, ApplicationError> {
    if !awaken_dispatch_execution::committed_terminal_ready(reader, summary)? {
        return Ok(false);
    }
    // Even a summary that says empty cannot hide a concurrent/unknown occupant.
    // Both directions of inconsistency are retryable facts, never vacancy.
    let original = dispatch
        .load_physical_attempt(&summary.run_id)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?;
    if original.is_some() != summary.physical_attempt_active {
        return Err(ApplicationError::unavailable(
            "terminal summary and original physical attempt disagree",
        ));
    }
    if let Some(original) = original {
        let Some(thread_id) = original.thread_id.as_ref() else {
            return Ok(false);
        };
        if original.claim.run_id != summary.run_id
            || *thread_id != summary.thread_id
            || original.session_thread_id != summary.session_thread_id
        {
            return Err(ApplicationError::invalid(
                "terminal scan and original physical attempt have different affinity",
            ));
        }
        let Some((directory, source)) = evidence else {
            return Ok(false);
        };
        if original.closing {
            let Some(binding) = original.binding.as_ref() else {
                return Ok(false);
            };
            binding
                .validate_for(&original.claim)
                .map_err(|error| ApplicationError::internal(error.to_string()))?;
            if !ensure_worker_closed(directory, binding.worker(), now_ms).await? {
                return Ok(false);
            }
        }
        close_original_attempt(dispatch, directory, source, recovery, original, now_ms).await?;
        if dispatch
            .load_physical_attempt(&summary.run_id)
            .await
            .map_err(|error| ApplicationError::unavailable(error.to_string()))?
            .is_some()
        {
            return Ok(false);
        }
    }
    // Quarantined and superseded delivery may lose original occupancy, but that
    // physical fact never requeues/settles them or permits ordinary execution.
    Ok(matches!(
        summary.state,
        crate::DispatchState::Pending
            | crate::DispatchState::Awaiting
            | crate::DispatchState::Leased
    ))
}

async fn close_original_attempt(
    dispatch: &dyn AttemptControlAuthority,
    directory: &dyn crate::WorkerDirectory,
    source: &dyn WorkerExecutionStopSource,
    recovery: Option<&awaken_dispatch_execution::PhysicalAttemptRecoveryRead<'_>>,
    mut predecessor: crate::PhysicalAttempt,
    now_ms: u64,
) -> Result<Option<WorkerProcessStopStatus>, ApplicationError> {
    let entry_was_closing = predecessor.closing;
    let Some(binding) = predecessor.binding.clone() else {
        return Ok(None);
    };
    binding
        .validate_for(&predecessor.claim)
        .map_err(|error| ApplicationError::internal(error.to_string()))?;
    let process_exit = if predecessor.closing {
        persisted_worker_exit(directory, &binding).await?
    } else {
        let observation =
            independently_close_worker(dispatch, directory, source, &predecessor, &binding, now_ms)
                .await?;
        let exit = match observation {
            Some(WorkerProcessStopStatus::Exited(exit)) => exit,
            Some(WorkerProcessStopStatus::Unknown) => {
                return Ok(Some(WorkerProcessStopStatus::Unknown));
            }
            None => return Ok(None),
        };
        if dispatch
            .close_attempt(&predecessor.claim)
            .await
            .map_err(|error| ApplicationError::unavailable(error.to_string()))?
            != crate::SettleOutcome::Applied
        {
            return Ok(Some(WorkerProcessStopStatus::Exited(exit)));
        }
        predecessor.closing = true;
        Some(exit)
    };
    let observation = process_exit
        .as_ref()
        .map_or(WorkerProcessStopStatus::Unknown, |exit| {
            WorkerProcessStopStatus::Exited(exit.clone())
        });
    if dispatch
        .load_physical_attempt(&predecessor.claim.run_id)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?
        .as_ref()
        != Some(&predecessor)
    {
        return Ok(Some(observation));
    }
    let scope_progress: Result<bool, ApplicationError> = async {
        let (Some(recovery), Some(thread_id)) = (recovery, predecessor.thread_id.as_ref()) else {
            return Ok(false);
        };
        let snapshot = recovery.snapshot_for(&predecessor).await?;
        if snapshot.thread_id != *thread_id || snapshot.claimed_run_id != predecessor.claim.run_id {
            return Err(ApplicationError::internal(
                "recovery source returned another physical attempt subject",
            ));
        }
        // Snapshot IO can race exact closure and a newly admitted successor.
        // Never issue the old stop after its physical identity has changed.
        if dispatch
            .load_physical_attempt(&predecessor.claim.run_id)
            .await
            .map_err(|error| ApplicationError::unavailable(error.to_string()))?
            .as_ref()
            != Some(&predecessor)
        {
            return Ok(false);
        }
        let stopped = source
            .close_attempt_execution(
                &ClosedAttemptExecution {
                    attempt: predecessor.clone(),
                    process_exit,
                },
                &snapshot,
            )
            .await?;
        if let AttemptStopStatus::Stopped(receipt) = &stopped {
            if receipt.attempt() != &predecessor {
                return Err(ApplicationError::internal(
                    "closure evidence names another physical attempt",
                ));
            }
            // Closing denies normal return/handoff during provider IO. Exact
            // closure cannot clear a successor's slot.
            let retirement = dispatch
                .finish_attempt_closure(receipt.record())
                .await
                .map_err(|error| ApplicationError::unavailable(error.to_string()))?;
            tracing::debug!(?retirement, kind = ?receipt.kind(), "independently closed physical attempt");
            return Ok(true);
        }
        Ok(false)
    }
    .await;
    if matches!(scope_progress, Ok(true)) {
        return Ok(Some(observation));
    }
    // Entry closure and an independent Worker exit are separate facts. A prior
    // close/crash must not suppress observing the original process. Reuse the
    // same guarded helper while the complete scope remains unproven; never
    // retire the physical slot from this local-only observation. A complete
    // source above keeps its existing path without a whole Worker stop.
    let local = if entry_was_closing {
        independently_close_worker(dispatch, directory, source, &predecessor, &binding, now_ms)
            .await?
    } else {
        None
    };
    // Preserve a full-scope failure even if independent local facts progressed.
    scope_progress?;
    Ok(local.or(Some(observation)))
}

async fn ensure_worker_closed(
    directory: &dyn crate::WorkerDirectory,
    identity: &crate::WorkerIdentity,
    now_ms: u64,
) -> Result<bool, ApplicationError> {
    let expiry = directory
        .expire_incarnation(identity, now_ms)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?;
    if expiry != crate::RegistryMutation::Applied
        && !directory
            .incarnation(identity)
            .await
            .map_err(|error| ApplicationError::unavailable(error.to_string()))?
            .is_some_and(|record| {
                matches!(
                    record.snapshot.state,
                    crate::WorkerState::Quiesced | crate::WorkerState::Dead
                )
            })
    {
        return Ok(false);
    }
    Ok(true)
}

async fn independently_close_worker(
    dispatch: &dyn AttemptControlAuthority,
    directory: &dyn crate::WorkerDirectory,
    source: &dyn WorkerExecutionStopSource,
    predecessor: &crate::PhysicalAttempt,
    binding: &crate::AttemptExecutionBinding,
    now_ms: u64,
) -> Result<Option<WorkerProcessStopStatus>, ApplicationError> {
    if !ensure_worker_closed(directory, binding.worker(), now_ms).await? {
        return Ok(None);
    }
    if dispatch
        .load_physical_attempt(&predecessor.claim.run_id)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?
        .as_ref()
        != Some(predecessor)
    {
        return Ok(None);
    }
    let Some(original_worker) = directory
        .incarnation(binding.worker())
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?
    else {
        return Ok(None);
    };
    let closed = ClosedWorkerExecution {
        binding: binding.clone(),
        create_ledger: original_worker.create_ledger,
    };
    let observation = if let Some(exit) = persisted_worker_exit(directory, binding).await? {
        WorkerProcessStopStatus::Exited(exit)
    } else {
        source.stop_closed_worker(&closed).await?
    };
    let WorkerProcessStopStatus::Exited(exit) = observation else {
        return Ok(Some(WorkerProcessStopStatus::Unknown));
    };
    if exit.binding() != binding {
        return Err(ApplicationError::internal(
            "Worker exit names another execution",
        ));
    }
    let receipt = crate::WorkerProcessExit::new(
        binding.provider_id(),
        binding.execution_ref(),
        exit.evidence_ref(),
    )
    .map_err(|error| ApplicationError::internal(error.to_string()))?;
    persist_worker_exit(
        directory,
        binding.worker(),
        receipt,
        ApplicationError::internal("Worker exit could not be committed"),
        ApplicationError::internal("Worker exit readback did not match"),
    )
    .await?;
    source.release_closed_worker_evidence(&closed).await?;
    Ok(Some(WorkerProcessStopStatus::Exited(exit)))
}

async fn persisted_worker_exit(
    directory: &dyn crate::WorkerDirectory,
    binding: &crate::AttemptExecutionBinding,
) -> Result<Option<ObservedWorkerProcessExit>, ApplicationError> {
    let record = directory
        .incarnation(binding.worker())
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?;
    let Some(record) = record else {
        return Ok(None);
    };
    let Some(receipt) = record.process_exit else {
        return Ok(None);
    };
    if receipt.provider_id() != binding.provider_id()
        || receipt.execution_ref() != binding.execution_ref()
    {
        return Err(ApplicationError::internal(
            "persisted Worker exit names another execution",
        ));
    }
    let closed = ClosedWorkerExecution {
        binding: binding.clone(),
        create_ledger: record.create_ledger,
    };
    let WorkerProcessStopStatus::Exited(exit) = closed.observed_exit(receipt.evidence_ref())?
    else {
        unreachable!()
    };
    Ok(Some(exit))
}

/// Select the existing physical commit owner from the Session Thread identity
/// already derived from a trusted, claim-guarded dispatch by Coordinator
/// application. The Coordinator retains the sole write invocation:
/// [`ClaimedCommitService`] is this port's only caller.
#[async_trait::async_trait]
pub trait ClaimedOperationCoordinatorResolver: Send + Sync {
    async fn resolve(
        &self,
        session_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<Arc<dyn OperationCoordinator>, ApplicationError>;
}

struct FixedOperationCoordinatorResolver(Arc<dyn OperationCoordinator>);

#[async_trait::async_trait]
impl ClaimedOperationCoordinatorResolver for FixedOperationCoordinatorResolver {
    async fn resolve(
        &self,
        _session_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<Arc<dyn OperationCoordinator>, ApplicationError> {
        Ok(self.0.clone())
    }
}

/// Exact-epoch committed-truth application service. Authentication and HTTP
/// status mapping are deliberately outside this owner.
pub struct ClaimedCommitService {
    dispatch: Arc<dyn DispatchCommitAuthority>,
    coordinator: Arc<dyn ClaimedOperationCoordinatorResolver>,
}

impl ClaimedCommitService {
    #[must_use]
    pub fn new(
        dispatch: Arc<dyn DispatchCommitAuthority>,
        coordinator: Arc<dyn OperationCoordinator>,
    ) -> Self {
        Self {
            dispatch,
            coordinator: Arc::new(FixedOperationCoordinatorResolver(coordinator)),
        }
    }

    #[must_use]
    pub fn with_coordinator_resolver(
        dispatch: Arc<dyn DispatchCommitAuthority>,
        coordinator: Arc<dyn ClaimedOperationCoordinatorResolver>,
    ) -> Self {
        Self {
            dispatch,
            coordinator,
        }
    }

    pub async fn apply_claimed(
        &self,
        request: ClaimedCommitRequest,
    ) -> Result<CommitReceipt, ApplicationError> {
        let guard = self
            .dispatch
            .lock_commit_epoch(&request.claim)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?
            .ok_or_else(|| ApplicationError::invalid("run claim is stale"))?;
        let trusted_dispatch = guard.request();
        validate_commit_dispatch_binding(trusted_dispatch, &request.operation.commit)
            .map_err(ApplicationError::invalid)?;
        if request.operation.operation_id.run_id != *trusted_dispatch.run_id() {
            return Err(ApplicationError::invalid(format!(
                "commit operation run {} does not match claimed dispatch run {}",
                request.operation.operation_id.run_id.0,
                trusted_dispatch.run_id().0
            )));
        }
        require_commit_operation_payload_hash(&request.operation)
            .map_err(|error| ApplicationError::invalid(error.to_string()))?;
        // The guarded queue row, never the Worker request, selects the Session
        // partition. A stale claim returned above before this resolution.
        let coordinator = self
            .coordinator
            .resolve(trusted_dispatch.session_thread_id())
            .await?;
        let committed = coordinator
            .commit_operation(request.operation)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()));
        match guard.release_after(committed).await {
            Ok(committed) => committed,
            Err(error) => Err(ApplicationError::unavailable(format!(
                "commit claim lock release failed: {error}"
            ))),
        }
    }
}

/// Memory/SQLite consume the original Registry exclusion around their canonical
/// slot operation. PostgreSQL transfers the same resource into its transaction.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) async fn admit_with_incarnation_guard(
    dispatch: &dyn DispatchExecution,
    claim: &crate::RunClaim,
    binding: Option<&crate::AttemptExecutionBinding>,
    guard: awaken_dispatch_contract::WorkerIncarnationGuard,
    handoff: bool,
) -> Result<crate::AttemptAdmission, crate::DispatchError> {
    let snapshot = guard.snapshot();
    if matches!(
        snapshot.state,
        crate::WorkerState::Quiesced | crate::WorkerState::Dead
    ) {
        guard
            .release()
            .await
            .map_err(crate::DispatchError::unavailable)?;
        return Ok(crate::AttemptAdmission::Fenced);
    }
    if snapshot.identity.lease_owner() != claim.owner
        || binding.is_none_or(|binding| binding.worker() != &snapshot.identity)
    {
        guard
            .release()
            .await
            .map_err(crate::DispatchError::unavailable)?;
        return Err(crate::DispatchError::Rejected(
            "Registry guard does not match the physical attempt".into(),
        ));
    }
    let admission = if handoff {
        dispatch.handoff_attempt(claim, binding).await
    } else {
        dispatch.begin_attempt(claim, binding).await
    };
    guard
        .release()
        .await
        .map_err(crate::DispatchError::unavailable)?;
    admission
}

use awaken_dispatch_contract::{
    DispatchAttemptControl, DispatchClaimOwnership, DispatchCommitAuthority, DispatchControlReads,
    DispatchExecution, DispatchReads,
};
pub trait AttemptControlAuthority:
    DispatchExecution
    + DispatchClaimOwnership
    + DispatchCommitAuthority
    + DispatchReads
    + DispatchAttemptControl
    + DispatchControlReads
{
}
impl<
    T: DispatchExecution
        + DispatchClaimOwnership
        + DispatchCommitAuthority
        + DispatchReads
        + DispatchAttemptControl
        + DispatchControlReads,
> AttemptControlAuthority for T
{
}

use awaken_dispatch_execution::PhysicalAttemptReads;

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_session_execution_contract::ApplicationErrorKind;

    #[test]
    fn complete_receipt_freezes_all_physical_identity_coordinates() {
        // R28 exact typed assertion => equal; independently vary Run, owner,
        // epoch, optional binding, logical/Session Thread, Worker
        // id/incarnation/generation, provider and execution reference =>
        // unequal. This is the same derived equality the
        // application uses before finish_attempt, not string concatenation or a
        // Run-only comparison. Blank evidence rejects. Constructor privacy is
        // covered by the compile-fail doctest; provider truth is outside this test.
        let worker = crate::WorkerIdentity::new("worker", "boot", 1);
        let binding =
            crate::AttemptExecutionBinding::new(worker.clone(), "provider", "instance").unwrap();
        let attempt = crate::PhysicalAttempt {
            closing: true,
            claim: crate::RunClaim {
                run_id: awaken_agent_contract::agent::run::Id("run".into()),
                owner: worker.lease_owner(),
                epoch: 1,
            },
            binding: Some(binding.clone()),
            thread_id: Some(awaken_agent_contract::agent::thread::Id("thread".into())),
            session_thread_id: Some(awaken_agent_contract::agent::thread::Id("session".into())),
        };
        let command = ClosedAttemptExecution {
            attempt: attempt.clone(),
            process_exit: Some(ObservedWorkerProcessExit {
                binding,
                evidence_ref: "local-exit".into(),
            }),
        };
        assert!(command.observed_all_execution_stopped("\n\t").is_err());
        let AttemptStopStatus::Stopped(receipt) =
            command.observed_all_execution_stopped("complete").unwrap()
        else {
            panic!("positive witness")
        };
        assert_eq!(receipt.attempt(), &attempt);
        for coordinate in [
            "run",
            "owner",
            "epoch",
            "missing-binding",
            "thread",
            "session-thread",
            "worker",
            "boot",
            "generation",
            "provider",
            "instance",
        ] {
            let mut other = attempt.clone();
            let mut other_worker = worker.clone();
            match coordinate {
                "run" => other.claim.run_id.0.push('2'),
                "owner" => other.claim.owner.push('2'),
                "epoch" => other.claim.epoch += 1,
                "missing-binding" => other.binding = None,
                "thread" => other.thread_id.as_mut().unwrap().0.push('2'),
                "session-thread" => other.session_thread_id.as_mut().unwrap().0.push('2'),
                "worker" => other_worker.worker_id.push('2'),
                "boot" => other_worker.incarnation_id.push('2'),
                "generation" => other_worker.generation += 1,
                _ => {}
            }
            if ![
                "run",
                "owner",
                "epoch",
                "missing-binding",
                "thread",
                "session-thread",
            ]
            .contains(&coordinate)
            {
                other.binding = Some(
                    crate::AttemptExecutionBinding::new(
                        other_worker,
                        if coordinate == "provider" {
                            "provider2"
                        } else {
                            "provider"
                        },
                        if coordinate == "instance" {
                            "instance2"
                        } else {
                            "instance"
                        },
                    )
                    .unwrap(),
                );
            }
            assert_ne!(receipt.attempt(), &other, "{coordinate}");
        }
    }

    #[test]
    fn application_error_kinds_preserve_http_mapping_causes() {
        // Cause/effect graph: C1 caller input/claim is invalid -> E1 4xx;
        // C2 state conflicts -> E2 409; C3 permanent internal failure -> E3 500;
        // C4 temporary dependency failure -> E4 retryable 503.
        // The application port preserves these causes without importing HTTP.
        //
        // | Rule | cause            | effect kind     |
        // | R1   | invalid request  | InvalidRequest  |
        // | R2   | state conflict   | Conflict        |
        // | R3   | internal fault   | Internal        |
        // | R4   | dependency down  | Unavailable     |
        assert_eq!(
            ApplicationError::invalid("x").kind,
            ApplicationErrorKind::InvalidRequest
        );
        assert_eq!(
            ApplicationError::conflict("x").kind,
            ApplicationErrorKind::Conflict
        );
        assert_eq!(
            ApplicationError::internal("x").kind,
            ApplicationErrorKind::Internal
        );
        assert_eq!(
            ApplicationError::unavailable("x").kind,
            ApplicationErrorKind::Unavailable
        );
    }
}
