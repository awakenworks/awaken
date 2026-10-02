// @formal surface obligation=ingress.single_owner
// @formal surface obligation=ingress.epoch_reclaim
// @formal surface obligation=ingress.settle_fence
// @formal reason: Backend-neutral dispatch role delegation, not a claim or SQL owner.
//! One backend-erased dispatch role bundle shared by Host, Worker and Coordinator.
//!
//! Process composition injects an existing `Dispatch` implementation and its
//! explicit optional continuation, operational and checkpoint roles. This crate
//! delegates to that implementation; it owns no queue, backend selection, clock,
//! wake, claim transition or Worker. Ingress remains the sole SQL/NATS factory.
//!
//! The trait-object indirection is deliberate: delegating through `dyn Dispatch`
//! erases the Postgres backend's sqlx `Executor` future to a `dyn Future + Send`
//! at the trait boundary, so composing it into the daemon's spawned task stays
//! `Send` (a direct enum match over the concrete sqlx store trips the sqlx
//! higher-ranked "implementation of Send is not general enough" limitation).

use awaken_dispatch_contract::ControlDispatchQueue;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use awaken_dispatch_contract::{PlacementPolicy, WorkerSnapshot};
use awaken_runtime_contract::resume::ResumeResult;

use awaken_dispatch_contract::RunDispatch;
use awaken_dispatch_contract::{
    AttemptCredentialBinding, CandidateFingerprint, CasOutcome, Claimed, CommitEpochGuard,
    CredentialRealizationReceipt, Dispatch, DispatchCompletion, DispatchError, DispatchOutcome,
    DispatchQueue, DispatchSummary, DurableDispatch, Inbox, LeaseRenewalOutcome, Outbox,
    PendingInput, PendingRecord, RunClaim, SessionChildAdmission, SessionRunReservationActivation,
    SessionRunReservationOutcome, SessionRunReservationResolution, SettleOutcome, SubmitOptions,
};
use awaken_dispatch_contract::{DispatchCursor, DispatchOperationalFeed, DispatchPage};

/// The active durable-dispatch backend behind one concrete type. Both a SQLite
/// and a Postgres store satisfy the full `Dispatch` bundle; this holds whichever
/// was selected and forwards every call to it.
///
/// Capability rule C1: an erased queue may wrap a Worker transport. It must
/// therefore fail to type-check as a Session continuation owner. C2, a genuine
/// durable constructor, exposes its installed owner through
/// [`Self::session_continuation`], covered by the construction test below.
///
/// ```compile_fail
/// use awaken_dispatch_erasure::AnyDispatchStore;
/// use awaken_dispatch_contract::SessionContinuationOutbox;
/// fn requires_continuation<T: SessionContinuationOutbox>() {}
/// requires_continuation::<AnyDispatchStore>();
/// ```
pub struct AnyDispatchStore {
    inner: Arc<dyn Dispatch>,
    continuation: Option<Arc<dyn awaken_dispatch_contract::SessionContinuationOutbox>>,
    operational: Option<Arc<dyn DispatchOperationalFeed>>,
    stream_checkpoint: Option<Arc<dyn StreamCheckpointStore>>,
}

impl AnyDispatchStore {
    /// Bind the exact durable queue, continuation, operational and optional
    /// checkpoint roles selected by one Coordinator backend factory. No role
    /// is inferred by probing an unsupported operation.
    pub fn from_durable_store(
        store: impl DurableDispatch + DispatchOperationalFeed + 'static,
        stream_checkpoint: Option<Arc<dyn StreamCheckpointStore>>,
    ) -> Self {
        let store = Arc::new(store);
        Self {
            inner: store.clone(),
            continuation: Some(store.clone()),
            operational: Some(store),
            stream_checkpoint,
        }
    }

    /// Wrap a Worker-side or otherwise non-continuation [`Dispatch`].
    /// The neutral seam for a composition root that assembles its own backend — e.g.
    /// a horizontal-scaling shard fan-out (`ShardedDispatchQueue`) that composes N
    /// per-shard stores. The composition injects this value into its host explicitly;
    /// the sharding/tenant policy stays in the closed caller.
    pub fn from_dispatch(inner: Arc<dyn Dispatch>) -> Self {
        Self {
            inner,
            continuation: None,
            operational: None,
            stream_checkpoint: None,
        }
    }

