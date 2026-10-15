// @formal surface obligation=ingress.single_owner
// @formal surface obligation=ingress.epoch_reclaim
// @formal surface obligation=ingress.settle_fence
// @formal reason: Backend-neutral dispatch role delegation, not a claim or SQL owner.
//! Required dispatch roles bound once for Host, Worker and Coordinator.
//!
//! Process composition injects an existing `Dispatch` implementation and its
//! required local or remote roles. This crate
//! delegates to that implementation; it owns no queue, backend selection, clock,
//! wake, claim transition or Worker. Ingress remains the sole SQL/NATS factory.
//!
//! The trait-object indirection is deliberate: delegating through `dyn Dispatch`
//! erases the Postgres backend's sqlx `Executor` future to a `dyn Future + Send`
//! at the trait boundary, so composing it into the daemon's spawned task stays
//! `Send` (a direct enum match over the concrete sqlx store trips the sqlx
//! higher-ranked "implementation of Send is not general enough" limitation).

mod binding;
mod bundle;
pub use binding::{DispatchBinding, DispatchTopology};
pub use bundle::{Dispatch, DurableDispatch, RemoteDispatch};
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
    CredentialRealizationReceipt, DispatchCompletion, DispatchDepths, DispatchError,
    DispatchOutcome, DispatchSummary, Inbox, InboxRead, LeaseRenewalOutcome, Outbox, PendingInput,
    PendingRecord, RunClaim, SessionChildAdmission, SessionRunReservationActivation,
    SessionRunReservationOutcome, SessionRunReservationResolution, SettleOutcome, SubmitOptions,
};
use awaken_dispatch_contract::{DispatchCursor, DispatchOperationalFeed, DispatchPage};

use awaken_dispatch_contract::{
    ContinuationAdmission, DispatchAdministration, DispatchAttemptControl, DispatchClaimOwnership,
    DispatchClaims, DispatchCommitAuthority, DispatchControlClaims, DispatchControlReads,
    DispatchEnqueue, DispatchExecution, DispatchMaintenance, DispatchReads, DispatchSettlement,
    SessionContinuationOutbox, SessionRunAdmissionReads, SessionRunReservations,
};

