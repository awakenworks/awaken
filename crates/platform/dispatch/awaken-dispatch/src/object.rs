// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Publishes Dispatch lifecycle transitions under the Session head and validates the owning epoch.
//! Durable per-Session Dispatch using the existing transition kernel.
//! Each mutation materializes one immutable snapshot and publishes only by CAS.
use crate::memory::ResumeResult;
use awaken_agent_contract::credential::CandidateFingerprint;
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use awaken_credential_contract::*;
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use awaken_dispatch_contract::*;
use awaken_object_storage::{SessionObjectStore, SessionObjectUpdate};
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use awaken_worker_contract::*;
use std::sync::Arc;

use awaken_agent_contract::agent::{run::Id as RunId, thread::Id as ThreadId};

pub struct ObjectDispatchPartition {
    objects: Arc<dyn SessionObjectStore>,
    owner: String,
    session: String,
    clock: Arc<dyn awaken_time::WallClock>,
}
impl ObjectDispatchPartition {
    pub fn new(
        objects: Arc<dyn SessionObjectStore>,
        owner: String,
        session: String,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            objects,
            owner,
            session,
            clock,
        }
    }
    pub(crate) async fn claim_requested(
        &self,
        identity: String,
        receipt: crate::claim_request::ClaimReceiptRequest<'_>,
        requester: &WorkerSnapshot,
        workers: Vec<WorkerSnapshot>,
        policy: Arc<dyn PlacementPolicy>,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        use awaken_time::WallClockExt as _;
        let expiry = u64::try_from(receipt.expires_at_ms).map_err(DispatchError::unavailable)?;
        let request = receipt.request.clone();
        let cutoff = self
            .objects
            .claim_request_cutoff()
            .await
            .map_err(DispatchError::unavailable)?;
        if expiry <= cutoff {
            return Ok(None);
        }
        let clock = self.clock.as_ref();
        self.transact(
            (identity, request, requester, workers, policy, lease_ms),
            move |state, (identity, request, requester, workers, policy, lease_ms)| {
                Box::pin(async move {
                    let Some(remaining) = expiry
                        .checked_sub(clock.now_ms())
                        .filter(|remaining| *remaining > 0)
                    else {
                        return Ok(None);
                    };
                    let lease_ms = lease_ms.min(remaining);
                    if let Some(replay) = state.requested_claim(&identity, &request)? {
                        return Ok(replay);
                    }
                    let claimed = match &request.run_id {
                        Some(run) => state.claim_run_compatible(run, requester, lease_ms).await?,
                        None => {
                            state
                                .claim_placed(requester, workers, policy, lease_ms)
                                .await?
                        }
                    };
                    if let Some(claimed) = &claimed {
                        state.record_claim_request(
                            identity,
                            request,
                            claimed.clone(),
                            expiry,
                            cutoff,
                        )?;
                    }
                    Ok(claimed)
                })
            },
        )
        .await
    }

    pub(crate) async fn backfill_claim_candidate(&self) -> Result<(), DispatchError> {
        for _ in 0..32 {
            let Some(observed) = self
                .objects
                .load(&self.session)
                .await
                .map_err(DispatchError::unavailable)?
            else {
                return Ok(());
            };
            let Some(payload) = observed.payload.as_deref() else {
                return Ok(());
            };
            let state = super::memory::MemoryDispatchStore::from_snapshot(
                Some(payload),
                self.clock.clone(),
            )?;
            let next_claim_at_ms = state.next_claim_at()?;
            if next_claim_at_ms.is_none() {
                return Ok(());
            }
            let update = SessionObjectUpdate {
                next_claim_at_ms,
                markers: state.publication_markers()?,
                ..SessionObjectUpdate::snapshot(payload.to_owned())
            };
            if self
                .objects
                .replace(&observed, &update)
                .await
                .map_err(DispatchError::unavailable)?
            {
                return Ok(());
            }
        }
        Err(DispatchError::unavailable(
            "Session changed during claim index backfill",
        ))
    }

    async fn transact<'a, A, T, F>(&'a self, args: A, invoke: F) -> Result<T, DispatchError>
    where
        A: Clone + Send + Sync + 'a,
        T: Send,
        F: Fn(
                Arc<super::memory::MemoryDispatchStore>,
                A,
            ) -> futures_util::future::BoxFuture<'a, Result<T, DispatchError>>
            + Send
            + Sync
            + 'a,
    {
        for _ in 0..32 {
            let observed = self
                .objects
                .load(&self.session)
                .await
                .map_err(DispatchError::unavailable)?;
            if observed
                .as_ref()
                .is_some_and(|object| object.owner_scope != self.owner)
            {
                return Err(DispatchError::Rejected(
                    "Session owner scope mismatch".into(),
                ));
            }
            let state = Arc::new(super::memory::MemoryDispatchStore::from_snapshot(
                observed.as_ref().and_then(|o| o.payload.as_deref()),
                self.clock.clone(),
            )?);
            let before = state.snapshot()?;
            let result = invoke(state.clone(), args.clone()).await?;
            let after = state.snapshot()?;
            if after == before {
                return Ok(result);
            }
            let records = state.identity_records_since(&before, &self.session)?;
            let update = SessionObjectUpdate {
                payload: after,
                records,
                markers: state.publication_markers()?,
                condition: None,
                next_claim_at_ms: state.next_claim_at()?,
            };
            let applied = match &observed {
                Some(object) => self.objects.replace(object, &update).await,
                None => {
                    self.objects
                        .insert(&self.owner, &self.session, &update)
                        .await
                }
            }
            .map_err(DispatchError::unavailable)?;
            if applied {
                return Ok(result);
            }
        }
        Err(DispatchError::unavailable(
            "Session Dispatch contention exceeded retry bound",
        ))
    }
}
#[async_trait::async_trait]
impl DispatchEnqueue for ObjectDispatchPartition {
    async fn enqueue_with(
        &self,
        request: RunDispatch,
        options: SubmitOptions,
    ) -> Result<(), DispatchError> {
        if request.session_thread_id().0 != self.session {
            return Err(DispatchError::Rejected(
                "Dispatch belongs to another Session partition".into(),
            ));
        }
        self.transact((request, options), |state, args| {
            Box::pin(async move { state.enqueue_with(args.0, args.1).await })
        })
        .await
    }
}

