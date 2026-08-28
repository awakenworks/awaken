// PostgreSQL realization of the one neutral DispatchQueue contract. Included by
// `postgres.rs` so public type and implementation paths remain unchanged.
#[async_trait]
impl DispatchQueue for PostgresDispatchStore {
    async fn worker_owns_run(
        &self,
        identity: &crate::WorkerIdentity,
        run_id: &RunId,
        now_ms: u64,
    ) -> Result<Option<RunClaim>, DispatchError> {
        current_worker_claim(&self.pool, NS, identity, run_id, now_ms).await
    }

    async fn reserve_session_run(
        &self,
        request: RunDispatch,
        reservation_deadline_ms: u64,
    ) -> Result<SessionRunReservationOutcome, DispatchError> {
        let deadline = validate_session_run_reservation_request(&request, reservation_deadline_ms)?;
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        lock_run_identity(&mut tx, &request.run_id().0).await?;
        let live = sqlx::query(&format!(
            "SELECT status, request FROM {p}_dispatch WHERE run_id = $1"
        ))
        .bind(&request.run_id().0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        if let Some(row) = live {
            let status: String = row.try_get("status").map_err(reject)?;
            let Json(stored): Json<RunDispatch> = row.try_get("request").map_err(reject)?;
            let state = DispatchState::from_db(&status).ok_or_else(|| {
                DispatchError::Rejected(format!("unknown persisted dispatch state `{status}`"))
            })?;
            let outcome = classify_live_session_run_reservation(&stored, state, &request);
            tx.commit().await.map_err(reject)?;
            return Ok(outcome);
        }
        let completed: Option<Option<String>> = sqlx::query_scalar(&format!(
            "SELECT request_fingerprint FROM {p}_dispatch_completion WHERE run_id = $1"
        ))
        .bind(&request.run_id().0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        if let Some(request_fingerprint) = completed {
            let outcome = classify_completed_session_run_reservation(
                request_fingerprint.as_deref(),
                &request,
            );
            tx.commit().await.map_err(reject)?;
            return Ok(outcome);
        }
        insert_dispatch_with_state(
            &mut tx,
            p,
            &request,
            &SubmitOptions::default(),
            "reserved",
            Some(crate::clock::db_millis(deadline)),
        )
        .await?;
        tx.commit().await.map_err(reject)?;
        Ok(SessionRunReservationOutcome::Reserved)
    }

    async fn activate_session_run_reservation(
        &self,
        run_id: &RunId,
        session_thread_id: &ThreadId,
        session_activity_epoch: u64,
    ) -> Result<SessionRunReservationActivation, DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        lock_run_identity(&mut tx, &run_id.0).await?;
        let current = sqlx::query(&format!(
            "SELECT status, request FROM {p}_dispatch WHERE run_id = $1 FOR UPDATE"
        ))
        .bind(&run_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        let Some(row) = current else {
            let completed: bool = sqlx::query_scalar(&format!(
                "SELECT EXISTS(SELECT 1 FROM {p}_dispatch_completion WHERE run_id = $1)"
            ))
            .bind(&run_id.0)
            .fetch_one(&mut *tx)
            .await
            .map_err(reject)?;
            let outcome = classify_session_run_reservation_activation(
                None,
                completed,
                session_thread_id,
                session_activity_epoch,
            )?
            .expect("missing reservation always has a closed outcome");
            tx.commit().await.map_err(reject)?;
            return Ok(outcome);
        };
        let status: String = row.try_get("status").map_err(reject)?;
        let Json(mut request): Json<RunDispatch> = row.try_get("request").map_err(reject)?;
        let state = DispatchState::from_db(&status).ok_or_else(|| {
            DispatchError::Rejected(format!("unknown persisted dispatch state `{status}`"))
        })?;
        let classified = classify_session_run_reservation_activation(
            Some((&request, state)),
            false,
            session_thread_id,
            session_activity_epoch,
        )?;
        let outcome = if let Some(outcome) = classified {
            outcome
        } else {
            request.session_activity_epoch = Some(session_activity_epoch);
            let changed = sqlx::query(&format!(
                "UPDATE {p}_dispatch SET request = $1, status = 'pending', \
                 lease_owner = NULL, lease_until = NULL WHERE run_id = $2 AND status = 'reserved'"
            ))
            .bind(Json(&request))
            .bind(&run_id.0)
            .execute(&mut *tx)
            .await
            .map_err(reject)?
            .rows_affected();
            if changed == 1 {
                SessionRunReservationActivation::Activated
            } else {
                SessionRunReservationActivation::RecoveryClaimed
            }
        };
        tx.commit().await.map_err(reject)?;
        Ok(outcome)
    }

    async fn reject_session_run_reservation(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        lock_run_identity(&mut tx, &run_id.0).await?;
        let changed = sqlx::query(&format!(
            "DELETE FROM {p}_dispatch WHERE run_id = $1 AND status = 'reserved'"
        ))
        .bind(&run_id.0)
        .execute(&mut *tx)
        .await
        .map_err(reject)?
        .rows_affected();
        if changed == 1 {
            sqlx::query(&format!("DELETE FROM {p}_pending WHERE run_id = $1"))
                .bind(&run_id.0)
                .execute(&mut *tx)
                .await
                .map_err(reject)?;
        }
        tx.commit().await.map_err(reject)?;
        Ok(changed == 1)
    }

    async fn resolve_claimed_session_run_reservation(
        &self,
        claim: &RunClaim,
        resolution: SessionRunReservationResolution,
    ) -> Result<SettleOutcome, DispatchError> {
        let resolution = validate_session_run_reservation_resolution(resolution)?;
        let p = NS;
        let claim_epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let row = sqlx::query(&format!(
            "SELECT request FROM {p}_dispatch WHERE run_id = $1 \
             AND status = 'reservation_running' AND lease_owner = $2 \
             AND lease_epoch = $3 FOR UPDATE"
        ))
        .bind(&claim.run_id.0)
        .bind(&claim.owner)
        .bind(claim_epoch)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        let Some(row) = row else {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        };
        let Json(mut request): Json<RunDispatch> = row.try_get("request").map_err(reject)?;
        let changed = match resolution {
            SessionRunReservationResolution::Admitted {
                session_activity_epoch,
            } => {
                request.session_activity_epoch = Some(session_activity_epoch);
                sqlx::query(&format!(
                    "UPDATE {p}_dispatch SET request = $1, status = 'pending', \
                     lease_owner = NULL, lease_until = NULL, worker_assignment = NULL, \
                     credential_bindings = NULL, credential_receipts = NULL \
                     WHERE run_id = $2 AND status = 'reservation_running' \
                     AND lease_owner = $3 AND lease_epoch = $4"
                ))
                .bind(Json(&request))
                .bind(&claim.run_id.0)
                .bind(&claim.owner)
                .bind(claim_epoch)
                .execute(&mut *tx)
                .await
                .map_err(reject)?
                .rows_affected()
            }
            SessionRunReservationResolution::Retry {
                reservation_deadline_ms,
            } => sqlx::query(&format!(
                "UPDATE {p}_dispatch SET status = 'reserved', lease_owner = NULL, \
                     lease_until = $1, worker_assignment = NULL, credential_bindings = NULL, \
                     credential_receipts = NULL, created_at = CURRENT_TIMESTAMP \
                     WHERE run_id = $2 AND status = 'reservation_running' \
                     AND lease_owner = $3 AND lease_epoch = $4"
            ))
            .bind(crate::clock::db_millis(reservation_deadline_ms))
            .bind(&claim.run_id.0)
            .bind(&claim.owner)
            .bind(claim_epoch)
            .execute(&mut *tx)
            .await
            .map_err(reject)?
            .rows_affected(),
            SessionRunReservationResolution::Rejected => {
                let changed = sqlx::query(&format!(
                    "DELETE FROM {p}_dispatch WHERE run_id = $1 \
                     AND status = 'reservation_running' AND lease_owner = $2 \
                     AND lease_epoch = $3"
                ))
                .bind(&claim.run_id.0)
                .bind(&claim.owner)
                .bind(claim_epoch)
                .execute(&mut *tx)
                .await
                .map_err(reject)?
                .rows_affected();
                if changed == 1 {
                    sqlx::query(&format!("DELETE FROM {p}_pending WHERE run_id = $1"))
                        .bind(&claim.run_id.0)
                        .execute(&mut *tx)
                        .await
                        .map_err(reject)?;
                }
                changed
            }
        };
        if changed != 1 {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        }
        tx.commit().await.map_err(reject)?;
        Ok(SettleOutcome::Applied)
    }

    async fn enqueue_with(
        &self,
        request: RunDispatch,
        options: SubmitOptions,
    ) -> Result<(), DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        lock_run_identity(&mut tx, &request.run_id().0).await?;

        // Run-id idempotency survives successful completion: a live row or the
        // permanent completion tombstone makes the whole command a no-op. Check
        // before supersession so replay cannot mutate sibling dispatches.
        if exact_run_replay(&mut tx, p, &request).await? {
            tx.commit().await.map_err(reject)?;
            return Ok(());
        }

        validate_executable_dispatch_admission(&request)?;
        insert_new_dispatch(&mut tx, p, &request, &options).await?;
        tx.commit().await.map_err(reject)?;
        Ok(())
    }

    async fn enqueue_session_child(
        &self,
        request: RunDispatch,
        admission: SessionChildAdmission,
    ) -> Result<(), DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        lock_run_identity(&mut tx, &request.run_id().0).await?;
        if exact_run_replay(&mut tx, p, &request).await? {
            tx.commit().await.map_err(reject)?;
            return Ok(());
        }
        admit_session_child(&mut tx, p, &request, &admission).await?;
        insert_new_dispatch(&mut tx, p, &request, &SubmitOptions::default()).await?;
        tx.commit().await.map_err(reject)?;
        Ok(())
    }

