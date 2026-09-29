//! Fault-injection adapter forwarding to the sole Control authority.
use super::*;

#[cfg(feature = "test-support")]
#[async_trait::async_trait]
impl awaken_dispatch::ControlDispatchQueue for FlakyDispatchStore {
    async fn admit_attempt_guarded(
        &self,
        claim: &awaken_dispatch::RunClaim,
        binding: Option<&awaken_dispatch::AttemptExecutionBinding>,
        guard: awaken_dispatch_contract::WorkerIncarnationGuard,
        handoff: bool,
    ) -> Result<awaken_dispatch::AttemptAdmission, awaken_dispatch::DispatchError> {
        self.inner
            .admit_attempt_guarded(claim, binding, guard, handoff)
            .await
    }

    async fn finish_attempt_closure(
        &self,
        claim: &awaken_dispatch::RunClaim,
    ) -> Result<awaken_dispatch::SettleOutcome, awaken_dispatch::DispatchError> {
        self.inner.finish_attempt_closure(claim).await
    }
    async fn current_credential_binding(
        &self,
        claim: &awaken_dispatch::RunClaim,
        candidate: &awaken_runtime_contract::CandidateFingerprint,
    ) -> Result<
        Option<awaken_dispatch::AttemptCredentialBinding>,
        awaken_dispatch::DispatchError,
    > {
        self.inner
            .current_credential_binding(claim, candidate)
            .await
    }

    async fn reserve_session_run(
        &self,
        request: awaken_dispatch::RunDispatch,
        reservation_ttl_ms: u64,
    ) -> Result<awaken_dispatch::SessionRunReservationOutcome, awaken_dispatch::DispatchError>
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
        awaken_dispatch::SessionRunReservationActivation,
        awaken_dispatch::DispatchError,
    > {
        self.inner
            .activate_session_run_reservation(run_id, session_thread_id, session_activity_epoch)
            .await
    }

    async fn reject_session_run_reservation(
        &self,
        run_id: &RunId,
    ) -> Result<bool, awaken_dispatch::DispatchError> {
        self.inner.reject_session_run_reservation(run_id).await
    }

    async fn lock_session_run_reservation_epoch(
        &self,
        claim: &awaken_dispatch::RunClaim,
    ) -> Result<Option<awaken_dispatch::CommitEpochGuard>, awaken_dispatch::DispatchError>
    {
        self.inner.lock_session_run_reservation_epoch(claim).await
    }

    async fn enqueue_session_child(
        &self,
        request: awaken_dispatch::RunDispatch,
        admission: awaken_dispatch::SessionChildAdmission,
    ) -> Result<(), awaken_dispatch::DispatchError> {
        self.inner.enqueue_session_child(request, admission).await
    }

    async fn claim_placed(
        &self,
        requester: &awaken_dispatch::WorkerSnapshot,
        workers: Vec<awaken_dispatch::WorkerSnapshot>,
        policy: Arc<dyn awaken_dispatch::PlacementPolicy>,
        lease_ms: u64,
    ) -> Result<Option<awaken_dispatch::Claimed>, awaken_dispatch::DispatchError> {
        self.inner
            .claim_placed(requester, workers, policy, lease_ms)
            .await
    }

    async fn claim_for_terminal_recovery(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
    ) -> Result<Option<awaken_dispatch::Claimed>, awaken_dispatch::DispatchError> {
        self.inner
            .claim_for_terminal_recovery(run_id, owner, lease_ms)
            .await
    }

    async fn load_physical_attempt(
        &self,
        run_id: &RunId,
    ) -> Result<Option<awaken_dispatch::PhysicalAttempt>, awaken_dispatch::DispatchError>
    {
        self.inner.load_physical_attempt(run_id).await
    }

    async fn completion_events_after(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<awaken_dispatch::DispatchCompletion>, awaken_dispatch::DispatchError>
    {
        self.inner
            .completion_events_after(after_sequence, limit)
            .await
    }

    async fn runnable_depth(&self) -> Result<Option<u64>, awaken_dispatch::DispatchError> {
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
        self.inner.runnable_depth().await
    }
    async fn scan_dispatch_ids(
        &self,
        after: Option<&RunId>,
        limit: usize,
    ) -> Result<Vec<RunId>, awaken_dispatch::DispatchError> {
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
    ) -> Result<Option<awaken_dispatch::DispatchSummary>, awaken_dispatch::DispatchError>
    {
        self.inner.dispatch_summary(run_id).await
    }
}