#[async_trait::async_trait]
impl DispatchClaims for ObjectDispatchPartition {
    async fn resolve_claimed_session_run_reservation(
        &self,
        claim: &RunClaim,
        resolution: SessionRunReservationResolution,
    ) -> Result<SettleOutcome, DispatchError> {
        self.transact((claim, resolution), |state, args| {
            Box::pin(async move {
                state
                    .resolve_claimed_session_run_reservation(args.0, args.1)
                    .await
            })
        })
        .await
    }
    async fn claim_new_run(
        &self,
        request: RunDispatch,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        if request.session_thread_id().0 != self.session {
            return Err(DispatchError::Rejected(
                "Dispatch belongs to another Session partition".into(),
            ));
        }
        self.transact((request, owner, lease_ms, capabilities), |state, args| {
            Box::pin(async move { state.claim_new_run(args.0, args.1, args.2, args.3).await })
        })
        .await
    }
    async fn claim_new_run_compatible(
        &self,
        request: RunDispatch,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        if request.session_thread_id().0 != self.session {
            return Err(DispatchError::Rejected(
                "Dispatch belongs to another Session partition".into(),
            ));
        }
        self.transact((request, worker, lease_ms), |state, args| {
            Box::pin(async move { state.claim_new_run_compatible(args.0, args.1, args.2).await })
        })
        .await
    }
    async fn deliver_and_claim(
        &self,
        input: PendingInput,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        self.transact((input, owner, lease_ms, capabilities), |state, args| {
            Box::pin(async move {
                state
                    .deliver_and_claim(args.0, args.1, args.2, args.3)
                    .await
            })
        })
        .await
    }
    async fn deliver_and_claim_compatible(
        &self,
        input: PendingInput,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        self.transact((input, worker, lease_ms), |state, args| {
            Box::pin(async move {
                state
                    .deliver_and_claim_compatible(args.0, args.1, args.2)
                    .await
            })
        })
        .await
    }
    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        self.transact((owner, lease_ms, capabilities), |state, args| {
            Box::pin(async move { state.claim(args.0, args.1, args.2).await })
        })
        .await
    }
    async fn claim_compatible(
        &self,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        self.transact((worker, lease_ms), |state, args| {
            Box::pin(async move { state.claim_compatible(args.0, args.1).await })
        })
        .await
    }
    async fn claim_run(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        self.transact((run_id, owner, lease_ms, capabilities), |state, args| {
            Box::pin(async move { state.claim_run(args.0, args.1, args.2, args.3).await })
        })
        .await
    }
    async fn claim_retry_exhausted(
        &self,
        owner: &str,
        lease_ms: u64,
        max_attempts: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        self.transact((owner, lease_ms, max_attempts), |state, args| {
            Box::pin(async move { state.claim_retry_exhausted(args.0, args.1, args.2).await })
        })
        .await
    }
    async fn claim_run_compatible(
        &self,
        run_id: &RunId,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        self.transact((run_id, worker, lease_ms), |state, args| {
            Box::pin(async move { state.claim_run_compatible(args.0, args.1, args.2).await })
        })
        .await
    }
}