/// Worker queue view, without local commit, maintenance or continuation authority.
///
/// ```compile_fail
/// use awaken_dispatch_erasure::AnyDispatchStore;
/// use awaken_dispatch_contract::DispatchCommitAuthority;
/// fn local<T: DispatchCommitAuthority>() {}
/// local::<AnyDispatchStore>();
/// ```
/// ```compile_fail
/// use awaken_dispatch_erasure::AnyDispatchStore;
/// use awaken_dispatch_contract::Inbox;
/// fn inbox<T: Inbox>() {}
/// inbox::<AnyDispatchStore>();
/// ```
/// ```compile_fail
/// use awaken_dispatch_erasure::AnyDispatchStore;
/// use awaken_dispatch_contract::DispatchMaintenance;
/// fn maintain<T: DispatchMaintenance>() {}
/// maintain::<AnyDispatchStore>();
/// ```
/// Neither scoped nor global operational reads are fabricated on a Worker view.
/// ```compile_fail
/// use awaken_dispatch_erasure::AnyDispatchStore;
/// use awaken_dispatch_contract::{DispatchOperationalFeed, DispatchCursor};
/// async fn global(worker: &AnyDispatchStore) {
///     worker.events_after(DispatchCursor(0), 1, None).await;
/// }
/// ```
/// ```compile_fail
/// use awaken_dispatch_erasure::AnyDispatchStore;
/// use awaken_dispatch_contract::{DispatchOperationalFeed, DispatchCursor};
/// use awaken_agent_contract::agent::run::Id;
/// async fn scoped(worker: &AnyDispatchStore, run: &Id) {
///     worker.events_after(DispatchCursor(0), 1, Some(run)).await;
/// }
/// ```
/// Admission input is also Coordinator-only; no claim-as-read escape exists.
/// ```compile_fail,E0599
/// use awaken_dispatch_erasure::AnyDispatchStore;
/// use awaken_dispatch_contract::SessionRunAdmissionReads;
/// use awaken_agent_contract::agent::run::Id;
/// async fn observe(worker: &AnyDispatchStore) {
///     worker.observe_session_run_admission(&Id("peer".into())).await;
/// }
/// ```
/// A Worker view also cannot discover a local checkpoint authority.
/// ```compile_fail
/// use awaken_dispatch_erasure::AnyDispatchStore;
/// fn checkpoint(worker: &AnyDispatchStore) { worker.stream_checkpoint_store(); }
/// ```
/// ```compile_fail
/// use awaken_dispatch_erasure::AnyDispatchStore;
/// use awaken_dispatch_contract::SessionContinuationOutbox;
/// fn continuation<T: SessionContinuationOutbox>() {}
/// continuation::<AnyDispatchStore>();
/// ```
pub struct AnyDispatchStore {
    inner: Arc<dyn Dispatch>,
}
impl AnyDispatchStore {
    pub fn from_dispatch(inner: Arc<dyn Dispatch>) -> Self {
        Self { inner }
    }
    fn dispatch_handle(&self) -> &dyn Dispatch {
        self.inner.as_ref()
    }
}
/// Coordinator view. One durable owner supplies every required local role.
pub struct AnyCoordinatorDispatch {
    owner: Arc<dyn DurableDispatch>,
    worker: Arc<AnyDispatchStore>,
    stream_checkpoint: Option<Arc<dyn StreamCheckpointStore>>,
}
impl AnyCoordinatorDispatch {
    pub fn from_durable_store(
        store: impl DurableDispatch + 'static,
        stream_checkpoint: Option<Arc<dyn StreamCheckpointStore>>,
    ) -> Self {
        Self::from_owner(Arc::new(store), stream_checkpoint)
    }
    pub fn from_durable_dispatch(owner: Arc<dyn DurableDispatch>) -> Self {
        Self::from_owner(owner, None)
    }
    fn from_owner(
        owner: Arc<dyn DurableDispatch>,
        stream_checkpoint: Option<Arc<dyn StreamCheckpointStore>>,
    ) -> Self {
        let worker = Arc::new(AnyDispatchStore::from_dispatch(owner.clone()));
        Self {
            owner,
            worker,
            stream_checkpoint,
        }
    }
    pub fn worker_view(&self) -> Arc<AnyDispatchStore> {
        self.worker.clone()
    }
    pub fn stream_checkpoint_store(&self) -> Option<Arc<dyn StreamCheckpointStore>> {
        self.stream_checkpoint.clone()
    }
    fn dispatch_handle(&self) -> &dyn Dispatch {
        self.owner.as_ref()
    }
}
#[cfg(test)]
mod capability_tests;
macro_rules! delegate {
    ($self:ident, $method:ident ($($arg:expr),*)) => { $self.dispatch_handle().$method($($arg),*).await };
}
macro_rules! delegate_owner {
    ($self:ident, $method:ident ($($arg:expr),*)) => { $self.owner.$method($($arg),*).await };
}
macro_rules! worker_roles {
    ($ty:ty) => {
        #[async_trait]
        impl DispatchEnqueue for $ty {
            async fn enqueue_with(
                &self,
                request: RunDispatch,
                options: SubmitOptions,
            ) -> Result<(), DispatchError> {
                delegate!(self, enqueue_with(request, options))
            }
        }
        #[async_trait]
        impl DispatchClaims for $ty {
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

            async fn claim_new_run(
                &self,
                request: RunDispatch,
                owner: &str,
                lease_ms: u64,
                capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
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
                capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
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
                capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
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
                capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
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
        }
        #[async_trait]
        impl DispatchExecution for $ty {
            async fn close_attempt(
                &self,
                claim: &RunClaim,
            ) -> Result<SettleOutcome, DispatchError> {
                delegate!(self, close_attempt(claim))
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

            async fn finish_attempt(
                &self,
                claim: &RunClaim,
                cause: Option<awaken_dispatch_contract::AttemptReturnCause>,
            ) -> Result<SettleOutcome, DispatchError> {
                delegate!(self, finish_attempt(claim, cause))
            }

            async fn relinquish_claim(
                &self,
                claim: &RunClaim,
            ) -> Result<SettleOutcome, DispatchError> {
                delegate!(self, relinquish_claim(claim))
            }

            async fn defer_claim(
                &self,
                claim: &RunClaim,
                retry_after_ms: u64,
            ) -> Result<SettleOutcome, DispatchError> {
                delegate!(self, defer_claim(claim, retry_after_ms))
            }

            async fn record_credential_realization(
                &self,
                claim: &RunClaim,
                receipt: CredentialRealizationReceipt,
            ) -> Result<SettleOutcome, DispatchError> {
                delegate!(self, record_credential_realization(claim, receipt))
            }

            async fn bind_sandbox(
                &self,
                claim: &RunClaim,
                sandbox_ref: &str,
            ) -> Result<SettleOutcome, DispatchError> {
                delegate!(self, bind_sandbox(claim, sandbox_ref))
            }
        }
        #[async_trait]
        impl DispatchClaimOwnership for $ty {
            async fn claim_is_current(&self, claim: &RunClaim) -> Result<bool, DispatchError> {
                delegate!(self, claim_is_current(claim))
            }
        }
        #[async_trait]
        impl DispatchSettlement for $ty {
            async fn settle(
                &self,
                run_id: &RunId,
                epoch: u64,
                outcome: DispatchOutcome,
                consumed: &[String],
            ) -> Result<SettleOutcome, DispatchError> {
                delegate!(self, settle(run_id, epoch, outcome, consumed))
            }
        }
        #[async_trait]
        impl InboxRead for $ty {
            async fn list(
                &self,
                thread_id: &ThreadId,
            ) -> Result<Vec<PendingRecord>, DispatchError> {
                delegate!(self, list(thread_id))
            }
        }
    };
}
worker_roles!(AnyDispatchStore);
worker_roles!(AnyCoordinatorDispatch);
#[async_trait]
impl DispatchAdministration for AnyCoordinatorDispatch {
    async fn cancel(&self, run_id: &RunId) -> Result<Option<ThreadId>, DispatchError> {
        delegate_owner!(self, cancel(run_id))
    }

    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        delegate_owner!(self, requeue(run_id))
    }
}
#[async_trait]
impl DispatchCommitAuthority for AnyCoordinatorDispatch {
    async fn worker_owns_run(
        &self,
        identity: &awaken_dispatch_contract::WorkerIdentity,
        run_id: &RunId,
    ) -> Result<Option<RunClaim>, DispatchError> {
        delegate_owner!(self, worker_owns_run(identity, run_id))
    }

    async fn lock_commit_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        delegate_owner!(self, lock_commit_epoch(claim))
    }
}
#[async_trait]
impl DispatchReads for AnyCoordinatorDispatch {
    async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError> {
        delegate_owner!(self, dead_letters())
    }