    /// Wrap an already-built durable composition and retain its exact Session
    /// continuation role alongside the base dispatch surface.
    pub fn from_durable_dispatch(inner: Arc<dyn DurableDispatch>) -> Self {
        Self {
            inner: inner.clone(),
            continuation: Some(inner),
            operational: None,
            stream_checkpoint: None,
        }
    }

    /// Durable interrupted-stream storage paired with this dispatch authority.
    /// Present for Postgres; SQLite compositions use the host's durable FS adapter,
    /// and remote transports persist through their claim-bound HTTP operations.
    pub fn stream_checkpoint_store(&self) -> Option<Arc<dyn StreamCheckpointStore>> {
        self.stream_checkpoint.clone()
    }

    /// Borrow the actual atomic Session continuation owner, when installed.
    /// The erased Worker queue itself does not implement this capability.
    pub fn session_continuation(
        &self,
    ) -> Option<&dyn awaken_dispatch_contract::SessionContinuationOutbox> {
        self.continuation.as_deref()
    }
}

/// Delegate one `&self` async method to the active backend through the trait
/// object (so the composed future stays `Send`, see the module doc).
macro_rules! delegate {
    ($self:ident, $method:ident ( $($arg:expr),* )) => {
        $self.inner.$method($($arg),*).await
    };
}

#[async_trait]
impl DispatchQueue for AnyDispatchStore {
    async fn close_attempt(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        delegate!(self, close_attempt(claim))
    }
    fn claimed_checkpoint_transport(
        &self,
    ) -> Option<&dyn awaken_dispatch_contract::ClaimedCheckpointTransport> {
        self.inner.claimed_checkpoint_transport()
    }

    fn owns_local_maintenance(&self) -> bool {
        self.inner.owns_local_maintenance()
    }

    fn claimed_credential_delivery_transport(
        &self,
    ) -> Option<&dyn awaken_dispatch_contract::ClaimedCredentialDeliveryTransport> {
        self.inner.claimed_credential_delivery_transport()
    }

    async fn resolve_claimed_session_run_reservation(
        &self,
        claim: &RunClaim,
        resolution: SessionRunReservationResolution,
    ) -> Result<SettleOutcome, DispatchError> {
        delegate!(
            self,
            resolve_claimed_session_run_reservation(claim, resolution)
        )
    }

    async fn worker_owns_run(
        &self,
        identity: &awaken_dispatch_contract::WorkerIdentity,
        run_id: &RunId,
    ) -> Result<Option<RunClaim>, DispatchError> {
        delegate!(self, worker_owns_run(identity, run_id))
    }

    async fn claim_is_current(&self, claim: &RunClaim) -> Result<bool, DispatchError> {
        delegate!(self, claim_is_current(claim))
    }

    async fn record_credential_realization(
        &self,
        claim: &RunClaim,
        receipt: CredentialRealizationReceipt,
    ) -> Result<SettleOutcome, DispatchError> {
        delegate!(self, record_credential_realization(claim, receipt))
    }