#[async_trait::async_trait]
impl DispatchExecution for ObjectDispatchPartition {
    async fn close_attempt(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        self.transact((claim,), |state, args| {
            Box::pin(async move { state.close_attempt(args.0).await })
        })
        .await
    }
    async fn renew_lease(
        &self,
        claim: &RunClaim,
        lease_ms: u64,
    ) -> Result<LeaseRenewalOutcome, DispatchError> {
        self.transact((claim, lease_ms), |state, args| {
            Box::pin(async move { state.renew_lease(args.0, args.1).await })
        })
        .await
    }
    async fn begin_attempt(
        &self,
        claim: &RunClaim,
        _binding: Option<&awaken_dispatch_contract::dispatch::AttemptExecutionBinding>,
    ) -> Result<awaken_dispatch_contract::AttemptAdmission, DispatchError> {
        self.transact((claim, _binding), |state, args| {
            Box::pin(async move { state.begin_attempt(args.0, args.1).await })
        })
        .await
    }
    async fn handoff_attempt(
        &self,
        claim: &RunClaim,
        _binding: Option<&awaken_dispatch_contract::dispatch::AttemptExecutionBinding>,
    ) -> Result<awaken_dispatch_contract::AttemptAdmission, DispatchError> {
        self.transact((claim, _binding), |state, args| {
            Box::pin(async move { state.handoff_attempt(args.0, args.1).await })
        })
        .await
    }
    async fn finish_attempt(
        &self,
        claim: &RunClaim,
        cause: Option<awaken_dispatch_contract::AttemptReturnCause>,
    ) -> Result<SettleOutcome, DispatchError> {
        self.transact((claim, cause), |state, args| {
            Box::pin(async move { state.finish_attempt(args.0, args.1).await })
        })
        .await
    }
    async fn relinquish_claim(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        self.transact((claim,), |state, args| {
            Box::pin(async move { state.relinquish_claim(args.0).await })
        })
        .await
    }
    async fn defer_claim(
        &self,
        claim: &RunClaim,
        retry_after_ms: u64,
    ) -> Result<SettleOutcome, DispatchError> {
        self.transact((claim, retry_after_ms), |state, args| {
            Box::pin(async move { state.defer_claim(args.0, args.1).await })
        })
        .await
    }
    async fn retry_claim(
        &self,
        claim: &RunClaim,
        retry_after_ms: u64,
        max_attempts: u64,
    ) -> Result<SettleOutcome, DispatchError> {
        self.transact((claim, retry_after_ms, max_attempts), |state, args| {
            Box::pin(async move { state.retry_claim(args.0, args.1, args.2).await })
        })
        .await
    }
    async fn record_credential_realization(
        &self,
        claim: &RunClaim,
        receipt: CredentialRealizationReceipt,
    ) -> Result<SettleOutcome, DispatchError> {
        self.transact((claim, receipt), |state, args| {
            Box::pin(async move { state.record_credential_realization(args.0, args.1).await })
        })
        .await
    }
    async fn bind_sandbox(
        &self,
        claim: &RunClaim,
        sandbox_ref: &str,
    ) -> Result<SettleOutcome, DispatchError> {
        self.transact((claim, sandbox_ref), |state, args| {
            Box::pin(async move { state.bind_sandbox(args.0, args.1).await })
        })
        .await
    }
}