    async fn superseded(&self) -> Result<Vec<RunId>, DispatchError> {
        delegate_owner!(self, superseded())
    }

    async fn list_dispatches(&self) -> Result<Vec<DispatchSummary>, DispatchError> {
        delegate_owner!(self, list_dispatches())
    }
}
#[async_trait]
impl DispatchMaintenance for AnyCoordinatorDispatch {
    async fn quarantine_retry_exhausted(
        &self,
        max_attempts: u64,
        expired_before_ms: u64,
    ) -> Result<usize, DispatchError> {
        delegate_owner!(
            self,
            quarantine_retry_exhausted(max_attempts, expired_before_ms)
        )
    }

    async fn purge_dead_letters(&self) -> Result<usize, DispatchError> {
        delegate_owner!(self, purge_dead_letters())
    }

    async fn purge_dead_letters_before(&self, cutoff_ms: u64) -> Result<usize, DispatchError> {
        delegate_owner!(self, purge_dead_letters_before(cutoff_ms))
    }
}
#[async_trait]
impl SessionRunAdmissionReads for AnyCoordinatorDispatch {
    async fn observe_session_run_admission(
        &self,
        run_id: &RunId,
    ) -> Result<awaken_dispatch_contract::SessionRunAdmissionObservation, DispatchError> {
        delegate_owner!(self, observe_session_run_admission(run_id))
    }
}

#[async_trait]
impl SessionRunReservations for AnyCoordinatorDispatch {
    async fn reserve_session_run(
        &self,
        request: RunDispatch,
        reservation_ttl_ms: u64,
    ) -> Result<SessionRunReservationOutcome, DispatchError> {
        delegate_owner!(self, reserve_session_run(request, reservation_ttl_ms))
    }

    async fn activate_session_run_reservation(
        &self,
        run_id: &RunId,
        session_thread_id: &ThreadId,
        session_activity_epoch: u64,
    ) -> Result<SessionRunReservationActivation, DispatchError> {
        delegate_owner!(
            self,
            activate_session_run_reservation(run_id, session_thread_id, session_activity_epoch)
        )
    }

    async fn reject_session_run_reservation(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        delegate_owner!(self, reject_session_run_reservation(run_id))
    }

    async fn enqueue_session_child(
        &self,
        request: RunDispatch,
        admission: SessionChildAdmission,
    ) -> Result<(), DispatchError> {
        delegate_owner!(self, enqueue_session_child(request, admission))
    }
}

#[async_trait::async_trait]
impl awaken_dispatch_contract::SessionReservationAuthority for AnyCoordinatorDispatch {
    async fn lock_session_run_reservation_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        delegate_owner!(self, lock_session_run_reservation_epoch(claim))
    }
}
#[async_trait]
impl DispatchAttemptControl for AnyCoordinatorDispatch {
    async fn finish_attempt_closure(
        &self,
        record: &awaken_dispatch_contract::AttemptClosureRecord,
    ) -> Result<SettleOutcome, DispatchError> {
        delegate_owner!(self, finish_attempt_closure(record))
    }