    async fn claim_new_run(
        &self,
        request: RunDispatch,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        lock_run_identity(&mut tx, &request.run_id().0).await?;
        if exact_run_replay(&mut tx, p, &request).await? {
            let run_id = request.run_id().clone();
            let claimed = claim_exact_transaction(
                &mut tx,
                &run_id,
                owner,
                lease_ms,
                now_ms,
                None,
                capabilities,
            )
            .await?;
            tx.commit().await.map_err(reject)?;
            return Ok(claimed);
        }
        validate_executable_dispatch_admission(&request)?;
        if !can_claim_locally(&request.placement) {
            tx.commit().await.map_err(reject)?;
            return Ok(None);
        }
        insert_new_dispatch(&mut tx, p, &request, &SubmitOptions::default()).await?;
        let run_id = request.run_id().clone();
        let claimed = claim_exact_transaction(
            &mut tx,
            &run_id,
            owner,
            lease_ms,
            now_ms,
            None,
            capabilities,
        )
        .await?;
        tx.commit().await.map_err(reject)?;
        Ok(claimed)
    }

    async fn claim_new_run_compatible(
        &self,
        request: RunDispatch,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let p = NS;
        let owner = worker.identity.lease_owner();
        let mut tx = self.pool.begin().await.map_err(reject)?;
        lock_run_identity(&mut tx, &request.run_id().0).await?;
        if exact_run_replay(&mut tx, p, &request).await? {
            let run_id = request.run_id().clone();
            let claimed = claim_exact_transaction(
                &mut tx,
                &run_id,
                &owner,
                lease_ms,
                now_ms,
                Some(worker),
                &installed_worker_credential_capabilities(worker)?,
            )
            .await?;
            tx.commit().await.map_err(reject)?;
            return Ok(claimed);
        }
        validate_executable_dispatch_admission(&request)?;
        if can_assign(worker, &request.placement, None, false, now_ms).is_err() {
            tx.commit().await.map_err(reject)?;
            return Ok(None);
        }
        insert_new_dispatch(&mut tx, p, &request, &SubmitOptions::default()).await?;
        let run_id = request.run_id().clone();
        let claimed = claim_exact_transaction(
            &mut tx,
            &run_id,
            &owner,
            lease_ms,
            now_ms,
            Some(worker),
            &installed_worker_credential_capabilities(worker)?,
        )
        .await?;
        tx.commit().await.map_err(reject)?;
        Ok(claimed)
    }