#[async_trait::async_trait]
impl DispatchClaimOwnership for ObjectDispatchPartition {
    async fn claim_is_current(&self, claim: &RunClaim) -> Result<bool, DispatchError> {
        self.transact((claim,), |state, args| {
            Box::pin(async move { state.claim_is_current(args.0).await })
        })
        .await
    }
}

#[async_trait::async_trait]
impl DispatchSettlement for ObjectDispatchPartition {
    async fn settle(
        &self,
        run_id: &RunId,
        generation: u64,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<SettleOutcome, DispatchError> {
        let Ok(epoch) = awaken_lease_contract::fence::Epoch::try_from(generation) else {
            return Ok(SettleOutcome::Fenced);
        };
        self.transact((run_id, epoch.get(), outcome, consumed), |state, args| {
            Box::pin(async move { state.settle(args.0, args.1, args.2, args.3).await })
        })
        .await
    }
}

#[async_trait::async_trait]
impl InboxRead for ObjectDispatchPartition {
    async fn list(&self, thread_id: &ThreadId) -> Result<Vec<PendingRecord>, DispatchError> {
        self.transact((thread_id,), |state, args| {
            Box::pin(async move { state.list(args.0).await })
        })
        .await
    }
}

#[async_trait::async_trait]
impl DispatchAdministration for ObjectDispatchPartition {
    async fn cancel(&self, run_id: &RunId) -> Result<Option<ThreadId>, DispatchError> {
        self.transact((run_id,), |state, args| {
            Box::pin(async move { state.cancel(args.0).await })
        })
        .await
    }
    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        self.transact((run_id,), |state, args| {
            Box::pin(async move { state.requeue(args.0).await })
        })
        .await
    }
}

#[async_trait::async_trait]
impl DispatchReads for ObjectDispatchPartition {
    async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError> {
        self.transact((), |state, _args| {
            Box::pin(async move { state.dead_letters().await })
        })
        .await
    }
    async fn superseded(&self) -> Result<Vec<RunId>, DispatchError> {
        self.transact((), |state, _args| {
            Box::pin(async move { state.superseded().await })
        })
        .await
    }
    async fn list_dispatches(&self) -> Result<Vec<DispatchSummary>, DispatchError> {
        self.transact((), |state, _args| {
            Box::pin(async move { state.list_dispatches().await })
        })
        .await
    }
}

#[async_trait::async_trait]
impl DispatchMaintenance for ObjectDispatchPartition {
    async fn quarantine_retry_exhausted(
        &self,
        max_attempts: u64,
        expired_before_ms: u64,
    ) -> Result<usize, DispatchError> {
        self.transact((max_attempts, expired_before_ms), |state, args| {
            Box::pin(async move { state.quarantine_retry_exhausted(args.0, args.1).await })
        })
        .await
    }
    async fn purge_dead_letters(&self) -> Result<usize, DispatchError> {
        self.transact((), |state, _args| {
            Box::pin(async move { state.purge_dead_letters().await })
        })
        .await
    }
    async fn purge_dead_letters_before(&self, cutoff_ms: u64) -> Result<usize, DispatchError> {
        self.transact((cutoff_ms,), |state, args| {
            Box::pin(async move { state.purge_dead_letters_before(args.0).await })
        })
        .await
    }
}

#[async_trait::async_trait]
impl DispatchAttemptControl for ObjectDispatchPartition {
    async fn finish_attempt_closure(
        &self,
        record: &awaken_dispatch_contract::AttemptClosureRecord,
    ) -> Result<SettleOutcome, DispatchError> {
        self.transact((record,), |state, args| {
            Box::pin(async move { state.finish_attempt_closure(args.0).await })
        })
        .await
    }
    async fn admit_attempt_guarded(
        &self,
        claim: &RunClaim,
        binding: Option<&awaken_dispatch_contract::dispatch::AttemptExecutionBinding>,
        guard: awaken_worker_contract::WorkerIncarnationGuard,
        handoff: bool,
    ) -> Result<awaken_dispatch_contract::AttemptAdmission, DispatchError> {
        // Retain the original Registry exclusion across every CAS retry.
        // The transient kernel borrows its snapshot; it never replaces authority.
        let snapshot = guard.snapshot().clone();
        let result = self
            .transact((claim, binding, snapshot, handoff), |state, args| {
                Box::pin(async move {
                    state
                        .admit_attempt_guarded(
                            args.0,
                            args.1,
                            awaken_worker_contract::WorkerIncarnationGuard::new((), args.2),
                            args.3,
                        )
                        .await
                })
            })
            .await;
        guard.release().await.map_err(DispatchError::unavailable)?;
        result
    }
}

