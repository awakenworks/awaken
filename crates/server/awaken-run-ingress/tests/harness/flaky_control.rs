//! Fault-injection adapter forwarding to the sole Control authority.
use super::*;

#[cfg(feature = "test-support")]
#[async_trait::async_trait]
impl awaken_run_ingress_contract::DispatchAttemptControl for FlakyDispatchStore {
    async fn admit_attempt_guarded(
        &self,
        claim: &awaken_run_ingress::RunClaim,
        binding: Option<&awaken_run_ingress::AttemptExecutionBinding>,
        guard: awaken_run_ingress_contract::WorkerIncarnationGuard,
        handoff: bool,
    ) -> Result<awaken_run_ingress::AttemptAdmission, awaken_run_ingress::DispatchError> {
        self.inner
            .admit_attempt_guarded(claim, binding, guard, handoff)
            .await
    }

    async fn finish_attempt_closure(
        &self,
        record: &awaken_run_ingress::AttemptClosureRecord,
    ) -> Result<awaken_run_ingress::SettleOutcome, awaken_run_ingress::DispatchError> {
        self.inner.finish_attempt_closure(record).await
    }
}

#[cfg(feature = "test-support")]
#[async_trait::async_trait]
impl awaken_run_ingress_contract::DispatchControlReads for FlakyDispatchStore {
    async fn current_credential_binding(
        &self,
        claim: &awaken_run_ingress::RunClaim,
        candidate: &awaken_agent_contract::credential::CandidateFingerprint,
    ) -> Result<
        Option<awaken_run_ingress::AttemptCredentialBinding>,
        awaken_run_ingress::DispatchError,
    > {
        self.inner
            .current_credential_binding(claim, candidate)
            .await
    }

    async fn load_physical_attempt(
        &self,
        run_id: &RunId,
    ) -> Result<Option<awaken_run_ingress::PhysicalAttempt>, awaken_run_ingress::DispatchError>
    {
        self.inner.load_physical_attempt(run_id).await
    }

    async fn completion_events_after(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<awaken_run_ingress::DispatchCompletion>, awaken_run_ingress::DispatchError>
    {
        self.inner
            .completion_events_after(after_sequence, limit)
            .await
    }

    async fn dispatch_depths(
        &self,
    ) -> Result<Option<awaken_run_ingress::DispatchDepths>, awaken_run_ingress::DispatchError> {
        self.depth_reads.fetch_add(1, Ordering::SeqCst);
        if let Some(entered) = &self.depth_entered {
            entered.notify_one();
        }
        if let Some(release) = &self.depth_release {
            release
                .acquire()
                .await
                .expect("depth sample release")
                .forget();
        }
        self.inner.dispatch_depths().await
    }
    async fn scan_dispatch_ids(
        &self,
        after: Option<&RunId>,
        limit: usize,
    ) -> Result<Vec<RunId>, awaken_run_ingress::DispatchError> {
        self.scan_limits.lock().unwrap().push(limit);
        // Fault injection reuses this adapter rather than introducing a second
        // Dispatch implementation. Return all remaining IDs, violating the
        // bounded-read contract; the production scanner must still cap visits.
        if self.oversized_scan_page {
            let mut ids: Vec<_> = self
                .inner
                .list_dispatches()
                .await?
                .into_iter()
                .map(|row| row.run_id)
                .filter(|id| after.is_none_or(|cursor| id.0 > cursor.0))
                .collect();
            ids.sort_by(|left, right| left.0.cmp(&right.0));
            Ok(ids)
        } else {
            self.observe_maintenance(
                maintenance::Operation::Scan,
                self.inner.scan_dispatch_ids(after, limit),
            )
            .await
        }
    }

    async fn dispatch_summary(
        &self,
        run_id: &RunId,
    ) -> Result<Option<awaken_run_ingress::DispatchSummary>, awaken_run_ingress::DispatchError>
    {
        let mut summary = self.inner.dispatch_summary(run_id).await?;
        // The one existing read-fault seam changes only a returned delivery
        // label. Never change original physical identity, storage, or receipts.
        if let Some((subject, state)) = &self.summary_delivery_projection
            && subject == run_id
            && let Some(row) = &mut summary
        {
            row.state = *state;
        }
        Ok(summary)
    }
}

#[cfg(feature = "test-support")]
#[async_trait::async_trait]
impl awaken_run_ingress_contract::SessionRunAdmissionReads for FlakyDispatchStore {
    async fn observe_session_run_admission(
        &self,
        run_id: &RunId,
    ) -> Result<
        awaken_run_ingress_contract::SessionRunAdmissionObservation,
        awaken_run_ingress::DispatchError,
    > {
        self.inner.observe_session_run_admission(run_id).await
    }
}

#[cfg(feature = "test-support")]
#[async_trait::async_trait]
impl awaken_run_ingress_contract::SessionRunReservations for FlakyDispatchStore {
    async fn reserve_session_run(
        &self,
        request: awaken_run_ingress::RunDispatch,
        reservation_ttl_ms: u64,
    ) -> Result<awaken_run_ingress::SessionRunReservationOutcome, awaken_run_ingress::DispatchError>
    {
        self.inner
            .reserve_session_run(request, reservation_ttl_ms)
            .await
    }

