//! Exact closed-idle Worker retirement port for the process maintenance scan.

use awaken_dispatch_contract::dispatch::PhysicalAttempt;
use awaken_dispatch_contract::{DispatchControlReads, DispatchReads, DispatchSummary};
use awaken_session_execution_contract::ApplicationError;
use awaken_worker_contract::{WorkerDirectory, WorkerIdentity};

/// Borrow the existing Execution-owned SDK source for one reconciliation call.
/// Consumers receive the published snapshot value, never the engine SPI or a
/// cached prefix. The original physical slot selects all read coordinates.
pub struct PhysicalAttemptRecoveryRead<'a> {
    source: &'a dyn awaken_agent_contract::thread::read::recovery::RunRecoverySource,
}

impl<'a> PhysicalAttemptRecoveryRead<'a> {
    pub fn new(
        source: &'a dyn awaken_agent_contract::thread::read::recovery::RunRecoverySource,
    ) -> Self {
        Self { source }
    }

    pub async fn snapshot_for(
        &self,
        attempt: &PhysicalAttempt,
    ) -> Result<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot, ApplicationError>
    {
        let thread = attempt.thread_id.as_ref().ok_or_else(|| {
            ApplicationError::invalid("physical attempt recovery requires an exact logical Thread")
        })?;
        let partition = attempt.session_thread_id.as_ref().unwrap_or(thread);
        self.source
            .recovery_snapshot_in_session(partition, thread, &attempt.claim.run_id)
            .await
            .map_err(|error| ApplicationError::unavailable(error.to_string()))
    }
}

/// Lower the sole SDK terminal predicate at the Execution boundary. This is a
/// fresh read from the supplied committed view, not caller-provided stop proof.
pub fn committed_terminal_ready(
    reader: &dyn awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView,
    summary: &DispatchSummary,
) -> Result<bool, ApplicationError> {
    use awaken_runtime_contract::terminal::{
        CommittedTerminalProjection, committed_terminal_projection,
    };
    match committed_terminal_projection(reader, &summary.run_id, &summary.thread_id) {
        CommittedTerminalProjection::Nonterminal => Ok(false),
        CommittedTerminalProjection::Exact(_) => Ok(true),
        CommittedTerminalProjection::IdentityConflict => Err(ApplicationError::invalid(
            "terminal closure names another committed Run or Thread",
        )),
    }
}

/// The Host scans and expires Registry records; Coordinator Ingress retains the
/// sole physical-stop and exact evidence-release implementation. This port
/// carries no process-exit receipt, closed-attempt command or second stop rule.
#[async_trait::async_trait]
pub trait WorkerEvidenceRelease: Send + Sync {
    /// Provider retention is optional. Registry expiry is independent of it.
    fn owns_worker_evidence(&self, identity: &WorkerIdentity) -> bool;

    /// Retire only the original closed-idle domain after the exact physical slot
    /// scan. The Host supplies its trusted clock so Ingress honors the original
    /// Registry lease deadline before stopping a still-active Worker. Unknown
    /// closure remains retryable; no exit receipt or attempt-settlement authority
    /// crosses this port.
    async fn release_closed_idle_worker_evidence(
        &self,
        dispatch: &dyn PhysicalAttemptReads,
        directory: &dyn WorkerDirectory,
        identity: &WorkerIdentity,
        now_ms: u64,
    ) -> Result<(), ApplicationError>;

    /// Release provider retention of ended process domains that never reached
    /// this Registry, e.g. a Worker Pod that died during registration. Such a
    /// domain has no Registry record for the per-record scan to visit. Carries
    /// no exit receipt and writes no Registry record.
    async fn release_unregistered_worker_evidence(
        &self,
        directory: &dyn WorkerDirectory,
    ) -> Result<(), ApplicationError>;

    /// Record the provider-observed process end of every closed incarnation
    /// that still occupies a physical slot, before the provider loses it. It
    /// observes only: no stop, no evidence release, no slot change; the
    /// Registry receipt it writes is the one blocked-successor closure reuses.
    async fn record_occupied_worker_ends(
        &self,
        dispatch: &dyn PhysicalAttemptReads,
        directory: &dyn WorkerDirectory,
        now_ms: u64,
    ) -> Result<(), ApplicationError>;
}

/// Trusted Coordinator-selected terminal maintenance role. The original idle
/// leaf stays narrow: only this explicit role can request independently proven
/// closure of an occupied terminal attempt. Ingress alone constructs the private
/// stop commands/receipts; this role returns only terminal-claim readiness.
/// Worker JSON and committed-terminal DTOs cannot provide an implementation.
/// Cause/compile-surface table: C1 only the existing idle leaf is selected ->
/// no terminal method; C2 a Worker queue view is supplied -> no local physical
/// authority. Both reject at the boundary, before any private stop command.
///
/// ```compile_fail
/// use awaken_dispatch_execution::{AnyCoordinatorDispatch, WorkerEvidenceRelease};
/// use awaken_dispatch_contract::DispatchSummary;
/// use awaken_worker_contract::WorkerDirectory;
/// use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
/// async fn idle_only(
///     source: &dyn WorkerEvidenceRelease, queue: &AnyCoordinatorDispatch,
///     directory: &dyn WorkerDirectory, reader: &dyn CommittedThreadView,
///     summary: &DispatchSummary,
/// ) {
///     let recovery: Option<&awaken_dispatch_execution::PhysicalAttemptRecoveryRead<'_>> = None;
///     let _ = source.reconcile_terminal_attempt(queue, directory, reader, summary, recovery, 0).await;
/// }
/// ```
/// ```compile_fail
/// use awaken_dispatch_execution::{AnyDispatchStore, WorkerTerminalReconciliation};
/// use awaken_dispatch_contract::DispatchSummary;
/// use awaken_worker_contract::WorkerDirectory;
/// use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
/// async fn worker_queue(
///     source: &dyn WorkerTerminalReconciliation, queue: &AnyDispatchStore,
///     directory: &dyn WorkerDirectory, reader: &dyn CommittedThreadView,
///     summary: &DispatchSummary,
/// ) {
///     let recovery: Option<&awaken_dispatch_execution::PhysicalAttemptRecoveryRead<'_>> = None;
///     let _ = source.reconcile_terminal_attempt(queue, directory, reader, summary, recovery, 0).await;
/// }
/// ```
#[async_trait::async_trait]
pub trait WorkerTerminalReconciliation: WorkerEvidenceRelease {
    async fn reconcile_terminal_attempt(
        &self,
        dispatch: &awaken_dispatch_erasure::AnyCoordinatorDispatch,
        directory: &dyn WorkerDirectory,
        reader: &dyn awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView,
        summary: &awaken_dispatch_contract::DispatchSummary,
        recovery: Option<&PhysicalAttemptRecoveryRead<'_>>,
        now_ms: u64,
    ) -> Result<bool, ApplicationError>;
}

/// Read-only physical-slot discovery supplied by the local Coordinator.
pub trait PhysicalAttemptReads: DispatchReads + DispatchControlReads {}
impl<T: DispatchReads + DispatchControlReads> PhysicalAttemptReads for T {}