#[async_trait::async_trait]
impl DispatchControlClaims for ObjectDispatchPartition {
    async fn claim_placed(
        &self,
        requester: &WorkerSnapshot,
        workers: Vec<WorkerSnapshot>,
        policy: Arc<dyn PlacementPolicy>,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        self.transact((requester, workers, policy, lease_ms), |state, args| {
            Box::pin(async move { state.claim_placed(args.0, args.1, args.2, args.3).await })
        })
        .await
    }
    async fn claim_for_terminal_recovery(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        self.transact((run_id, owner, lease_ms), |state, args| {
            Box::pin(async move {
                state
                    .claim_for_terminal_recovery(args.0, args.1, args.2)
                    .await
            })
        })
        .await
    }
}

#[async_trait::async_trait]
impl DispatchAttemptRecordReads for ObjectDispatchPartition {
    async fn attempt_admissions_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
        session_thread: &ThreadId,
        member_threads: &[ThreadId],
    ) -> Result<DispatchPage, DispatchError> {
        self.transact(
            (cursor, limit, session_thread, member_threads),
            |state, args| {
                Box::pin(async move {
                    state
                        .attempt_admissions_after(args.0, args.1, args.2, args.3)
                        .await
                })
            },
        )
        .await
    }
    async fn attempt_closure_records_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
        run: Option<&RunId>,
    ) -> Result<awaken_dispatch_contract::AttemptClosurePage, DispatchError> {
        self.transact((cursor, limit, run), |state, args| {
            Box::pin(async move {
                state
                    .attempt_closure_records_after(args.0, args.1, args.2)
                    .await
            })
        })
        .await
    }
}

#[async_trait::async_trait]
impl DispatchControlReads for ObjectDispatchPartition {
    async fn physical_slot_occupied_guarded(
        &self,
        guard: &mut awaken_worker_contract::WorkerIncarnationGuard,
    ) -> Result<bool, DispatchError> {
        // The caller retains the original Registry exclusion throughout this
        // read. The transient kernel receives only that guarded snapshot, as
        // on admission; it does not establish a second source of authority.
        self.transact(guard.snapshot().clone(), |state, snapshot| {
            Box::pin(async move {
                state
                    .physical_slot_occupied_guarded(
                        &mut awaken_worker_contract::WorkerIncarnationGuard::new((), snapshot),
                    )
                    .await
            })
        })
        .await
    }