    async fn activate_session_run_reservation(
        &self,
        run_id: &RunId,
        session_thread_id: &ThreadId,
        session_activity_epoch: u64,
    ) -> Result<
        awaken_run_ingress::SessionRunReservationActivation,
        awaken_run_ingress::DispatchError,
    > {
        self.inner
            .activate_session_run_reservation(run_id, session_thread_id, session_activity_epoch)
            .await
    }

    async fn reject_session_run_reservation(
        &self,
        run_id: &RunId,
    ) -> Result<bool, awaken_run_ingress::DispatchError> {
        self.inner.reject_session_run_reservation(run_id).await
    }

    async fn enqueue_session_child(
        &self,
        request: awaken_run_ingress::RunDispatch,
        admission: awaken_run_ingress::SessionChildAdmission,
    ) -> Result<(), awaken_run_ingress::DispatchError> {
        self.inner.enqueue_session_child(request, admission).await
    }
}

#[cfg(feature = "test-support")]
#[async_trait::async_trait]
impl awaken_run_ingress_contract::SessionReservationAuthority for FlakyDispatchStore {
    async fn lock_session_run_reservation_epoch(
        &self,
        claim: &awaken_run_ingress::RunClaim,
    ) -> Result<Option<awaken_run_ingress::CommitEpochGuard>, awaken_run_ingress::DispatchError>
    {
        self.inner.lock_session_run_reservation_epoch(claim).await
    }
}

#[cfg(feature = "test-support")]
#[async_trait::async_trait]
impl awaken_run_ingress_contract::DispatchControlClaims for FlakyDispatchStore {
    async fn claim_placed(
        &self,
        requester: &awaken_run_ingress::WorkerSnapshot,
        workers: Vec<awaken_run_ingress::WorkerSnapshot>,
        policy: Arc<dyn awaken_run_ingress::PlacementPolicy>,
        lease_ms: u64,
    ) -> Result<Option<awaken_run_ingress::Claimed>, awaken_run_ingress::DispatchError> {
        self.inner
            .claim_placed(requester, workers, policy, lease_ms)
            .await
    }

    async fn claim_for_terminal_recovery(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
    ) -> Result<Option<awaken_run_ingress::Claimed>, awaken_run_ingress::DispatchError> {
        self.inner
            .claim_for_terminal_recovery(run_id, owner, lease_ms)
            .await
    }
}

use awaken_run_ingress_contract::DispatchReads as _;

#[cfg(feature = "test-support")]
#[async_trait::async_trait]
impl awaken_run_ingress_contract::DispatchAttemptRecordReads for FlakyDispatchStore {
    async fn attempt_admissions_after(
        &self,
        cursor: awaken_run_ingress::DispatchCursor,
        limit: usize,
        session_thread: &awaken_agent_contract::agent::thread::Id,
        member_threads: &[awaken_agent_contract::agent::thread::Id],
    ) -> Result<awaken_run_ingress::DispatchPage, awaken_run_ingress::DispatchError> {
        self.inner
            .attempt_admissions_after(cursor, limit, session_thread, member_threads)
            .await
    }

    async fn attempt_closure_records_after(
        &self,
        cursor: awaken_run_ingress::DispatchCursor,
        limit: usize,
        run: Option<&RunId>,
    ) -> Result<awaken_run_ingress::AttemptClosurePage, awaken_run_ingress::DispatchError> {
        self.inner
            .attempt_closure_records_after(cursor, limit, run)
            .await
    }
}