    async fn lock_commit_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        delegate!(self, lock_commit_epoch(claim))
    }

    async fn enqueue_with(
        &self,
        request: RunDispatch,
        options: SubmitOptions,
    ) -> Result<(), DispatchError> {
        delegate!(self, enqueue_with(request, options))
    }

    async fn claim_new_run(
        &self,
        request: RunDispatch,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(self, claim_new_run(request, owner, lease_ms, capabilities))
    }

    async fn claim_new_run_compatible(
        &self,
        request: RunDispatch,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(self, claim_new_run_compatible(request, worker, lease_ms))
    }

    async fn deliver_and_claim(
        &self,
        input: PendingInput,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(
            self,
            deliver_and_claim(input, owner, lease_ms, capabilities)
        )
    }

    async fn deliver_and_claim_compatible(
        &self,
        input: PendingInput,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(self, deliver_and_claim_compatible(input, worker, lease_ms))
    }

    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(self, claim(owner, lease_ms, capabilities))
    }

    async fn claim_compatible(
        &self,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(self, claim_compatible(worker, lease_ms))
    }

    async fn claim_run(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(self, claim_run(run_id, owner, lease_ms, capabilities))
    }

    async fn claim_retry_exhausted(
        &self,
        owner: &str,
        lease_ms: u64,
        max_attempts: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(self, claim_retry_exhausted(owner, lease_ms, max_attempts))
    }

    async fn claim_run_compatible(
        &self,
        run_id: &RunId,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(self, claim_run_compatible(run_id, worker, lease_ms))
    }

    async fn renew_lease(
        &self,
        claim: &RunClaim,
        lease_ms: u64,
    ) -> Result<LeaseRenewalOutcome, DispatchError> {
        delegate!(self, renew_lease(claim, lease_ms))
    }

    async fn begin_attempt(
        &self,
        claim: &RunClaim,
        _binding: Option<&awaken_dispatch_contract::dispatch::AttemptExecutionBinding>,
    ) -> Result<awaken_dispatch_contract::AttemptAdmission, DispatchError> {
        delegate!(self, begin_attempt(claim, _binding))
    }

    async fn handoff_attempt(
        &self,
        claim: &RunClaim,
        _binding: Option<&awaken_dispatch_contract::dispatch::AttemptExecutionBinding>,
    ) -> Result<awaken_dispatch_contract::AttemptAdmission, DispatchError> {
        delegate!(self, handoff_attempt(claim, _binding))
    }

    async fn finish_attempt(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        delegate!(self, finish_attempt(claim))
    }

    async fn relinquish_claim(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        delegate!(self, relinquish_claim(claim))
    }

    async fn defer_claim(
        &self,
        claim: &RunClaim,
        retry_after_ms: u64,
    ) -> Result<SettleOutcome, DispatchError> {
        delegate!(self, defer_claim(claim, retry_after_ms))
    }

    async fn settle(
        &self,
        run_id: &RunId,
        epoch: u64,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<SettleOutcome, DispatchError> {
        delegate!(self, settle(run_id, epoch, outcome, consumed))
    }

    async fn quarantine_retry_exhausted(
        &self,
        max_attempts: u64,
        expired_before_ms: u64,
    ) -> Result<usize, DispatchError> {
        delegate!(
            self,
            quarantine_retry_exhausted(max_attempts, expired_before_ms)
        )
    }

    async fn bind_sandbox(
        &self,
        claim: &RunClaim,
        sandbox_ref: &str,
    ) -> Result<SettleOutcome, DispatchError> {
        delegate!(self, bind_sandbox(claim, sandbox_ref))
    }

    async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError> {
        delegate!(self, dead_letters())
    }

    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        delegate!(self, requeue(run_id))
    }

    async fn cancel(&self, run_id: &RunId) -> Result<Option<ThreadId>, DispatchError> {
        delegate!(self, cancel(run_id))
    }

    async fn purge_dead_letters(&self) -> Result<usize, DispatchError> {
        delegate!(self, purge_dead_letters())
    }

    async fn purge_dead_letters_before(&self, cutoff_ms: u64) -> Result<usize, DispatchError> {
        delegate!(self, purge_dead_letters_before(cutoff_ms))
    }

    async fn superseded(&self) -> Result<Vec<RunId>, DispatchError> {
        delegate!(self, superseded())
    }

    async fn list_dispatches(&self) -> Result<Vec<DispatchSummary>, DispatchError> {
        delegate!(self, list_dispatches())
    }

    fn control(&self) -> Option<&dyn ControlDispatchQueue> {
        // The adapter owns the Control rung exactly when its inner handle does:
        // a Coordinator wraps a durable store, a Worker wraps its transport
        // projection, and the difference stays visible to the composition root.
        self.inner
            .control()
            .map(|_| self as &dyn ControlDispatchQueue)
    }
}