    async fn current_credential_binding(
        &self,
        claim: &RunClaim,
        candidate: &CandidateFingerprint,
    ) -> Result<Option<AttemptCredentialBinding>, DispatchError> {
        self.transact((claim, candidate), |state, args| {
            Box::pin(async move { state.current_credential_binding(args.0, args.1).await })
        })
        .await
    }
    async fn load_physical_attempt(
        &self,
        run_id: &RunId,
    ) -> Result<Option<awaken_dispatch_contract::dispatch::PhysicalAttempt>, DispatchError> {
        self.transact((run_id,), |state, args| {
            Box::pin(async move { state.load_physical_attempt(args.0).await })
        })
        .await
    }
    async fn completion_events_after(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<DispatchCompletion>, DispatchError> {
        self.transact((after_sequence, limit), |state, args| {
            Box::pin(async move { state.completion_events_after(args.0, args.1).await })
        })
        .await
    }
    async fn dispatch_depths(&self) -> Result<Option<DispatchDepths>, DispatchError> {
        self.transact((), |state, _args| {
            Box::pin(async move { state.dispatch_depths().await })
        })
        .await
    }
    async fn scan_dispatch_ids(
        &self,
        after: Option<&RunId>,
        limit: usize,
    ) -> Result<Vec<RunId>, DispatchError> {
        self.transact((after, limit), |state, args| {
            Box::pin(async move { state.scan_dispatch_ids(args.0, args.1).await })
        })
        .await
    }
    async fn dispatch_summary(
        &self,
        run_id: &RunId,
    ) -> Result<Option<DispatchSummary>, DispatchError> {
        self.transact((run_id,), |state, args| {
            Box::pin(async move { state.dispatch_summary(args.0).await })
        })
        .await
    }
}

#[async_trait::async_trait]
impl Inbox for ObjectDispatchPartition {
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError> {
        self.transact((input,), |state, args| {
            Box::pin(async move { state.append(args.0).await })
        })
        .await
    }
    async fn retract(
        &self,
        message_id: &str,
        expected_revision: u64,
    ) -> Result<CasOutcome, DispatchError> {
        self.transact((message_id, expected_revision), |state, args| {
            Box::pin(async move { state.retract(args.0, args.1).await })
        })
        .await
    }
    async fn edit(
        &self,
        message_id: &str,
        expected_revision: u64,
        result: ResumeResult,
    ) -> Result<CasOutcome, DispatchError> {
        self.transact((message_id, expected_revision, result), |state, args| {
            Box::pin(async move { state.edit(args.0, args.1, args.2).await })
        })
        .await
    }
}

#[async_trait::async_trait]
impl Outbox for ObjectDispatchPartition {
    async fn stage(&self, input: PendingInput) -> Result<bool, DispatchError> {
        self.transact((input,), |state, args| {
            Box::pin(async move { state.stage(args.0).await })
        })
        .await
    }
    async fn relay(&self) -> Result<usize, DispatchError> {
        self.transact((), |state, _args| {
            Box::pin(async move { state.relay().await })
        })
        .await
    }
}

#[async_trait::async_trait]
impl SessionContinuationOutbox for ObjectDispatchPartition {
    async fn stage_session_resume(
        &self,
        input: PendingInput,
        thread: &ThreadId,
        prior: Option<u64>,
        epoch: Option<u64>,
    ) -> Result<bool, DispatchError> {
        self.transact((input, thread, prior, epoch), |state, args| {
            Box::pin(async move {
                state
                    .stage_session_resume(args.0, args.1, args.2, args.3)
                    .await
            })
        })
        .await
    }
    async fn relay_and_enqueue(
        &self,
        input: PendingInput,
        request: RunDispatch,
        admission: ContinuationAdmission,
    ) -> Result<(), DispatchError> {
        if request.session_thread_id().0 != self.session {
            return Err(DispatchError::Rejected(
                "Dispatch belongs to another Session partition".into(),
            ));
        }
        self.transact((input, request, admission), |state, args| {
            Box::pin(async move { state.relay_and_enqueue(args.0, args.1, args.2).await })
        })
        .await
    }
}

#[async_trait::async_trait]
impl DispatchOperationalFeed for ObjectDispatchPartition {
    async fn events_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
        run: Option<&RunId>,
    ) -> Result<DispatchPage, DispatchError> {
        self.transact((cursor, limit, run), |state, args| {
            Box::pin(async move { state.events_after(args.0, args.1, args.2).await })
        })
        .await
    }
}

#[async_trait::async_trait]
impl SessionRunAdmissionReads for ObjectDispatchPartition {
    async fn observe_session_run_admission(
        &self,
        run_id: &RunId,
    ) -> Result<awaken_dispatch_contract::SessionRunAdmissionObservation, DispatchError> {
        self.transact((run_id,), |state, args| {
            Box::pin(async move { state.observe_session_run_admission(args.0).await })
        })
        .await
    }
}