    async fn admit_attempt_guarded(
        &self,
        claim: &RunClaim,
        binding: Option<&awaken_dispatch_contract::dispatch::AttemptExecutionBinding>,
        guard: awaken_dispatch_contract::WorkerIncarnationGuard,
        handoff: bool,
    ) -> Result<awaken_dispatch_contract::AttemptAdmission, DispatchError> {
        delegate_owner!(self, admit_attempt_guarded(claim, binding, guard, handoff))
    }
}
#[async_trait]
impl DispatchControlClaims for AnyCoordinatorDispatch {
    async fn claim_placed(
        &self,
        requester: &WorkerSnapshot,
        workers: Vec<WorkerSnapshot>,
        policy: Arc<dyn PlacementPolicy>,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate_owner!(self, claim_placed(requester, workers, policy, lease_ms))
    }

    async fn claim_for_terminal_recovery(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate_owner!(self, claim_for_terminal_recovery(run_id, owner, lease_ms))
    }
}
#[async_trait]
impl DispatchControlReads for AnyCoordinatorDispatch {
    async fn current_credential_binding(
        &self,
        claim: &RunClaim,
        candidate: &CandidateFingerprint,
    ) -> Result<Option<AttemptCredentialBinding>, DispatchError> {
        delegate_owner!(self, current_credential_binding(claim, candidate))
    }

    async fn load_physical_attempt(
        &self,
        run_id: &RunId,
    ) -> Result<Option<awaken_dispatch_contract::dispatch::PhysicalAttempt>, DispatchError> {
        delegate_owner!(self, load_physical_attempt(run_id))
    }

    async fn completion_events_after(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<DispatchCompletion>, DispatchError> {
        delegate_owner!(self, completion_events_after(after_sequence, limit))
    }

    async fn dispatch_depths(&self) -> Result<Option<DispatchDepths>, DispatchError> {
        delegate_owner!(self, dispatch_depths())
    }

    async fn scan_dispatch_ids(
        &self,
        after: Option<&RunId>,
        limit: usize,
    ) -> Result<Vec<RunId>, DispatchError> {
        delegate_owner!(self, scan_dispatch_ids(after, limit))
    }

    async fn dispatch_summary(
        &self,
        run_id: &RunId,
    ) -> Result<Option<DispatchSummary>, DispatchError> {
        delegate_owner!(self, dispatch_summary(run_id))
    }
}
#[async_trait]
impl Inbox for AnyCoordinatorDispatch {
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError> {
        delegate_owner!(self, append(input))
    }

    async fn retract(
        &self,
        message_id: &str,
        expected_revision: u64,
    ) -> Result<CasOutcome, DispatchError> {
        delegate_owner!(self, retract(message_id, expected_revision))
    }

    async fn edit(
        &self,
        message_id: &str,
        expected_revision: u64,
        result: ResumeResult,
    ) -> Result<CasOutcome, DispatchError> {
        delegate_owner!(self, edit(message_id, expected_revision, result))
    }
}
#[async_trait]
impl Outbox for AnyCoordinatorDispatch {
    async fn stage(&self, input: PendingInput) -> Result<bool, DispatchError> {
        delegate_owner!(self, stage(input))
    }

    async fn relay(&self) -> Result<usize, DispatchError> {
        delegate_owner!(self, relay())
    }
}

#[async_trait]
impl DispatchOperationalFeed for AnyCoordinatorDispatch {
    async fn events_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
        run: Option<&RunId>,
    ) -> Result<DispatchPage, DispatchError> {
        self.owner.events_after(cursor, limit, run).await
    }
}
#[async_trait]
impl SessionContinuationOutbox for AnyCoordinatorDispatch {
    async fn stage_session_resume(
        &self,
        input: PendingInput,
        thread: &ThreadId,
        prior: Option<u64>,
        epoch: Option<u64>,
    ) -> Result<bool, DispatchError> {
        self.owner
            .stage_session_resume(input, thread, prior, epoch)
            .await
    }
    async fn relay_and_enqueue(
        &self,
        input: PendingInput,
        request: RunDispatch,
        admission: ContinuationAdmission,
    ) -> Result<(), DispatchError> {
        self.owner
            .relay_and_enqueue(input, request, admission)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_dispatch_contract::DispatchAttemptRecordReads for AnyCoordinatorDispatch {
    async fn attempt_closure_records_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
        run: Option<&RunId>,
    ) -> Result<awaken_dispatch_contract::AttemptClosurePage, DispatchError> {
        delegate_owner!(self, attempt_closure_records_after(cursor, limit, run))
    }
}