    async fn deliver_and_claim(
        &self,
        input: PendingInput,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let input = normalize_pending_millis(input);
        let run_id = input.run_id.clone();
        let mut tx = self.pool.begin().await.map_err(reject)?;
        append_pending_transaction(&mut tx, NS, &input).await?;
        let claimed = claim_exact_transaction(
            &mut tx,
            &run_id,
            owner,
            lease_ms,
            now_ms,
            None,
            capabilities,
        )
        .await?;
        tx.commit().await.map_err(reject)?;
        Ok(claimed)
    }

    async fn deliver_and_claim_compatible(
        &self,
        input: PendingInput,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let input = normalize_pending_millis(input);
        let run_id = input.run_id.clone();
        let mut tx = self.pool.begin().await.map_err(reject)?;
        append_pending_transaction(&mut tx, NS, &input).await?;
        let claimed = claim_exact_transaction(
            &mut tx,
            &run_id,
            &worker.identity.lease_owner(),
            lease_ms,
            now_ms,
            Some(worker),
            &installed_worker_credential_capabilities(worker)?,
        )
        .await?;
        tx.commit().await.map_err(reject)?;
        Ok(claimed)
    }

    async fn lock_commit_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        self.lock_claim_epoch_in_status(claim, "running").await
    }

    async fn lock_session_run_reservation_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        self.lock_claim_epoch_in_status(claim, "reservation_running")
            .await
    }

    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let p = NS;
        // Candidate discovery is deliberately read-only. The exact transition
        // below is the sole claim algorithm and always acquires the per-Thread
        // advisory lock before its row lock. Locking a candidate row here would
        // invert that order against exact claimers and can deadlock active-active
        // Postgres schedulers.
        let local_eligible = "(d.cancel_requested = 1 OR (\
            COALESCE(d.request #>> '{placement,location}', 'remote_preferred') \
                <> 'remote_required' AND \
            jsonb_array_length(COALESCE(\
                d.request #> '{placement,required_credentials}', '[]'::jsonb)) = 0))";
        // ADR-0022 single writer is an open-Run fence: an Awaiting Run keeps
        // ownership until its exact reply/cancel resumes that same row. V0012
        // remains the hard backstop for concurrent transitions into Running.
        let thread_available = thread_available_for_claim(p);
        let candidates = sqlx::query_scalar::<_, String>(&format!(
            "SELECT d.run_id FROM {p}_dispatch d WHERE \
             (d.status IN ('reserved', 'reservation_running') AND \
               d.lease_until IS NOT NULL AND d.lease_until < $1 AND {}) OR \
             (d.status = 'running' AND d.lease_until IS NOT NULL AND \
               d.lease_until < $1 AND {local_eligible}) OR \
             (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS ( \
               SELECT 1 FROM {p}_pending pe WHERE pe.run_id = d.run_id AND \
                 (pe.available_at IS NULL OR pe.available_at <= $1))) AND \
               {thread_available} AND {local_eligible}) OR \
             (d.status = 'pending' AND {thread_available} AND {local_eligible}) \
             ORDER BY CASE \
               WHEN d.cancel_requested = 1 THEN 0 \
               WHEN d.status IN ('reserved', 'reservation_running') THEN 1 \
               WHEN d.status = 'running' THEN 2 \
               WHEN d.status = 'awaiting' THEN 3 ELSE 4 END, \
               d.priority DESC, d.created_at",
            no_running_peer(p)
        ))
        .bind(crate::clock::db_millis(now_ms))
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        for run_id in candidates {
            let mut tx = self.pool.begin().await.map_err(reject)?;
            let claimed = claim_exact_transaction(
                &mut tx,
                &RunId(run_id),
                owner,
                lease_ms,
                now_ms,
                None,
                capabilities,
            )
            .await?;
            tx.commit().await.map_err(reject)?;
            if claimed.is_some() {
                return Ok(claimed);
            }
        }
        Ok(None)
    }

    async fn claim_compatible(
        &self,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let p = NS;
        let thread_available = thread_available_for_claim(p);
        let rows = sqlx::query(&format!(
            "SELECT d.run_id, d.request, d.sandbox, d.worker_assignment, d.status, d.cancel_requested, d.lease_epoch FROM {p}_dispatch d WHERE \
             (d.status IN ('reserved', 'reservation_running') AND \
               d.lease_until IS NOT NULL AND d.lease_until < $1 AND {}) OR \
             (d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < $1) OR \
             (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS (SELECT 1 FROM {p}_pending pe \
               WHERE pe.run_id = d.run_id AND (pe.available_at IS NULL OR pe.available_at <= $1)) \
               ) AND {thread_available}) OR \
             (d.status = 'pending' AND {thread_available}) \
             ORDER BY CASE WHEN d.cancel_requested = 1 THEN 0 WHEN d.status = 'running' THEN 1 WHEN d.status = 'awaiting' THEN 2 ELSE 3 END, \
                      d.priority DESC, d.created_at",
            no_running_peer(p)
        ))
        .bind(crate::clock::db_millis(now_ms))
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        let capabilities = installed_worker_credential_capabilities(worker)?;
        let mut selected = Vec::new();
        for row in rows {
            let Json(request): Json<RunDispatch> = row.try_get("request").map_err(reject)?;
            let sandbox: Option<String> = row.try_get("sandbox").map_err(reject)?;
            let previous: Option<Json<WorkerAssignment>> =
                row.try_get("worker_assignment").map_err(reject)?;
            let status: String = row.try_get("status").map_err(reject)?;
            let cancellation_requested: i64 = row.try_get("cancel_requested").map_err(reject)?;
            let lease_epoch: i64 = row.try_get("lease_epoch").map_err(reject)?;
            let next_epoch = next_claim_epoch(lease_epoch)?;
            if matches!(status.as_str(), "reserved" | "reservation_running")
                || cancellation_requested != 0
                || (can_assign(
                    worker,
                    &request.placement,
                    previous.as_ref().map(|value| &value.0),
                    sandbox.is_some(),
                    now_ms,
                )
                .is_ok()
                    && can_admit_attempt_credentials(&request, &capabilities, next_epoch, now_ms))
            {
                selected.push(RunId(row.try_get("run_id").map_err(reject)?));
            }
        }
        for run_id in selected {
            let mut tx = self.pool.begin().await.map_err(reject)?;
            let claimed = claim_exact_transaction(
                &mut tx,
                &run_id,
                &worker.identity.lease_owner(),
                lease_ms,
                now_ms,
                Some(worker),
                &capabilities,
            )
            .await?;
            tx.commit().await.map_err(reject)?;
            if claimed.is_some() {
                return Ok(claimed);
            }
        }
        Ok(None)
    }

    async fn claim_placed(
        &self,
        requester: &WorkerSnapshot,
        workers: Vec<WorkerSnapshot>,
        policy: std::sync::Arc<dyn PlacementPolicy>,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let p = NS;
        let thread_available = thread_available_for_claim(p);
        let rows = sqlx::query(&format!(
            "SELECT d.run_id, d.request, d.sandbox, d.worker_assignment, d.status, d.cancel_requested, d.lease_epoch FROM {p}_dispatch d WHERE \
             (d.status IN ('reserved', 'reservation_running') AND \
               d.lease_until IS NOT NULL AND d.lease_until < $1 AND {}) OR \
             (d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < $1) OR \
             (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS (SELECT 1 FROM {p}_pending pe \
               WHERE pe.run_id = d.run_id AND (pe.available_at IS NULL OR pe.available_at <= $1)) \
               ) AND {thread_available}) OR \
             (d.status = 'pending' AND {thread_available}) \
             ORDER BY CASE WHEN d.cancel_requested = 1 THEN 0 WHEN d.status = 'running' THEN 1 WHEN d.status = 'awaiting' THEN 2 ELSE 3 END, \
                      d.priority DESC, d.created_at",
            no_running_peer(p)
        ))
        .bind(crate::clock::db_millis(now_ms))
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        let capabilities = installed_worker_credential_capabilities(requester)?;
        let mut selected = Vec::new();
        for row in rows {
            let Json(request): Json<RunDispatch> = row.try_get("request").map_err(reject)?;
            let sandbox: Option<String> = row.try_get("sandbox").map_err(reject)?;
            let previous: Option<Json<WorkerAssignment>> =
                row.try_get("worker_assignment").map_err(reject)?;
            let status: String = row.try_get("status").map_err(reject)?;
            let cancellation_requested: i64 = row.try_get("cancel_requested").map_err(reject)?;
            let lease_epoch: i64 = row.try_get("lease_epoch").map_err(reject)?;
            let next_epoch = next_claim_epoch(lease_epoch)?;
            if matches!(status.as_str(), "reserved" | "reservation_running")
                || cancellation_requested != 0
                || (policy_selects_requester(
                    &request,
                    policy.as_ref(),
                    DispatchPlacement {
                        recovered: status == "running",
                        previous: previous.as_ref().map(|value| &value.0),
                        sandbox_bound: sandbox.is_some(),
                        requester: &requester.identity,
                        workers: &workers,
                        now_ms,
                    },
                )? && can_admit_attempt_credentials(
                    &request,
                    &capabilities,
                    next_epoch,
                    now_ms,
                ))
            {
                selected.push(RunId(row.try_get("run_id").map_err(reject)?));
            }
        }
        for run_id in selected {
            let mut tx = self.pool.begin().await.map_err(reject)?;
            let claimed = claim_exact_transaction(
                &mut tx,
                &run_id,
                &requester.identity.lease_owner(),
                lease_ms,
                now_ms,
                Some(requester),
                &capabilities,
            )
            .await?;
            tx.commit().await.map_err(reject)?;
            if claimed.is_some() {
                return Ok(claimed);
            }
        }
        Ok(None)
    }

    async fn claim_run(
        &self,
        requested_run: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let claimed = claim_exact_transaction(
            &mut tx,
            requested_run,
            owner,
            lease_ms,
            now_ms,
            None,
            capabilities,
        )
        .await?;
        tx.commit().await.map_err(reject)?;
        Ok(claimed)
    }

    async fn claim_for_terminal_recovery(
        &self,
        requested_run: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let claimed = claim_exact_transaction_with_mode(
            &mut tx,
            requested_run,
            owner,
            lease_ms,
            now_ms,
            None,
            &Default::default(),
            ExactClaimMode::TerminalRecovery,
        )
        .await?;
        tx.commit().await.map_err(reject)?;
        Ok(claimed)
    }

    async fn claim_retry_exhausted(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        max_attempts: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = crate::clock::normalize_millis(now_ms);
        let max_attempts_i64 = durable_i64("dispatch retry limit", max_attempts)?;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        loop {
            let run_id = retry_exhausted_candidate(&mut tx, NS, now_ms, max_attempts_i64).await?;
            let Some(run_id) = run_id else {
                tx.commit().await.map_err(reject)?;
                return Ok(None);
            };
            if let Some(claimed) = claim_exact_transaction_with_mode(
                &mut tx,
                &RunId(run_id),
                owner,
                lease_ms,
                now_ms,
                None,
                &Default::default(),
                ExactClaimMode::RetryExhausted { max_attempts },
            )
            .await?
            {
                tx.commit().await.map_err(reject)?;
                return Ok(Some(claimed));
            }
            // A concurrent exact claim won the advisory candidate. Search again
            // before reporting None, which barriers the same-now ordinary claim.
        }
    }

    async fn claim_run_compatible(
        &self,
        requested_run: &RunId,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let claimed = claim_exact_transaction(
            &mut tx,
            requested_run,
            &worker.identity.lease_owner(),
            lease_ms,
            now_ms,
            Some(worker),
            &installed_worker_credential_capabilities(worker)?,
        )
        .await?;
        tx.commit().await.map_err(reject)?;
        Ok(claimed)
    }

    async fn renew_lease(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<bool, DispatchError> {
        let p = NS;
        let result = sqlx::query(&format!(
            "UPDATE {p}_dispatch SET lease_until = $1 \
             WHERE run_id = $2 AND status IN ('running', 'reservation_running') \
             AND lease_owner = $3"
        ))
        .bind(crate::clock::db_millis(crate::clock::deadline_millis(
            now_ms, lease_ms,
        )))
        .bind(&run_id.0)
        .bind(owner)
        .execute(&self.pool)
        .await
        .map_err(reject)?;
        Ok(result.rows_affected() > 0)
    }

    async fn bind_sandbox(
        &self,
        claim: &RunClaim,
        sandbox_ref: &str,
    ) -> Result<SettleOutcome, DispatchError> {
        let p = NS;
        let claim_epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
        let result = sqlx::query(&format!(
            "UPDATE {p}_dispatch SET sandbox = $1 WHERE run_id = $2 \
             AND status = 'running' AND lease_owner = $3 AND lease_epoch = $4"
        ))
        .bind(sandbox_ref)
        .bind(&claim.run_id.0)
        .bind(&claim.owner)
        .bind(claim_epoch)
        .execute(&self.pool)
        .await
        .map_err(reject)?;
        Ok(if result.rows_affected() == 1 {
            SettleOutcome::Applied
        } else {
            SettleOutcome::Fenced
        })
    }

    async fn record_credential_realization(
        &self,
        claim: &RunClaim,
        receipt: CredentialRealizationReceipt,
    ) -> Result<SettleOutcome, DispatchError> {
        let p = NS;
        let claim_epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let current = sqlx::query(&format!(
            "SELECT credential_bindings, credential_receipts FROM {p}_dispatch \
             WHERE run_id = $1 AND status = 'running' \
             AND lease_owner = $2 AND lease_epoch = $3 FOR UPDATE"
        ))
        .bind(&claim.run_id.0)
        .bind(&claim.owner)
        .bind(claim_epoch)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        let Some(current) = current else {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        };
        let bindings = current
            .try_get::<Option<Json<Vec<AttemptCredentialBinding>>>, _>("credential_bindings")
            .map_err(reject)?
            .map(|value| value.0)
            .unwrap_or_default();
        verify_credential_realization_receipt(&bindings, &receipt)
            .map_err(|error| DispatchError::Rejected(error.to_string()))?;
        let mut receipts = current
            .try_get::<Option<Json<Vec<CredentialRealizationReceipt>>>, _>("credential_receipts")
            .map_err(reject)?
            .map(|value| value.0)
            .unwrap_or_default();
        if let Some(existing) = receipts
            .iter()
            .find(|existing| existing.candidate_fingerprint == receipt.candidate_fingerprint)
        {
            if existing != &receipt {
                return Err(DispatchError::Rejected(
                    "credential realization receipt conflicts with committed evidence".to_string(),
                ));
            }
            tx.commit().await.map_err(reject)?;
            return Ok(SettleOutcome::Applied);
        }
        receipts.push(receipt);
        let changed = sqlx::query(&format!(
            "UPDATE {p}_dispatch SET credential_receipts = $1 \
             WHERE run_id = $2 AND status = 'running' \
             AND lease_owner = $3 AND lease_epoch = $4"
        ))
        .bind(Json(&receipts))
        .bind(&claim.run_id.0)
        .bind(&claim.owner)
        .bind(claim_epoch)
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
        if changed.rows_affected() != 1 {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        }
        tx.commit().await.map_err(reject)?;
        Ok(SettleOutcome::Applied)
    }

    async fn runnable_depth(&self, now_ms: u64) -> Result<Option<u64>, DispatchError> {
        let p = NS;
        let thread_available = thread_available_for_claim(p);
        let depth: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {p}_dispatch d WHERE \
             (d.status IN ('reserved', 'reservation_running') \
               AND d.lease_until IS NOT NULL AND d.lease_until < $1 AND {}) OR \
             (d.status = 'pending' AND {thread_available}) OR \
             (d.status = 'running' AND d.lease_until < $1) OR \
             (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS (\
               SELECT 1 FROM {p}_pending i WHERE i.run_id = d.run_id \
               AND (i.available_at IS NULL OR i.available_at <= $1)\
             )) AND {thread_available})",
            no_running_peer(p)
        ))
        .bind(crate::clock::db_millis(now_ms))
        .fetch_one(&self.pool)
        .await
        .map_err(reject)?;
        Ok(Some(durable_u64("runnable dispatch depth", depth)?))
    }

    async fn renew_owned_leases(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<usize, DispatchError> {
        let p = NS;
        // Only rows within half a lease of expiring — a fresh claim's lease is a
        // full length out, so it is skipped until it approaches expiry, bounding
        // the heartbeat's write amplification (ADR-0024).
        let result = sqlx::query(&format!(
            "UPDATE {p}_dispatch SET lease_until = $1 \
             WHERE status IN ('running', 'reservation_running') AND lease_owner = $2 \
             AND lease_until IS NOT NULL AND lease_until < $3"
        ))
        .bind(crate::clock::db_millis(crate::clock::deadline_millis(
            now_ms, lease_ms,
        )))
        .bind(owner)
        .bind(crate::clock::db_millis(crate::clock::deadline_millis(
            now_ms,
            lease_ms / 2,
        )))
        .execute(&self.pool)
        .await
        .map_err(reject)?;
        Ok(result.rows_affected() as usize)
    }

    async fn relinquish_claim(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        let p = NS;
        let epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let changed = sqlx::query(&format!(
            "UPDATE {p}_dispatch SET status = 'pending', lease_owner = NULL, \
             lease_until = NULL, created_at = clock_timestamp() \
             WHERE run_id = $1 AND status = 'running' \
             AND lease_owner = $2 AND lease_epoch = $3"
        ))
        .bind(&claim.run_id.0)
        .bind(&claim.owner)
        .bind(epoch)
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
        if changed.rows_affected() != 1 {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        }
        insert_operation(
            &mut tx,
            &DispatchOperation::LeaseLost {
                claim: claim.clone(),
                reason: LeaseLossReason::Relinquished,
            },
        )
        .await?;
        tx.commit().await.map_err(reject)?;
        Ok(SettleOutcome::Applied)
    }

    async fn settle(
        &self,
        run_id: &RunId,
        epoch: u64,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<SettleOutcome, DispatchError> {
        let p = NS;
        let epoch_i64 = durable_i64("dispatch lease epoch", epoch)?;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let authority = sqlx::query(&format!(
            "SELECT lease_owner, request FROM {p}_dispatch WHERE run_id = $1 \
             AND status = 'running' AND lease_epoch = $2 AND lease_owner IS NOT NULL \
             FOR UPDATE"
        ))
        .bind(&run_id.0)
        .bind(epoch_i64)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        let Some(authority) = authority else {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        };
        let owner: String = authority.try_get("lease_owner").map_err(reject)?;
        let Json(request): Json<RunDispatch> = authority.try_get("request").map_err(reject)?;
        let request_fingerprint = request.canonical_fingerprint();
        let claim = RunClaim {
            run_id: run_id.clone(),
            owner,
            epoch,
        };
        // Fence first: mutate the dispatch row ONLY while the caller still holds the
        // current epoch. A stale owner (lower epoch) affects zero rows, so its settle
        // touches neither the dispatch nor its pending — the reclaimer's in-flight
        // state is inviolate.
        let dispatch_rows = match outcome {
            DispatchOutcome::Done => sqlx::query(&format!(
                "DELETE FROM {p}_dispatch WHERE run_id = $1 \
                 AND status = 'running' AND lease_epoch = $2"
            ))
            .bind(&run_id.0)
            .bind(epoch_i64)
            .execute(&mut *tx)
            .await
            .map_err(reject)?
            .rows_affected(),
            DispatchOutcome::Awaiting => sqlx::query(&format!(
                "UPDATE {p}_dispatch SET status = 'awaiting', lease_owner = NULL, \
                 lease_until = NULL, attempt_count = 0 WHERE run_id = $1 \
                 AND status = 'running' AND lease_epoch = $2"
            ))
            .bind(&run_id.0)
            .bind(epoch_i64)
            .execute(&mut *tx)
            .await
            .map_err(reject)?
            .rows_affected(),
        };
        if dispatch_rows == 0 {
            // Fenced: the run was re-claimed under a higher epoch (or already gone).
            // Change nothing and report the loss so the stale caller abandons.
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        }
        if outcome == DispatchOutcome::Done {
            sqlx::query(&format!(
                "INSERT INTO {p}_dispatch_completion \
                 (run_id, request_fingerprint, thread_id, session_thread_id) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (run_id) DO NOTHING"
            ))
            .bind(&run_id.0)
            .bind(&request_fingerprint)
            .bind(&request.thread_id().0)
            .bind(
                request
                    .session_thread_id
                    .as_ref()
                    .map(|thread| thread.0.as_str()),
            )
            .execute(&mut *tx)
            .await
            .map_err(reject)?;
        }
        // The fence held; now reconcile the run's pending input.
        match outcome {
            DispatchOutcome::Done => {
                // Drop the run's own pending and anything else consumed this
                // attempt (e.g. unbound idle-thread input, ADR-0021).
                sqlx::query(&format!(
                    "DELETE FROM {p}_pending WHERE run_id = $1 OR message_id = ANY($2)"
                ))
                .bind(&run_id.0)
                .bind(consumed)
                .execute(&mut *tx)
                .await
                .map_err(reject)?;
            }
            DispatchOutcome::Awaiting => {
                sqlx::query(&format!(
                    "DELETE FROM {p}_pending WHERE message_id = ANY($1)"
                ))
                .bind(consumed)
                .execute(&mut *tx)
                .await
                .map_err(reject)?;
            }
        }
        insert_operation(&mut tx, &DispatchOperation::Settled { claim, outcome }).await?;
        tx.commit().await.map_err(reject)?;
        Ok(SettleOutcome::Applied)
    }

    async fn completion_events_after(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<DispatchCompletion>, DispatchError> {
        load_completion_events(&self.pool, NS, after_sequence, limit).await
    }

    async fn quarantine_retry_exhausted(
        &self,
        max_attempts: u64,
        now_ms: u64,
    ) -> Result<usize, DispatchError> {
        let p = NS;
        let max_attempts_i64 = durable_i64("dispatch retry limit", max_attempts)?;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let rows = sqlx::query(&format!(
            "WITH candidates AS ( \
                 SELECT run_id, lease_owner, lease_epoch, attempt_count \
                 FROM {p}_dispatch WHERE status = 'running' \
                 AND lease_until IS NOT NULL AND lease_until < $1 \
                 AND attempt_count >= $2 FOR UPDATE \
             ) \
             UPDATE {p}_dispatch AS dispatch SET status = 'dead_letter', \
             lease_owner = NULL, lease_until = NULL, dead_lettered_at = $1 \
             FROM candidates WHERE dispatch.run_id = candidates.run_id \
             RETURNING dispatch.run_id, candidates.lease_owner, \
                       candidates.lease_epoch, candidates.attempt_count"
        ))
        .bind(crate::clock::db_millis(now_ms))
        .bind(max_attempts_i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(reject)?;
        for row in &rows {
            let owner = row
                .try_get::<Option<String>, _>("lease_owner")
                .map_err(reject)?
                .ok_or_else(|| {
                    DispatchError::Rejected(
                        "expired running dispatch has no persisted lease owner".to_string(),
                    )
                })?;
            let epoch = row.try_get::<i64, _>("lease_epoch").map_err(reject)?;
            let attempt_count = row.try_get::<i64, _>("attempt_count").map_err(reject)?;
            let claim = RunClaim {
                run_id: RunId(row.try_get("run_id").map_err(reject)?),
                owner,
                epoch: durable_u64("dispatch lease epoch", epoch)?,
            };
            insert_operation(
                &mut tx,
                &DispatchOperation::LeaseLost {
                    claim: claim.clone(),
                    reason: LeaseLossReason::RetryExhausted,
                },
            )
            .await?;
            insert_operation(
                &mut tx,
                &DispatchOperation::DeadLettered {
                    claim,
                    attempt_count: durable_u64("dispatch attempt count", attempt_count)?,
                },
            )
            .await?;
        }
        tx.commit().await.map_err(reject)?;
        Ok(rows.len())
    }

    async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError> {
        self.run_ids_by_status("dead_letter").await
    }

    async fn superseded(&self) -> Result<Vec<RunId>, DispatchError> {
        self.run_ids_by_status("superseded").await
    }

    async fn list_dispatches(&self) -> Result<Vec<DispatchSummary>, DispatchError> {
        let p = NS;
        let rows = sqlx::query(&format!(
            "SELECT run_id, thread_id, request, status, attempt_count, cancel_requested, sandbox, lease_until FROM {p}_dispatch \
             ORDER BY created_at"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        rows.into_iter()
            .map(|row| {
                let Json(request): Json<RunDispatch> = row.try_get("request").map_err(reject)?;
                Ok(DispatchSummary {
                    run_id: RunId(row.try_get("run_id").map_err(reject)?),
                    thread_id: ThreadId(row.try_get("thread_id").map_err(reject)?),
                    session_thread_id: request.session_thread_id,
                    session_activity_epoch: request.session_activity_epoch,
                    reservation_deadline_ms: if row
                        .try_get::<String, _>("status")
                        .map_err(reject)?
                        == "reserved"
                    {
                        row.try_get::<Option<i64>, _>("lease_until")
                            .map_err(reject)?
                            .map(crate::clock::millis_from_db)
                            .transpose()
                            .map_err(|error| DispatchError::Rejected(error.to_string()))?
                    } else {
                        None
                    },
                    state: {
                        let status = row.try_get::<String, _>("status").map_err(reject)?;
                        DispatchState::from_db(&status).ok_or_else(|| {
                            DispatchError::Rejected(format!(
                                "unknown persisted dispatch state {status}"
                            ))
                        })?
                    },
                    cancellation_requested: row
                        .try_get::<i64, _>("cancel_requested")
                        .map_err(reject)?
                        != 0,
                    attempt_count: durable_u64(
                        "dispatch attempt count",
                        row.try_get::<i64, _>("attempt_count").map_err(reject)?,
                    )?,
                    sandbox_bound: row
                        .try_get::<Option<String>, _>("sandbox")
                        .map_err(reject)?
                        .is_some(),
                })
            })
            .collect()
    }

    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        let p = NS;
        let result = sqlx::query(&format!(
            "UPDATE {p}_dispatch SET status = 'pending', attempt_count = 0, lease_owner = NULL, \
             lease_until = NULL WHERE run_id = $1 AND status = 'dead_letter' \
             AND cancel_requested = 0"
        ))
        .bind(&run_id.0)
        .execute(&self.pool)
        .await
        .map_err(reject)?;
        Ok(result.rows_affected() > 0)
    }

    async fn cancel(&self, run_id: &RunId) -> Result<Option<ThreadId>, DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let current = sqlx::query(&format!(
            "SELECT thread_id, status, lease_owner, lease_epoch FROM {p}_dispatch \
             WHERE run_id = $1 AND status IN \
             ('reserved', 'reservation_running', 'pending', 'awaiting', 'running', 'dead_letter') \
             FOR UPDATE"
        ))
        .bind(&run_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        let Some(current) = current else {
            tx.commit().await.map_err(reject)?;
            return Ok(None);
        };
        let thread = current.try_get::<String, _>("thread_id").map_err(reject)?;
        let status = current.try_get::<String, _>("status").map_err(reject)?;
        let previous_owner = current
            .try_get::<Option<String>, _>("lease_owner")
            .map_err(reject)?;
        let previous_epoch = current.try_get::<i64, _>("lease_epoch").map_err(reject)?;
        let claimed = matches!(status.as_str(), "running" | "reservation_running");
        let next_epoch = if claimed {
            Some(
                i64::try_from(crate::next_claim_epoch(previous_epoch)?).map_err(|_| {
                    DispatchError::Rejected(
                        "dispatch claim epoch exceeds the Postgres authority range".to_string(),
                    )
                })?,
            )
        } else {
            None
        };
        sqlx::query(&format!(
            "UPDATE {p}_dispatch SET cancel_requested = 1, \
             lease_epoch = CASE WHEN status IN ('running', 'reservation_running') \
               THEN $2 ELSE lease_epoch END, \
             lease_owner = CASE WHEN status IN ('running', 'reservation_running') \
               THEN NULL ELSE lease_owner END, \
             lease_until = CASE WHEN status = 'reservation_running' \
               THEN 0 WHEN status = 'running' THEN NULL ELSE lease_until END, \
             status = CASE WHEN status = 'running' THEN 'pending' \
               WHEN status = 'reservation_running' THEN 'reserved' ELSE status END \
             WHERE run_id = $1"
        ))
        .bind(&run_id.0)
        .bind(next_epoch)
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
        if claimed {
            insert_operation(
                &mut tx,
                &DispatchOperation::LeaseLost {
                    claim: RunClaim {
                        run_id: run_id.clone(),
                        owner: previous_owner.ok_or_else(|| {
                            DispatchError::Rejected(
                                "claimed dispatch has no persisted lease owner".to_string(),
                            )
                        })?,
                        epoch: u64::try_from(previous_epoch).map_err(|_| {
                            DispatchError::Rejected(
                                "persisted dispatch claim epoch is negative".to_string(),
                            )
                        })?,
                    },
                    reason: LeaseLossReason::Cancelled,
                },
            )
            .await?;
        }
        tx.commit().await.map_err(reject)?;
        Ok(Some(ThreadId(thread)))
    }

    async fn awaiting_run(&self, thread_id: &ThreadId) -> Result<Option<RunId>, DispatchError> {
        let p = NS;
        let run: Option<String> = sqlx::query_scalar(&format!(
            "SELECT run_id FROM {p}_dispatch WHERE thread_id = $1 AND status = 'awaiting' \
             AND cancel_requested = 0 \
             ORDER BY created_at LIMIT 1"
        ))
        .bind(&thread_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(reject)?;
        Ok(run.map(RunId))
    }

    async fn purge_dead_letters(&self) -> Result<usize, DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        sqlx::query(&format!(
            "DELETE FROM {p}_pending WHERE run_id IN \
             (SELECT run_id FROM {p}_dispatch WHERE status = 'dead_letter')"
        ))
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
        let result = sqlx::query(&format!(
            "DELETE FROM {p}_dispatch WHERE status = 'dead_letter'"
        ))
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
        tx.commit().await.map_err(reject)?;
        Ok(result.rows_affected() as usize)
    }

    async fn purge_dead_letters_before(&self, cutoff_ms: u64) -> Result<usize, DispatchError> {
        let p = NS;
        let cond = "status = 'dead_letter' AND dead_lettered_at IS NOT NULL \
                    AND dead_lettered_at <= $1";
        let mut tx = self.pool.begin().await.map_err(reject)?;
        sqlx::query(&format!(
            "DELETE FROM {p}_pending WHERE run_id IN \
             (SELECT run_id FROM {p}_dispatch WHERE {cond})"
        ))
        .bind(crate::clock::db_millis(cutoff_ms))
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
        let result = sqlx::query(&format!("DELETE FROM {p}_dispatch WHERE {cond}"))
            .bind(crate::clock::db_millis(cutoff_ms))
            .execute(&mut *tx)
            .await
            .map_err(reject)?;
        tx.commit().await.map_err(reject)?;
        Ok(result.rows_affected() as usize)
    }
}