#[async_trait::async_trait]
impl SessionRunReservations for ObjectDispatchPartition {
    async fn adopt_pending_session_run(
        &self,
        expected_original: RunDispatch,
        command_fingerprint: awaken_session_types::SessionRunCommandFingerprint,
        owner: &str,
        lease_ms: u64,
    ) -> Result<Option<RunClaim>, DispatchError> {
        self.transact(
            (expected_original, command_fingerprint, owner, lease_ms),
            |state, args| {
                Box::pin(async move {
                    state
                        .adopt_pending_session_run(args.0, args.1, args.2, args.3)
                        .await
                })
            },
        )
        .await
    }
    async fn reserve_session_run(
        &self,
        request: RunDispatch,
        reservation_ttl_ms: u64,
    ) -> Result<SessionRunReservationOutcome, DispatchError> {
        if request.session_thread_id().0 != self.session {
            return Err(DispatchError::Rejected(
                "Dispatch belongs to another Session partition".into(),
            ));
        }
        self.transact((request, reservation_ttl_ms), |state, args| {
            Box::pin(async move { state.reserve_session_run(args.0, args.1).await })
        })
        .await
    }
    async fn activate_session_run_reservation(
        &self,
        run_id: &RunId,
        session_thread_id: &ThreadId,
        activity_version: u64,
    ) -> Result<SessionRunReservationActivation, DispatchError> {
        self.transact(
            (run_id, session_thread_id, activity_version),
            |state, args| {
                Box::pin(async move {
                    state
                        .activate_session_run_reservation(args.0, args.1, args.2)
                        .await
                })
            },
        )
        .await
    }
    async fn reject_session_run_reservation(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        self.transact((run_id,), |state, args| {
            Box::pin(async move { state.reject_session_run_reservation(args.0).await })
        })
        .await
    }
    async fn enqueue_session_child(
        &self,
        request: RunDispatch,
        admission: SessionChildAdmission,
    ) -> Result<(), DispatchError> {
        if request.session_thread_id().0 != self.session {
            return Err(DispatchError::Rejected(
                "Dispatch belongs to another Session partition".into(),
            ));
        }
        self.transact((request, admission), |state, args| {
            Box::pin(async move { state.enqueue_session_child(args.0, args.1).await })
        })
        .await
    }
}

impl ObjectDispatchPartition {
    async fn observe_authority(
        &self,
        claim: &RunClaim,
        reservation: bool,
    ) -> Result<Option<ClaimAuthority>, DispatchError> {
        let Some(object) = self
            .objects
            .load(&self.session)
            .await
            .map_err(DispatchError::unavailable)?
        else {
            return Ok(None);
        };
        let prefix = if reservation { "reservation" } else { "commit" };
        let active = format!(
            "{prefix}:{}:{}:{}:active",
            claim.run_id.0, claim.owner, claim.epoch
        );
        let cancelled = format!(
            "{prefix}:{}:{}:{}:cancelled",
            claim.run_id.0, claim.owner, claim.epoch
        );
        let Some(condition) = object
            .conditions
            .get(&active)
            .or_else(|| object.conditions.get(&cancelled))
        else {
            return Ok(None);
        };
        let state = super::memory::MemoryDispatchStore::from_snapshot(
            object.payload.as_deref(),
            self.clock.clone(),
        )?;
        Ok(state
            .conditional_claim_authority(
                claim,
                reservation,
                awaken_session_types::SessionPublicationCondition::new(condition.clone())
                    .map_err(DispatchError::unavailable)?,
            )?
            .map(ClaimAuthority::Conditional))
    }
}
#[async_trait::async_trait]
impl DispatchCommitAuthority for ObjectDispatchPartition {
    async fn worker_owns_run(
        &self,
        identity: &WorkerIdentity,
        run_id: &RunId,
    ) -> Result<Option<RunClaim>, DispatchError> {
        self.transact((identity, run_id), |state, args| {
            Box::pin(async move { state.worker_owns_run(args.0, args.1).await })
        })
        .await
    }
    async fn lock_commit_epoch(
        &self,
        _claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        Err(DispatchError::Rejected(
            "object storage requires conditional publication, not a held row lock".into(),
        ))
    }
    async fn acquire_commit_authority(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<ClaimAuthority>, DispatchError> {
        self.observe_authority(claim, false).await
    }
}
#[async_trait::async_trait]
impl SessionReservationAuthority for ObjectDispatchPartition {
    async fn lock_session_run_reservation_epoch(
        &self,
        _claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        Err(DispatchError::Rejected(
            "object storage requires conditional publication, not a held row lock".into(),
        ))
    }
    async fn acquire_session_run_reservation_authority(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<ClaimAuthority>, DispatchError> {
        self.observe_authority(claim, true).await
    }
}