macro_rules! delegate_control {
    ($self:ident, $method:ident ( $($arg:expr),* )) => {
        match $self.inner.control() {
            Some(control) => control.$method($($arg),*).await,
            None => Err(DispatchError::Rejected(
                "this dispatch handle is a Worker-side projection without the Control rung"
                    .to_string(),
            )),
        }
    };
}

#[async_trait]
impl ControlDispatchQueue for AnyDispatchStore {
    async fn finish_attempt_closure(
        &self,
        claim: &RunClaim,
    ) -> Result<SettleOutcome, DispatchError> {
        delegate_control!(self, finish_attempt_closure(claim))
    }

    async fn admit_attempt_guarded(
        &self,
        claim: &RunClaim,
        binding: Option<&awaken_dispatch_contract::dispatch::AttemptExecutionBinding>,
        guard: awaken_dispatch_contract::WorkerIncarnationGuard,
        handoff: bool,
    ) -> Result<awaken_dispatch_contract::AttemptAdmission, DispatchError> {
        delegate_control!(self, admit_attempt_guarded(claim, binding, guard, handoff))
    }

    async fn current_credential_binding(
        &self,
        claim: &RunClaim,
        candidate: &CandidateFingerprint,
    ) -> Result<Option<AttemptCredentialBinding>, DispatchError> {
        delegate_control!(self, current_credential_binding(claim, candidate))
    }

    async fn reserve_session_run(
        &self,
        request: RunDispatch,
        reservation_ttl_ms: u64,
    ) -> Result<SessionRunReservationOutcome, DispatchError> {
        delegate_control!(self, reserve_session_run(request, reservation_ttl_ms))
    }

    async fn activate_session_run_reservation(
        &self,
        run_id: &RunId,
        session_thread_id: &ThreadId,
        session_activity_epoch: u64,
    ) -> Result<SessionRunReservationActivation, DispatchError> {
        delegate_control!(
            self,
            activate_session_run_reservation(run_id, session_thread_id, session_activity_epoch)
        )
    }

    async fn reject_session_run_reservation(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        delegate_control!(self, reject_session_run_reservation(run_id))
    }

    async fn lock_session_run_reservation_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        delegate_control!(self, lock_session_run_reservation_epoch(claim))
    }

    async fn enqueue_session_child(
        &self,
        request: RunDispatch,
        admission: SessionChildAdmission,
    ) -> Result<(), DispatchError> {
        delegate_control!(self, enqueue_session_child(request, admission))
    }

    async fn claim_placed(
        &self,
        requester: &WorkerSnapshot,
        workers: Vec<WorkerSnapshot>,
        policy: Arc<dyn PlacementPolicy>,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate_control!(self, claim_placed(requester, workers, policy, lease_ms))
    }

    async fn claim_for_terminal_recovery(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate_control!(self, claim_for_terminal_recovery(run_id, owner, lease_ms))
    }

    async fn load_physical_attempt(
        &self,
        run_id: &RunId,
    ) -> Result<Option<awaken_dispatch_contract::dispatch::PhysicalAttempt>, DispatchError> {
        delegate_control!(self, load_physical_attempt(run_id))
    }

    async fn completion_events_after(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<DispatchCompletion>, DispatchError> {
        delegate_control!(self, completion_events_after(after_sequence, limit))
    }

    async fn runnable_depth(&self) -> Result<Option<u64>, DispatchError> {
        delegate_control!(self, runnable_depth())
    }

    async fn scan_dispatch_ids(
        &self,
        after: Option<&RunId>,
        limit: usize,
    ) -> Result<Vec<RunId>, DispatchError> {
        delegate_control!(self, scan_dispatch_ids(after, limit))
    }

    async fn dispatch_summary(
        &self,
        run_id: &RunId,
    ) -> Result<Option<DispatchSummary>, DispatchError> {
        delegate_control!(self, dispatch_summary(run_id))
    }
}

#[async_trait]
impl DispatchOperationalFeed for AnyDispatchStore {
    async fn events_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
        run: Option<&RunId>,
    ) -> Result<DispatchPage, DispatchError> {
        let Some(feed) = &self.operational else {
            return Err(DispatchError::Rejected(
                "configured dispatch adapter does not expose an operational feed".to_string(),
            ));
        };
        feed.events_after(cursor, limit, run).await
    }
}

#[async_trait]
impl Inbox for AnyDispatchStore {
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError> {
        delegate!(self, append(input))
    }

    async fn list(&self, thread_id: &ThreadId) -> Result<Vec<PendingRecord>, DispatchError> {
        delegate!(self, list(thread_id))
    }

    async fn retract(
        &self,
        message_id: &str,
        expected_revision: u64,
    ) -> Result<CasOutcome, DispatchError> {
        delegate!(self, retract(message_id, expected_revision))
    }

    async fn edit(
        &self,
        message_id: &str,
        expected_revision: u64,
        result: ResumeResult,
    ) -> Result<CasOutcome, DispatchError> {
        delegate!(self, edit(message_id, expected_revision, result))
    }
}

#[async_trait]
impl Outbox for AnyDispatchStore {
    async fn stage(&self, input: PendingInput) -> Result<bool, DispatchError> {
        delegate!(self, stage(input))
    }

    async fn relay(&self) -> Result<usize, DispatchError> {
        delegate!(self, relay())
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;

    #[tokio::test]
    async fn erased_dispatch_retains_only_the_roles_selected_by_composition() {
        // Causes: C1 Worker supplies only Dispatch; C2 Coordinator supplies
        // DurableDispatch only or its exact local operational role; C3 paired
        // checkpoint absent/present. Effects: E1 no invented continuation,
        // operational or checkpoint role; E2 continuation retained; E3 local
        // operational feed retained; E4 checkpoint is the same injected Arc.
        // Constraint: no branch probes an unsupported operation to infer a
        // role. Decision table: R1 C1 => E1; R2 C2 durable-only => E2, !E3;
        // R3 C2 local + !C3 => E2+E3, !E4; R4 C2 local + C3 => E2+E3+E4.
        let backend = Arc::new(awaken_dispatch::MemoryDispatchStore::new(
            awaken_lease_contract::system_clock(),
        ));
        let worker = AnyDispatchStore::from_dispatch(backend.clone());
        let coordinator = AnyDispatchStore::from_durable_dispatch(backend);
        let local = AnyDispatchStore::from_durable_store(
            awaken_dispatch::MemoryDispatchStore::new(awaken_lease_contract::system_clock()),
            None,
        );
        let checkpoint: Arc<dyn StreamCheckpointStore> =
            Arc::new(awaken_store_inmem::MemoryStreamCheckpointStore::new());
        let paired = AnyDispatchStore::from_durable_store(
            awaken_dispatch::MemoryDispatchStore::new(awaken_lease_contract::system_clock()),
            Some(checkpoint.clone()),
        );

        assert!(worker.session_continuation().is_none(), "R1/E1");
        assert!(worker.stream_checkpoint_store().is_none(), "R1/E1");
        assert!(
            worker
                .events_after(DispatchCursor(0), 1, None)
                .await
                .is_err(),
            "R1/E1"
        );
        assert!(coordinator.session_continuation().is_some(), "R2/E2");
        assert!(
            coordinator
                .events_after(DispatchCursor(0), 1, None)
                .await
                .is_err(),
            "R2/!E3"
        );
        assert!(local.session_continuation().is_some(), "R3/E2");
        assert!(
            local.events_after(DispatchCursor(0), 1, None).await.is_ok(),
            "R3/E3"
        );
        assert!(local.stream_checkpoint_store().is_none(), "R3/!E4");
        assert!(paired.session_continuation().is_some(), "R4/E2");
        assert!(
            paired
                .events_after(DispatchCursor(0), 1, None)
                .await
                .is_ok(),
            "R4/E3"
        );
        assert!(
            Arc::ptr_eq(&paired.stream_checkpoint_store().unwrap(), &checkpoint),
            "R4/E4 exact checkpoint"
        );
    }
}
