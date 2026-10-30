// @formal entrypoint credential_effect.boundary_preserves_exact_materialization_authority
// @formal entrypoint ingress.pending_input_exactly_once
// @formal entrypoint ingress.settle_fence
// @formal entrypoint remote_worker.pending_signal_is_consumed_by_commit
// @formal entrypoint remote_worker.single_live_claim_owner
// @formal entrypoint run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
// @formal entrypoint run_ingress.failed_attempt_release_spends_one_attempt_and_exhausts_through_retry_exhaustion
// @formal entrypoint run_ingress.relinquish_requires_exact_current_claim_and_preserves_retry_budget
// @formal entrypoint run_ingress.renewal_projects_cancel_only_to_exact_physical_attempt
// @formal entrypoint run_ingress.request_local_resolution_retry_honors_persisted_deadline
// @formal entrypoint session.agent_message_completed_target_allows_late_source_receipt_without_reexecution
// @formal entrypoint session.agent_message_exact_replay_reuses_one_dispatch_and_payload_conflict_stutters
// @formal entrypoint session_execution.one_physical_attempt_per_thread
// @formal entrypoint session_run.recovery_claim_is_admission_only
// @formal entrypoint session_run.reservation_ttl_uses_store_clock
// @formal entrypoint worker.durable_dispatch_contains_no_plaintext_secret
// @formal evidence ingress.epoch_reclaim
// @formal evidence ingress.single_owner
// awaken-allow(file-limit-ceiling): 1706
// PostgreSQL realization of the one neutral DispatchQueue contract. Included by
// `postgres.rs` so public type and implementation paths remain unchanged.
impl PostgresDispatchStore {
    async fn admit_physical_attempt(
        &self,
        claim: &RunClaim,
        handoff: bool,
        binding: Option<&AttemptExecutionBinding>,
    ) -> Result<AttemptAdmission, DispatchError> {
        let tx = self.pool.begin().await.map_err(store_error)?;
        self.admit_physical_attempt_transaction(claim, handoff, binding, tx, None)
            .await
    }

    async fn admit_physical_attempt_transaction(
        &self,
        claim: &RunClaim,
        handoff: bool,
        binding: Option<&AttemptExecutionBinding>,
        mut tx: Transaction<'static, Postgres>,
        worker: Option<WorkerSnapshot>,
    ) -> Result<AttemptAdmission, DispatchError> {
        let p = NS;
        let epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
        // Match claim's Thread-before-row order. The unique physical index is
        // the storage backstop; recheck peers under this transaction lock to
        // return the domain Blocked outcome instead of an expected SQL error.
        let Some(thread) = lock_dispatch_thread(&mut tx, &claim.run_id, ThreadLock::Wait).await?
        else {
            return Ok(AttemptAdmission::Fenced);
        };
        let now_ms = crate::postgres_helpers::postgres_now_ms(&mut *tx).await?;
        if worker.as_ref().is_some_and(|snapshot| {
            snapshot.expires_at_ms <= now_ms
                || matches!(
                    snapshot.state,
                    crate::WorkerState::Quiesced | crate::WorkerState::Dead
                )
        }) {
            tx.rollback().await.map_err(store_error)?;
            return Ok(AttemptAdmission::Fenced);
        }

        let current = sqlx::query(&format!(
            "SELECT status, lease_owner, lease_epoch, lease_until, \
             active_attempt_owner, active_attempt_epoch, retired_attempt_epoch, active_attempt_binding, worker_assignment, active_attempt_closing, request \
             FROM {p}_dispatch WHERE run_id = $1 FOR UPDATE"
        ))
        .bind(&claim.run_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        let Some(current) = current else {
            let _ = tx.rollback().await;
            return Ok(AttemptAdmission::Fenced);
        };
        let status: String = current.try_get("status").map_err(store_error)?;
        let owner: Option<String> = current.try_get("lease_owner").map_err(store_error)?;
        let persisted_epoch: i64 = current.try_get("lease_epoch").map_err(store_error)?;
        let lease_until: Option<i64> = current.try_get("lease_until").map_err(store_error)?;
        let active_owner: Option<String> = current
            .try_get("active_attempt_owner")
            .map_err(store_error)?;
        let active_epoch: Option<i64> = current
            .try_get("active_attempt_epoch")
            .map_err(store_error)?;
        let persisted_epoch = durable_u64("dispatch lease epoch", persisted_epoch)?;
        let active_epoch = active_epoch
            .map(|value| durable_u64("dispatch attempt epoch", value))
            .transpose()?;
        let retired_epoch = durable_u64(
            "retired attempt epoch",
            current
                .try_get("retired_attempt_epoch")
                .map_err(store_error)?,
        )?;
        let live = awaken_dispatch_contract::attempt_epoch_is_open(claim.epoch, retired_epoch)
            && status == "running"
            && owner.as_deref() == Some(&claim.owner)
            && persisted_epoch == claim.epoch
            && lease_until.is_some_and(|until| until >= crate::clock::db_millis(now_ms));
        let slot = PhysicalAttemptSlot::from_facts(
            active_owner.is_some(),
            active_epoch.is_some(),
            active_owner.as_deref() == Some(&claim.owner),
            active_epoch == Some(claim.epoch),
        );
        let peer_occupied: bool = sqlx::query_scalar(&format!(
            "SELECT EXISTS (SELECT 1 FROM {p}_dispatch WHERE thread_id = $1
             AND run_id <> $2 AND (active_attempt_owner IS NOT NULL OR active_attempt_epoch IS NOT NULL))"
        )).bind(&thread).bind(&claim.run_id.0).fetch_one(&mut *tx).await.map_err(store_error)?;
        let (transition, admission) = crate::dispatch::decide_physical_attempt_admission(
            live,
            slot,
            handoff,
            peer_occupied,
            current
                .try_get("active_attempt_closing")
                .map_err(store_error)?,
        )?;
        let stored = crate::dispatch::decode_physical_attempt(
            &claim.run_id,
            active_owner,
            active_epoch,
            current
                .try_get("active_attempt_binding")
                .map_err(store_error)?,
            current
                .try_get("active_attempt_closing")
                .map_err(store_error)?,
        )?;
        let assignment: Option<Json<WorkerAssignment>> =
            current.try_get("worker_assignment").map_err(store_error)?;
        crate::dispatch::verify_physical_attempt_binding(
            transition,
            claim,
            assignment.as_ref().map(|a| &a.0),
            binding,
            stored.as_ref().and_then(|a| a.binding.as_ref()),
        )?;
        if matches!(
            transition,
            PhysicalAttemptTransition::Install | PhysicalAttemptTransition::Replace
        ) {
            let Json(request): Json<RunDispatch> =
                current.try_get("request").map_err(store_error)?;
            let operation = crate::dispatch::physical_admission_operation(claim, binding, &request);
            insert_operation(&mut tx, operation).await?;
            sqlx::query(&format!(
                "UPDATE {p}_dispatch SET active_attempt_owner = $2, \
                     active_attempt_epoch = $3, active_attempt_binding = $4, active_attempt_closing = FALSE WHERE run_id = $1"
            ))
            .bind(&claim.run_id.0)
            .bind(&claim.owner)
            .bind(epoch)
            .bind(
                binding
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(DispatchError::unavailable)?,
            )
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        }
        tx.commit().await.map_err(store_error)?;
        Ok(admission)
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchCommitAuthority for PostgresDispatchStore {
    async fn worker_owns_run(
        &self,
        identity: &crate::WorkerIdentity,
        run_id: &RunId,
    ) -> Result<Option<RunClaim>, DispatchError> {
        let now_ms = crate::postgres_helpers::postgres_now_ms(&self.pool).await?;
        current_worker_claim(&self.pool, NS, identity, run_id, now_ms).await
    }

    async fn lock_commit_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        self.lock_claim_epoch_in_status(claim, "running").await
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchClaims for PostgresDispatchStore {
    async fn resolve_claimed_session_run_reservation(
        &self,
        claim: &RunClaim,
        resolution: SessionRunReservationResolution,
    ) -> Result<SettleOutcome, DispatchError> {
        let resolution = validate_session_run_reservation_resolution(resolution)?;
        let p = NS;
        let claim_epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let row = sqlx::query(&format!(
            "SELECT request, cancel_requested FROM {p}_dispatch WHERE run_id = $1 \
             AND status = 'reservation_running' AND lease_owner = $2 \
             AND lease_epoch = $3 FOR UPDATE"
        ))
        .bind(&claim.run_id.0)
        .bind(&claim.owner)
        .bind(claim_epoch)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        let Some(row) = row else {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        };
        let Json(mut request): Json<RunDispatch> = row.try_get("request").map_err(store_error)?;
        let cancellation_requested: i64 = row.try_get("cancel_requested").map_err(store_error)?;
        let current = crate::persisted_dispatch_transition(
            "reservation_running",
            claim_epoch,
            cancellation_requested != 0,
        )?;
        let resolution_value = match resolution {
            SessionRunReservationResolution::Admitted { .. } => Some(true),
            SessionRunReservationResolution::Retry { .. } => Some(false),
            SessionRunReservationResolution::Rejected => None,
        };
        let transition = current.resolve_reservation(claim.epoch, true, resolution_value);
        let changed = match resolution {
            SessionRunReservationResolution::Admitted {
                session_activity_epoch,
            } => {
                request.session_activity_epoch = Some(session_activity_epoch);
                let awaken_dispatch_contract::GuardedTransition::Applied(next) = transition else {
                    unreachable!("the exact recovery claim is admitted by the kernel")
                };
                sqlx::query(&format!(
                    "UPDATE {p}_dispatch SET request = $1, status = $5, \
                     lease_owner = NULL, lease_until = NULL, worker_assignment = NULL, \
                     credential_bindings = NULL, credential_receipts = NULL \
                     WHERE run_id = $2 AND status = 'reservation_running' \
                     AND lease_owner = $3 AND lease_epoch = $4"
                ))
                .bind(Json(&request))
                .bind(&claim.run_id.0)
                .bind(&claim.owner)
                .bind(claim_epoch)
                .bind(crate::dispatch_state_db(next.state))
                .execute(&mut *tx)
                .await
                .map_err(store_error)?
                .rows_affected()
            }
            SessionRunReservationResolution::Retry { reservation_ttl_ms } => {
                let awaken_dispatch_contract::GuardedTransition::Applied(next) = transition else {
                    unreachable!("the exact recovery claim is admitted by the kernel")
                };
                let store_now_ms = crate::postgres_helpers::postgres_now_ms(&mut *tx).await?;
                let reservation_deadline_ms =
                    crate::clock::deadline_millis(store_now_ms, reservation_ttl_ms);
                sqlx::query(&format!(
                    "UPDATE {p}_dispatch SET status = $5, lease_owner = NULL, \
                     lease_until = $1, worker_assignment = NULL, credential_bindings = NULL, \
                     credential_receipts = NULL, created_at = CURRENT_TIMESTAMP \
                     WHERE run_id = $2 AND status = 'reservation_running' \
                     AND lease_owner = $3 AND lease_epoch = $4"
                ))
                .bind(crate::clock::db_millis(reservation_deadline_ms))
                .bind(&claim.run_id.0)
                .bind(&claim.owner)
                .bind(claim_epoch)
                .bind(crate::dispatch_state_db(next.state))
                .execute(&mut *tx)
                .await
                .map_err(store_error)?
                .rows_affected()
            }
            SessionRunReservationResolution::Rejected => {
                assert!(matches!(
                    transition,
                    awaken_dispatch_contract::GuardedTransition::Removed
                ));
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
                .map_err(store_error)?
                .rows_affected();
                if changed == 1 {
                    sqlx::query(&format!("DELETE FROM {p}_pending WHERE run_id = $1"))
                        .bind(&claim.run_id.0)
                        .execute(&mut *tx)
                        .await
                        .map_err(store_error)?;
                }
                changed
            }
        };
        if changed != 1 {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        }
        tx.commit().await.map_err(store_error)?;
        Ok(SettleOutcome::Applied)
    }

    async fn claim_new_run(
        &self,
        request: RunDispatch,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        lock_run_identity(&mut tx, &request.run_id().0).await?;
        if exact_run_replay(&mut tx, p, &request).await? {
            let run_id = request.run_id().clone();
            let claimed = claim_exact_transaction(
                &mut tx,
                ThreadLock::Wait,
                &run_id,
                owner,
                lease_ms,
                None,
                capabilities,
            )
            .await?;
            tx.commit().await.map_err(store_error)?;
            return Ok(claimed);
        }
        validate_executable_dispatch_admission(&request)?;
        if !can_claim_locally(&request.placement) {
            tx.commit().await.map_err(store_error)?;
            return Ok(None);
        }
        insert_new_dispatch(&mut tx, p, &request, &SubmitOptions::default()).await?;
        let run_id = request.run_id().clone();
        let claimed = claim_exact_transaction(
            &mut tx,
            ThreadLock::Wait,
            &run_id,
            owner,
            lease_ms,
            None,
            capabilities,
        )
        .await?;
        tx.commit().await.map_err(store_error)?;
        Ok(claimed)
    }

    async fn claim_new_run_compatible(
        &self,
        request: RunDispatch,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let p = NS;
        let owner = worker.identity.lease_owner();
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let now_ms = crate::postgres_helpers::postgres_now_ms(&mut *tx).await?;
        lock_run_identity(&mut tx, &request.run_id().0).await?;
        if exact_run_replay(&mut tx, p, &request).await? {
            let run_id = request.run_id().clone();
            let claimed = claim_exact_transaction(
                &mut tx,
                ThreadLock::Wait,
                &run_id,
                &owner,
                lease_ms,
                Some(worker),
                &installed_worker_credential_capabilities(worker)?,
            )
            .await?;
            tx.commit().await.map_err(store_error)?;
            return Ok(claimed);
        }
        validate_executable_dispatch_admission(&request)?;
        if can_assign(worker, &request.placement, None, false, now_ms).is_err() {
            tx.commit().await.map_err(store_error)?;
            return Ok(None);
        }
        insert_new_dispatch(&mut tx, p, &request, &SubmitOptions::default()).await?;
        let run_id = request.run_id().clone();
        let claimed = claim_exact_transaction(
            &mut tx,
            ThreadLock::Wait,
            &run_id,
            &owner,
            lease_ms,
            Some(worker),
            &installed_worker_credential_capabilities(worker)?,
        )
        .await?;
        tx.commit().await.map_err(store_error)?;
        Ok(claimed)
    }

    async fn deliver_and_claim(
        &self,
        input: PendingInput,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let input = normalize_pending_millis(input);
        let run_id = input.run_id.clone();
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        append_pending_transaction(&mut tx, NS, &input).await?;
        let claimed = claim_exact_transaction(
            &mut tx,
            ThreadLock::Wait,
            &run_id,
            owner,
            lease_ms,
            None,
            capabilities,
        )
        .await?;
        tx.commit().await.map_err(store_error)?;
        Ok(claimed)
    }

    async fn deliver_and_claim_compatible(
        &self,
        input: PendingInput,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let input = normalize_pending_millis(input);
        let run_id = input.run_id.clone();
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        append_pending_transaction(&mut tx, NS, &input).await?;
        let claimed = claim_exact_transaction(
            &mut tx,
            ThreadLock::Wait,
            &run_id,
            &worker.identity.lease_owner(),
            lease_ms,
            Some(worker),
            &installed_worker_credential_capabilities(worker)?,
        )
        .await?;
        tx.commit().await.map_err(store_error)?;
        Ok(claimed)
    }

    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        self.claim_discovered(owner, lease_ms, capabilities, CandidateAudience::Local)
            .await
    }

    async fn claim_compatible(
        &self,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let capabilities = installed_worker_credential_capabilities(worker)?;
        self.claim_discovered(
            &worker.identity.lease_owner(),
            lease_ms,
            &capabilities,
            CandidateAudience::Compatible(worker),
        )
        .await
    }

    async fn claim_run(
        &self,
        requested_run: &RunId,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let claimed = claim_exact_transaction(
            &mut tx,
            ThreadLock::Wait,
            requested_run,
            owner,
            lease_ms,
            None,
            capabilities,
        )
        .await?;
        tx.commit().await.map_err(store_error)?;
        Ok(claimed)
    }

    async fn claim_retry_exhausted(
        &self,
        owner: &str,
        lease_ms: u64,
        max_attempts: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let max_attempts_i64 = durable_i64("dispatch retry limit", max_attempts)?;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        loop {
            // Discovery and the exact claim must use the same clock authority;
            // a future caller timestamp otherwise repeatedly selects live rows.
            let now_ms = crate::postgres_helpers::postgres_now_ms(&mut *tx).await?;
            let run_id = retry_exhausted_candidate(&mut tx, NS, now_ms, max_attempts_i64).await?;
            let Some(run_id) = run_id else {
                tx.commit().await.map_err(store_error)?;
                return Ok(None);
            };
            if let Some(claimed) = claim_exact_transaction_with_mode(
                &mut tx,
                ThreadLock::Wait,
                &RunId(run_id),
                owner,
                lease_ms,
                None,
                &Default::default(),
                ExactClaimMode::RetryExhausted { max_attempts },
            )
            .await?
            {
                tx.commit().await.map_err(store_error)?;
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
    ) -> Result<Option<Claimed>, DispatchError> {
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let claimed = claim_exact_transaction(
            &mut tx,
            ThreadLock::Wait,
            requested_run,
            &worker.identity.lease_owner(),
            lease_ms,
            Some(worker),
            &installed_worker_credential_capabilities(worker)?,
        )
        .await?;
        tx.commit().await.map_err(store_error)?;
        Ok(claimed)
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchEnqueue for PostgresDispatchStore {
    async fn enqueue_with(
        &self,
        request: RunDispatch,
        options: SubmitOptions,
    ) -> Result<(), DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        lock_run_identity(&mut tx, &request.run_id().0).await?;

        // Run-id idempotency survives successful completion: a live row or the
        // permanent completion tombstone makes the whole command a no-op. Check
        // before supersession so replay cannot mutate sibling dispatches.
        if exact_run_replay(&mut tx, p, &request).await? {
            tx.commit().await.map_err(store_error)?;
            return Ok(());
        }

        validate_executable_dispatch_admission(&request)?;
        insert_new_dispatch(&mut tx, p, &request, &options).await?;
        tx.commit().await.map_err(store_error)?;
        Ok(())
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchClaimOwnership for PostgresDispatchStore {
    async fn claim_is_current(&self, claim: &RunClaim) -> Result<bool, DispatchError> {
        let epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
        sqlx::query_scalar(&format!(
            "SELECT EXISTS(SELECT 1 FROM {NS}_dispatch WHERE run_id = $1 \
             AND status = 'running' AND lease_owner = $2 AND lease_epoch = $3 \
             AND lease_until IS NOT NULL AND lease_until > \
             FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::bigint)"
        ))
        .bind(&claim.run_id.0)
        .bind(&claim.owner)
        .bind(epoch)
        .fetch_one(&self.pool)
        .await
        .map_err(store_error)
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchExecution for PostgresDispatchStore {
    async fn renew_lease(
        &self,
        claim: &RunClaim,
        lease_ms: u64,
    ) -> Result<LeaseRenewalOutcome, DispatchError> {
        let claim_epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let now_ms = crate::postgres_helpers::postgres_now_ms(&mut *tx).await?;
        let current = sqlx::query(&format!(
            "SELECT status, lease_owner, lease_epoch, cancel_requested, \
             active_attempt_owner, active_attempt_epoch FROM {p}_dispatch \
             WHERE run_id = $1 FOR UPDATE"
        ))
        .bind(&claim.run_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        let Some(current) = current else {
            tx.commit().await.map_err(store_error)?;
            return Ok(LeaseRenewalOutcome::Lost);
        };
        let status: String = current.try_get("status").map_err(store_error)?;
        let owner: Option<String> = current.try_get("lease_owner").map_err(store_error)?;
        let persisted_epoch: i64 = current.try_get("lease_epoch").map_err(store_error)?;
        let cancellation_requested: i64 =
            current.try_get("cancel_requested").map_err(store_error)?;
        let attempt_owner: Option<String> = current
            .try_get("active_attempt_owner")
            .map_err(store_error)?;
        let attempt_epoch: Option<i64> = current
            .try_get("active_attempt_epoch")
            .map_err(store_error)?;
        let current_claim = matches!(status.as_str(), "running" | "reservation_running")
            && owner.as_deref() == Some(&claim.owner)
            && persisted_epoch == claim_epoch;
        let cancelled_physical_attempt = cancellation_requested != 0
            && attempt_owner.as_deref() == Some(&claim.owner)
            && attempt_epoch == Some(claim_epoch);
        let outcome = decide_lease_renewal(current_claim, cancelled_physical_attempt);
        if outcome == LeaseRenewalOutcome::Current {
            sqlx::query(&format!(
                "UPDATE {p}_dispatch SET lease_until = $1 WHERE run_id = $2 \
                 AND status IN ('running', 'reservation_running') \
                 AND lease_owner = $3 AND lease_epoch = $4"
            ))
            .bind(crate::clock::db_millis(crate::clock::deadline_millis(
                now_ms, lease_ms,
            )))
            .bind(&claim.run_id.0)
            .bind(&claim.owner)
            .bind(claim_epoch)
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        }
        tx.commit().await.map_err(store_error)?;
        Ok(outcome)
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
        .map_err(store_error)?;
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
        let mut tx = self.pool.begin().await.map_err(store_error)?;
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
        .map_err(store_error)?;
        let Some(current) = current else {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        };
        let bindings = current
            .try_get::<Option<Json<Vec<AttemptCredentialBinding>>>, _>("credential_bindings")
            .map_err(store_error)?
            .map(|value| value.0)
            .unwrap_or_default();
        verify_credential_realization_receipt(&bindings, &receipt)
            .map_err(|error| DispatchError::Rejected(error.to_string()))?;
        let mut receipts = current
            .try_get::<Option<Json<Vec<CredentialRealizationReceipt>>>, _>("credential_receipts")
            .map_err(store_error)?
            .map(|value| value.0)
            .unwrap_or_default();
        let existing = receipts
            .iter()
            .find(|existing| existing.candidate_fingerprint == receipt.candidate_fingerprint);
        match credential_realization_persistence_decision(existing, &receipt) {
            CredentialRealizationPersistenceDecision::Collision => {
                return Err(DispatchError::Rejected(
                    "credential realization receipt conflicts with committed evidence".to_string(),
                ));
            }
            CredentialRealizationPersistenceDecision::ExactReplay => {
                tx.commit().await.map_err(store_error)?;
                return Ok(SettleOutcome::Applied);
            }
            CredentialRealizationPersistenceDecision::Insert => {}
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
        .map_err(store_error)?;
        if changed.rows_affected() != 1 {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        }
        tx.commit().await.map_err(store_error)?;
        Ok(SettleOutcome::Applied)
    }

    async fn begin_attempt(
        &self,
        claim: &RunClaim,
        _binding: Option<&awaken_dispatch_contract::dispatch::AttemptExecutionBinding>,
    ) -> Result<AttemptAdmission, DispatchError> {
        self.admit_physical_attempt(claim, false, _binding).await
    }

    async fn handoff_attempt(
        &self,
        claim: &RunClaim,
        _binding: Option<&awaken_dispatch_contract::dispatch::AttemptExecutionBinding>,
    ) -> Result<AttemptAdmission, DispatchError> {
        self.admit_physical_attempt(claim, true, _binding).await
    }

    async fn close_attempt(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        self.close_physical_attempt(claim).await
    }
    async fn finish_attempt(
        &self,
        claim: &RunClaim,
        cause: Option<awaken_dispatch_contract::AttemptReturnCause>,
    ) -> Result<SettleOutcome, DispatchError> {
        self.finish_physical_attempt_with(claim, crate::dispatch::AttemptFinish::Ordinary(cause))
            .await
    }

    async fn relinquish_claim(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        release_claim_transaction(
            &self.pool,
            claim,
            None,
            awaken_dispatch_contract::ReleasedAttempt::Unspent,
        )
        .await
    }

    async fn defer_claim(
        &self,
        claim: &RunClaim,
        retry_after_ms: u64,
    ) -> Result<SettleOutcome, DispatchError> {
        release_claim_transaction(
            &self.pool,
            claim,
            Some(retry_after_ms),
            awaken_dispatch_contract::ReleasedAttempt::Unspent,
        )
        .await
    }

    async fn retry_claim(
        &self,
        claim: &RunClaim,
        retry_after_ms: u64,
        max_attempts: u64,
    ) -> Result<SettleOutcome, DispatchError> {
        release_claim_transaction(
            &self.pool,
            claim,
            Some(retry_after_ms),
            awaken_dispatch_contract::ReleasedAttempt::Failed { max_attempts },
        )
        .await
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchSettlement for PostgresDispatchStore {
    async fn settle(
        &self,
        run_id: &RunId,
        epoch: u64,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<SettleOutcome, DispatchError> {
        let p = NS;
        let epoch_i64 = durable_i64("dispatch lease epoch", epoch)?;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let authority = sqlx::query(&format!(
            "SELECT status, lease_owner, lease_epoch, cancel_requested, request, active_attempt_epoch, active_attempt_owner \
             FROM {p}_dispatch WHERE run_id = $1 FOR UPDATE"
        ))
        .bind(&run_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        let Some(authority) = authority else {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        };
        let status: String = authority.try_get("status").map_err(store_error)?;
        let persisted_epoch: i64 = authority.try_get("lease_epoch").map_err(store_error)?;
        let cancellation_requested: i64 =
            authority.try_get("cancel_requested").map_err(store_error)?;
        let active_attempt: Option<i64> = authority
            .try_get("active_attempt_epoch")
            .map_err(store_error)?;
        let attempt_owner: Option<String> = authority
            .try_get("active_attempt_owner")
            .map_err(store_error)?;
        let slot = awaken_dispatch_contract::PhysicalAttemptSlot::from_facts(
            attempt_owner.is_some(),
            active_attempt.is_some(),
            false,
            false,
        );
        let plan = pending_input_settlement_plan(
            crate::persisted_dispatch_transition(
                &status,
                persisted_epoch,
                cancellation_requested != 0,
            )?,
            epoch,
            outcome,
            slot,
            run_id,
            consumed,
        );
        if matches!(plan, PendingInputSettlementPlan::Fenced) {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        }
        let owner: Option<String> = authority.try_get("lease_owner").map_err(store_error)?;
        let Some(owner) = owner else {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        };
        let Json(request): Json<RunDispatch> = authority.try_get("request").map_err(store_error)?;
        let request_fingerprint = request.admission_fingerprint();
        let claim = RunClaim {
            run_id: run_id.clone(),
            owner,
            epoch,
        };
        // Fence first: mutate the dispatch row ONLY while the caller still holds the
        // current epoch. A stale owner (lower epoch) affects zero rows, so its settle
        // touches neither the dispatch nor its pending — the reclaimer's in-flight
        // state is inviolate.
        let dispatch_rows = match plan {
            PendingInputSettlementPlan::Done { .. } => sqlx::query(&format!(
                "DELETE FROM {p}_dispatch WHERE run_id = $1 \
                 AND status = 'running' AND lease_epoch = $2"
            ))
            .bind(&run_id.0)
            .bind(epoch_i64)
            .execute(&mut *tx)
            .await
            .map_err(store_error)?
            .rows_affected(),
            PendingInputSettlementPlan::Awaiting { next, .. } => sqlx::query(&format!(
                "UPDATE {p}_dispatch SET status = $3, lease_owner = NULL, \
                 lease_until = NULL, attempt_count = 0 WHERE run_id = $1 \
                 AND status = 'running' AND lease_epoch = $2"
            ))
            .bind(&run_id.0)
            .bind(epoch_i64)
            .bind(crate::dispatch_state_db(next.state))
            .execute(&mut *tx)
            .await
            .map_err(store_error)?
            .rows_affected(),
            PendingInputSettlementPlan::Fenced => unreachable!("handled before mutation"),
        };
        if dispatch_rows == 0 {
            // Fenced: the run was re-claimed under a higher epoch (or already gone).
            // Change nothing and report the loss so the stale caller abandons.
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        }
        if matches!(plan, PendingInputSettlementPlan::Done { .. }) {
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
            .map_err(store_error)?;
        }
        // The fence held; now reconcile the run's pending input.
        match plan {
            PendingInputSettlementPlan::Done { run_id, consumed } => {
                // Drop the run's own pending and anything else consumed this
                // attempt (e.g. unbound idle-thread input, ADR-0021).
                sqlx::query(&format!(
                    "DELETE FROM {p}_pending WHERE run_id = $1 OR message_id = ANY($2)"
                ))
                .bind(&run_id.0)
                .bind(consumed)
                .execute(&mut *tx)
                .await
                .map_err(store_error)?;
            }
            PendingInputSettlementPlan::Awaiting { consumed, .. } => {
                sqlx::query(&format!(
                    "DELETE FROM {p}_pending WHERE message_id = ANY($1)"
                ))
                .bind(consumed)
                .execute(&mut *tx)
                .await
                .map_err(store_error)?;
            }
            PendingInputSettlementPlan::Fenced => unreachable!("handled before mutation"),
        }
        insert_operation(&mut tx, &DispatchOperation::Settled { claim, outcome }).await?;
        tx.commit().await.map_err(store_error)?;
        Ok(SettleOutcome::Applied)
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchMaintenance for PostgresDispatchStore {
    async fn quarantine_retry_exhausted(
        &self,
        max_attempts: u64,
        expired_before_ms: u64,
    ) -> Result<usize, DispatchError> {
        let p = NS;
        let max_attempts_i64 = durable_i64("dispatch retry limit", max_attempts)?;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let rows = sqlx::query(&format!(
            "SELECT run_id, lease_owner, lease_epoch, attempt_count, cancel_requested, \
             active_attempt_owner, active_attempt_epoch \
             FROM {p}_dispatch WHERE status = 'running' \
             AND lease_until IS NOT NULL AND lease_until <= $1 \
             AND attempt_count >= $2 \
             AND (active_attempt_epoch IS NULL OR cancel_requested <> 0) FOR UPDATE"
        ))
        .bind(crate::clock::db_millis(expired_before_ms))
        .bind(max_attempts_i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_error)?;
        let mut quarantined = 0usize;
        for row in &rows {
            let owner = row
                .try_get::<Option<String>, _>("lease_owner")
                .map_err(store_error)?
                .ok_or_else(|| {
                    DispatchError::Rejected(
                        "expired running dispatch has no persisted lease owner".to_string(),
                    )
                })?;
            let epoch = row.try_get::<i64, _>("lease_epoch").map_err(store_error)?;
            let attempt_count = row
                .try_get::<i64, _>("attempt_count")
                .map_err(store_error)?;
            let cancellation_requested: i64 =
                row.try_get("cancel_requested").map_err(store_error)?;
            let claim_epoch = durable_u64("dispatch lease epoch", epoch)?;
            // The SQL above only prefilters. The slot disposition is decided here,
            // on the locked row, by the one kernel every backend shares.
            let active_attempt_owner: Option<String> =
                row.try_get("active_attempt_owner").map_err(store_error)?;
            let active_attempt_epoch: Option<i64> =
                row.try_get("active_attempt_epoch").map_err(store_error)?;
            let slot = PhysicalAttemptSlot::from_facts(
                active_attempt_owner.is_some(),
                active_attempt_epoch.is_some(),
                active_attempt_owner.as_deref() == Some(owner.as_str()),
                active_attempt_epoch == Some(epoch),
            );
            if !slot.permits_settlement()
                && !slot.quarantine_admits_occupation(cancellation_requested != 0)
            {
                continue;
            }
            let current = crate::persisted_dispatch_transition(
                "running",
                epoch,
                cancellation_requested != 0,
            )?;
            let awaken_dispatch_contract::GuardedTransition::Applied(next) =
                current.exhaust_retries(claim_epoch, true)
            else {
                continue;
            };
            let run_id: String = row.try_get("run_id").map_err(store_error)?;
            let changed = sqlx::query(&format!(
                "UPDATE {p}_dispatch SET status = $4, lease_owner = NULL, \
                 lease_until = NULL, dead_lettered_at = $1 WHERE run_id = $2 \
                 AND status = 'running' AND lease_epoch = $3"
            ))
            .bind(crate::clock::db_millis(expired_before_ms))
            .bind(&run_id)
            .bind(epoch)
            .bind(crate::dispatch_state_db(next.state))
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
            if changed.rows_affected() != 1 {
                continue;
            }
            let claim = RunClaim {
                run_id: RunId(run_id),
                owner,
                epoch: claim_epoch,
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
            quarantined += 1;
        }
        tx.commit().await.map_err(store_error)?;
        Ok(quarantined)
    }

    async fn purge_dead_letters(&self) -> Result<usize, DispatchError> {
        self.purge_dead_letters_with_cutoff(None).await
    }

    async fn purge_dead_letters_before(&self, cutoff_ms: u64) -> Result<usize, DispatchError> {
        self.purge_dead_letters_with_cutoff(Some(cutoff_ms)).await
    }
}

impl PostgresDispatchStore {
    async fn purge_dead_letters_with_cutoff(
        &self,
        cutoff_ms: Option<u64>,
    ) -> Result<usize, DispatchError> {
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let p = NS;
        // SQL projection of the same actually-empty physical slot used by
        // settlement. Check every retained fact: historical unbound, torn
        // owner/epoch, orphan binding and closing rows must survive GC.
        let age = if cutoff_ms.is_some() {
            " AND dead_lettered_at IS NOT NULL AND dead_lettered_at <= $1"
        } else {
            ""
        };
        let cond = format!(
            "status = 'dead_letter' AND active_attempt_owner IS NULL \
             AND active_attempt_epoch IS NULL AND active_attempt_binding IS NULL \
             AND active_attempt_closing = FALSE{age}"
        );
        // Freeze one candidate set under the same Dispatch row locks as
        // admission/requeue. Both deletes consume these exact ids, so a
        // concurrent closure/requeue cannot change the set between them.
        let candidates = format!("SELECT run_id FROM {p}_dispatch WHERE {cond} FOR UPDATE");
        let mut candidates = sqlx::query_scalar::<_, String>(&candidates);
        if let Some(cutoff_ms) = cutoff_ms {
            candidates = candidates.bind(crate::clock::db_millis(cutoff_ms));
        }
        let candidates = candidates.fetch_all(&mut *tx).await.map_err(store_error)?;
        sqlx::query(&format!("DELETE FROM {p}_pending WHERE run_id = ANY($1)"))
            .bind(&candidates)
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        let removed = sqlx::query(&format!("DELETE FROM {p}_dispatch WHERE run_id = ANY($1)"))
            .bind(&candidates)
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        tx.commit().await.map_err(store_error)?;
        Ok(removed.rows_affected() as usize)
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchReads for PostgresDispatchStore {
    async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError> {
        self.run_ids_by_status("dead_letter").await
    }

    async fn superseded(&self) -> Result<Vec<RunId>, DispatchError> {
        self.run_ids_by_status("superseded").await
    }

    async fn list_dispatches(&self) -> Result<Vec<DispatchSummary>, DispatchError> {
        self.read_dispatch_summaries(None).await
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchAdministration for PostgresDispatchStore {
    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let current = sqlx::query(&format!(
            "SELECT status, lease_epoch, cancel_requested FROM {p}_dispatch \
             WHERE run_id = $1 FOR UPDATE"
        ))
        .bind(&run_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        let Some(current) = current else {
            tx.commit().await.map_err(store_error)?;
            return Ok(false);
        };
        let status: String = current.try_get("status").map_err(store_error)?;
        let lease_epoch: i64 = current.try_get("lease_epoch").map_err(store_error)?;
        let cancellation_requested: i64 =
            current.try_get("cancel_requested").map_err(store_error)?;
        let Some(next) = crate::persisted_dispatch_transition(
            &status,
            lease_epoch,
            cancellation_requested != 0,
        )?
        .requeue_dead_letter() else {
            tx.commit().await.map_err(store_error)?;
            return Ok(false);
        };
        let result = sqlx::query(&format!(
            "UPDATE {p}_dispatch SET status = $2, attempt_count = 0, lease_owner = NULL, \
             lease_until = NULL WHERE run_id = $1 AND status = 'dead_letter' \
             AND cancel_requested = 0"
        ))
        .bind(&run_id.0)
        .bind(crate::dispatch_state_db(next.state))
        .execute(&mut *tx)
        .await
        .map_err(store_error)?;
        tx.commit().await.map_err(store_error)?;
        Ok(result.rows_affected() > 0)
    }

    async fn cancel(&self, run_id: &RunId) -> Result<Option<ThreadId>, DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let current = sqlx::query(&format!(
            "SELECT thread_id, status, lease_owner, lease_epoch, cancel_requested FROM {p}_dispatch \
             WHERE run_id = $1 AND status IN \
             ('reserved', 'reservation_running', 'pending', 'awaiting', 'running', 'dead_letter') \
             FOR UPDATE"
        ))
        .bind(&run_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        let Some(current) = current else {
            tx.commit().await.map_err(store_error)?;
            return Ok(None);
        };
        let thread = current
            .try_get::<String, _>("thread_id")
            .map_err(store_error)?;
        let status = current
            .try_get::<String, _>("status")
            .map_err(store_error)?;
        let previous_owner = current
            .try_get::<Option<String>, _>("lease_owner")
            .map_err(store_error)?;
        let previous_epoch = current
            .try_get::<i64, _>("lease_epoch")
            .map_err(store_error)?;
        let cancellation_requested: i64 =
            current.try_get("cancel_requested").map_err(store_error)?;
        let transition = crate::persisted_dispatch_transition(
            &status,
            previous_epoch,
            cancellation_requested != 0,
        )?;
        let awaken_dispatch_contract::CancelTransition::Applied {
            state: next,
            revoked_lease,
            clear_retry_deadline,
        } = transition.cancel().map_err(crate::transition_error)?
        else {
            tx.commit().await.map_err(store_error)?;
            return Ok(None);
        };
        let next_epoch = durable_i64("dispatch lease epoch", next.lease_epoch)?;
        sqlx::query(&format!(
            "UPDATE {p}_dispatch SET cancel_requested = $2, lease_epoch = $3, \
             lease_owner = CASE WHEN $4 THEN NULL ELSE lease_owner END, \
             lease_until = CASE WHEN $4 AND $5 = 'reserved' THEN 0 \
               WHEN $4 THEN NULL ELSE lease_until END, status = $5, \
             retry_not_before = CASE WHEN $6 THEN NULL ELSE retry_not_before END \
             WHERE run_id = $1"
        ))
        .bind(&run_id.0)
        .bind(i64::from(next.cancellation_requested))
        .bind(next_epoch)
        .bind(revoked_lease)
        .bind(crate::dispatch_state_db(next.state))
        .bind(clear_retry_deadline)
        .execute(&mut *tx)
        .await
        .map_err(store_error)?;
        if revoked_lease {
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
        tx.commit().await.map_err(store_error)?;
        Ok(Some(ThreadId(thread)))
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchAttemptControl for PostgresDispatchStore {
    async fn finish_attempt_closure(
        &self,
        record: &crate::AttemptClosureRecord,
    ) -> Result<SettleOutcome, DispatchError> {
        self.finish_physical_attempt_with(
            &record.attempt().claim,
            crate::dispatch::AttemptFinish::Independent(record),
        )
        .await
    }

    async fn admit_attempt_guarded(
        &self,
        claim: &RunClaim,
        binding: Option<&AttemptExecutionBinding>,
        mut guard: awaken_dispatch_contract::WorkerIncarnationGuard,
        handoff: bool,
    ) -> Result<AttemptAdmission, DispatchError> {
        let snapshot = guard.snapshot().clone();
        let identity = &snapshot.identity;
        if identity.lease_owner() != claim.owner || binding.is_none_or(|b| b.worker() != identity) {
            return Err(DispatchError::Rejected(
                "Registry guard does not match the physical attempt".into(),
            ));
        }
        let Some((tx, pool)) = guard.take_resource::<(Transaction<'static, Postgres>, PgPool)>()
        else {
            return Err(DispatchError::Rejected(
                "PostgreSQL admission requires its original Registry transaction".into(),
            ));
        };
        if !std::sync::Arc::ptr_eq(&pool.connect_options(), &self.pool.connect_options()) {
            return Err(DispatchError::Rejected(
                "Registry and Dispatch do not share the original process pool".into(),
            ));
        }
        self.admit_physical_attempt_transaction(claim, handoff, binding, tx, Some(snapshot))
            .await
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchControlReads for PostgresDispatchStore {
    async fn current_credential_binding(
        &self,
        claim: &RunClaim,
        candidate: &awaken_agent_contract::credential::CandidateFingerprint,
    ) -> Result<Option<AttemptCredentialBinding>, DispatchError> {
        let p = NS;
        let claim_epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let now_ms = durable_i64(
            "dispatch store time",
            crate::postgres_helpers::postgres_now_ms(&mut *tx).await?,
        )?;
        let row = sqlx::query(&format!(
            "SELECT credential_bindings FROM {p}_dispatch WHERE run_id = $1 \
             AND status = 'running' AND lease_owner = $2 AND lease_epoch = $3 \
             AND lease_until > $4"
        ))
        .bind(&claim.run_id.0)
        .bind(&claim.owner)
        .bind(claim_epoch)
        .bind(now_ms)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        let binding = row
            .map(|row| {
                row.try_get::<Option<Json<Vec<AttemptCredentialBinding>>>, _>("credential_bindings")
                    .map_err(store_error)
            })
            .transpose()?
            .flatten()
            .and_then(|bindings| {
                bindings
                    .0
                    .into_iter()
                    .find(|binding| &binding.candidate_fingerprint == candidate)
            });
        tx.commit().await.map_err(store_error)?;
        Ok(binding)
    }

    async fn dispatch_depths(&self) -> Result<Option<DispatchDepths>, DispatchError> {
        let now_ms = crate::postgres_helpers::postgres_now_ms(&self.pool).await?;
        let p = NS;
        let thread_available = thread_available_for_claim(p);
        let (runnable, leased): (i64, i64) = sqlx::query_as(&format!(
            "SELECT (SELECT COUNT(*) FROM {p}_dispatch d WHERE (\
             (d.status IN ('reserved', 'reservation_running') \
               AND d.lease_until IS NOT NULL AND d.lease_until <= $1 AND {}) OR \
             (d.status = 'pending' AND {thread_available}) OR \
             (d.status = 'running' AND d.lease_until <= $1) OR \
             (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS (\
               SELECT 1 FROM {p}_pending i WHERE i.run_id = d.run_id \
               AND (i.available_at IS NULL OR i.available_at <= $1)\
             )) AND {thread_available})) \
             AND (d.retry_not_before IS NULL \
                  OR d.retry_not_before <= $1)), \
             (SELECT COUNT(*) FROM {p}_dispatch d \
              WHERE d.status IN ('reservation_running', 'running') \
                AND d.lease_until > $1)",
            no_running_peer(p)
        ))
        .bind(crate::clock::db_millis(now_ms))
        .fetch_one(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(Some(DispatchDepths {
            runnable: durable_u64("runnable dispatch depth", runnable)?,
            leased: durable_u64("leased dispatch depth", leased)?,
        }))
    }

    async fn load_physical_attempt(
        &self,
        run_id: &RunId,
    ) -> Result<Option<PhysicalAttempt>, DispatchError> {
        let p = NS;
        let row = sqlx::query(&format!(
            "SELECT active_attempt_owner, active_attempt_epoch, active_attempt_binding, request, active_attempt_closing FROM {p}_dispatch WHERE run_id = $1"
        )).bind(&run_id.0).fetch_optional(&self.pool).await.map_err(store_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let epoch: Option<i64> = row.try_get("active_attempt_epoch").map_err(store_error)?;
        let Json(request): Json<RunDispatch> = row.try_get("request").map_err(store_error)?;
        let closing: bool = row.try_get("active_attempt_closing").map_err(store_error)?;
        crate::dispatch::decode_physical_attempt(
            run_id,
            row.try_get("active_attempt_owner").map_err(store_error)?,
            epoch
                .map(|v| durable_u64("dispatch attempt epoch", v))
                .transpose()?,
            row.try_get("active_attempt_binding").map_err(store_error)?,
            closing,
        )
        .map(|attempt| {
            attempt.map(|mut attempt| {
                attempt.thread_id = Some(request.thread_id().clone());
                attempt.session_thread_id = request.session_thread_id.clone();
                attempt
            })
        })
    }

    async fn completion_events_after(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<DispatchCompletion>, DispatchError> {
        load_completion_events(&self.pool, NS, after_sequence, limit).await
    }

    async fn scan_dispatch_ids(
        &self,
        after: Option<&RunId>,
        limit: usize,
    ) -> Result<Vec<RunId>, DispatchError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(limit).map_err(|_| {
            DispatchError::Rejected("dispatch scan limit exceeds database range".into())
        })?;
        let clause = if after.is_some() {
            "WHERE run_id > $2"
        } else {
            ""
        };
        let sql = format!("SELECT run_id FROM {NS}_dispatch {clause} ORDER BY run_id LIMIT $1");
        let query = sqlx::query_scalar::<_, String>(&sql).bind(limit);
        let query = if let Some(after) = after {
            query.bind(&after.0)
        } else {
            query
        };
        Ok(query
            .fetch_all(&self.pool)
            .await
            .map_err(store_error)?
            .into_iter()
            .map(RunId)
            .collect())
    }

    async fn dispatch_summary(
        &self,
        run_id: &RunId,
    ) -> Result<Option<DispatchSummary>, DispatchError> {
        Ok(self
            .read_dispatch_summaries(Some(run_id.0.clone()))
            .await?
            .pop())
    }
}

#[async_trait]
impl awaken_dispatch_contract::SessionRunAdmissionReads for PostgresDispatchStore {
    async fn observe_session_run_admission(
        &self,
        run_id: &RunId,
    ) -> Result<awaken_dispatch_contract::SessionRunAdmissionObservation, DispatchError> {
        use awaken_dispatch_contract::SessionRunAdmissionObservation;
        let mut connection = self.pool.acquire().await.map_err(store_error)?;
        let row = load_run_identity_evidence(&mut connection, NS, run_id).await?;
        if let Some(request) = decode_live_identity(&row)? {
            crate::dispatch::validate_observed_dispatch_key(
                run_id,
                &ThreadId(row.try_get("live_thread_id").map_err(store_error)?),
                &request,
            )?;
            return Ok(SessionRunAdmissionObservation::Live(Box::new(request)));
        }
        if decode_completed_identity(&row)?.is_some() {
            return Ok(SessionRunAdmissionObservation::Completed(
                decode_dispatch_completion(&row)?,
            ));
        }
        Ok(SessionRunAdmissionObservation::Missing)
    }
}

#[async_trait]
impl awaken_dispatch_contract::SessionRunReservations for PostgresDispatchStore {
    async fn adopt_pending_session_run(
        &self,
        expected_original: RunDispatch,
        command_fingerprint: awaken_session_types::SessionRunCommandFingerprint,
        owner: &str,
        lease_ms: u64,
    ) -> Result<Option<RunClaim>, DispatchError> {
        let lease_ms = crate::dispatch::validate_pending_session_run_adoption(
            &expected_original,
            &command_fingerprint,
            owner,
            lease_ms,
        )?;
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        lock_run_identity(&mut tx, &expected_original.run_id().0).await?;
        let row = sqlx::query(&format!(
            "SELECT request, status, lease_epoch, lease_owner, lease_until, cancel_requested, \
             worker_assignment, credential_bindings, credential_receipts, \
             active_attempt_owner, active_attempt_epoch, active_attempt_binding, active_attempt_closing \
             FROM {p}_dispatch WHERE run_id = $1 FOR UPDATE"
        ))
        .bind(&expected_original.run_id().0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        let Some(row) = row else {
            tx.commit().await.map_err(store_error)?;
            return Ok(None);
        };
        let Json(stored): Json<RunDispatch> = row.try_get("request").map_err(store_error)?;
        let status: String = row.try_get("status").map_err(store_error)?;
        let epoch: i64 = row.try_get("lease_epoch").map_err(store_error)?;
        let cancelled: i64 = row.try_get("cancel_requested").map_err(store_error)?;
        let lease_owner: Option<String> = row.try_get("lease_owner").map_err(store_error)?;
        let lease_until: Option<i64> = row.try_get("lease_until").map_err(store_error)?;
        let assignment: Option<serde_json::Value> =
            row.try_get("worker_assignment").map_err(store_error)?;
        let bindings: Option<Json<Vec<AttemptCredentialBinding>>> =
            row.try_get("credential_bindings").map_err(store_error)?;
        let receipts: Option<Json<Vec<CredentialRealizationReceipt>>> =
            row.try_get("credential_receipts").map_err(store_error)?;
        let physical_owner: Option<String> =
            row.try_get("active_attempt_owner").map_err(store_error)?;
        let physical_epoch: Option<i64> =
            row.try_get("active_attempt_epoch").map_err(store_error)?;
        let physical_binding: Option<serde_json::Value> =
            row.try_get("active_attempt_binding").map_err(store_error)?;
        let closing: i64 = row.try_get("active_attempt_closing").map_err(store_error)?;
        crate::dispatch::observed_physical_attempt_is_present(
            physical_owner.is_some(),
            physical_epoch.is_some(),
            physical_binding.is_some(),
            closing != 0,
        )?;
        let adopted = crate::dispatch::pending_session_run_adoption(
            &stored,
            &expected_original,
            crate::persisted_dispatch_transition(&status, epoch, cancelled != 0)?,
            lease_owner.is_none()
                && lease_until.is_none()
                && assignment.is_none()
                && bindings
                    .as_ref()
                    .is_none_or(|bindings| bindings.0.is_empty())
                && receipts
                    .as_ref()
                    .is_none_or(|receipts| receipts.0.is_empty()),
            PhysicalAttemptSlot::from_facts(
                physical_owner.is_some(),
                physical_epoch.is_some(),
                false,
                false,
            ),
            closing != 0,
            command_fingerprint,
        )?;
        let Some((request, next)) = adopted else {
            tx.commit().await.map_err(store_error)?;
            return Ok(None);
        };
        let epoch = durable_i64("dispatch lease epoch", next.lease_epoch)?;
        let now_ms = crate::postgres_helpers::postgres_now_ms(&mut *tx).await?;
        let deadline = crate::clock::deadline_millis(now_ms, lease_ms);
        sqlx::query(&format!(
            "UPDATE {p}_dispatch SET request = $1, status = 'reservation_running', \
             lease_owner = $2, lease_until = $3, lease_epoch = $4 WHERE run_id = $5"
        ))
        .bind(Json(&request))
        .bind(owner)
        .bind(crate::clock::db_millis(deadline))
        .bind(epoch)
        .bind(&request.run_id().0)
        .execute(&mut *tx)
        .await
        .map_err(store_error)?;
        tx.commit().await.map_err(store_error)?;
        Ok(Some(RunClaim {
            run_id: request.run_id().clone(),
            owner: owner.to_string(),
            epoch: next.lease_epoch,
        }))
    }

    async fn reserve_session_run(
        &self,
        request: RunDispatch,
        reservation_ttl_ms: u64,
    ) -> Result<SessionRunReservationOutcome, DispatchError> {
        let (request, reservation_ttl_ms) =
            validate_session_run_reservation_request(request, reservation_ttl_ms)?;
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let store_now_ms = crate::postgres_helpers::postgres_now_ms(&mut *tx).await?;
        let deadline = crate::clock::deadline_millis(store_now_ms, reservation_ttl_ms);
        lock_run_identity(&mut tx, &request.run_id().0).await?;
        let evidence = load_run_identity_evidence(&mut tx, p, request.run_id()).await?;
        if evidence
            .try_get::<Option<String>, _>("live_run_id")
            .map_err(store_error)?
            .is_some()
        {
            let status: String = evidence.try_get("status").map_err(store_error)?;
            let stored = crate::postgres_identity::decode_live_dispatch(&evidence)?;
            let state = DispatchState::from_db(&status).ok_or_else(|| {
                DispatchError::Rejected(format!("unknown persisted dispatch state `{status}`"))
            })?;
            let outcome = classify_live_session_run_reservation(&stored, state, &request);
            tx.commit().await.map_err(store_error)?;
            return Ok(outcome);
        }
        let completed = decode_completed_identity(&evidence)?;
        if let Some(request_fingerprint) = completed {
            let outcome = classify_completed_session_run_reservation(
                request_fingerprint.as_deref(),
                &request,
            );
            tx.commit().await.map_err(store_error)?;
            return Ok(outcome);
        }
        let options = SubmitOptions {
            supersede: request.session_run_replacement.supersedes_prior(),
            ..Default::default()
        };
        insert_dispatch_with_state(
            &mut tx,
            p,
            &request,
            &options,
            "reserved",
            Some(crate::clock::db_millis(deadline)),
        )
        .await?;
        tx.commit().await.map_err(store_error)?;
        Ok(SessionRunReservationOutcome::Reserved)
    }

    async fn activate_session_run_reservation(
        &self,
        run_id: &RunId,
        session_thread_id: &ThreadId,
        session_activity_epoch: u64,
    ) -> Result<SessionRunReservationActivation, DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        lock_run_identity(&mut tx, &run_id.0).await?;
        let current = sqlx::query(&format!(
            "SELECT status, request, lease_epoch, cancel_requested \
             FROM {p}_dispatch WHERE run_id = $1 FOR UPDATE"
        ))
        .bind(&run_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        let Some(row) = current else {
            let completed: bool = sqlx::query_scalar(&format!(
                "SELECT EXISTS(SELECT 1 FROM {p}_dispatch_completion WHERE run_id = $1)"
            ))
            .bind(&run_id.0)
            .fetch_one(&mut *tx)
            .await
            .map_err(store_error)?;
            let outcome = classify_session_run_reservation_activation(
                None,
                completed,
                session_thread_id,
                session_activity_epoch,
            )?
            .expect("missing reservation always has a closed outcome");
            tx.commit().await.map_err(store_error)?;
            return Ok(outcome);
        };
        let status: String = row.try_get("status").map_err(store_error)?;
        let Json(mut request): Json<RunDispatch> = row.try_get("request").map_err(store_error)?;
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
            let lease_epoch: i64 = row.try_get("lease_epoch").map_err(store_error)?;
            let cancellation_requested: i64 =
                row.try_get("cancel_requested").map_err(store_error)?;
            let next = crate::persisted_dispatch_transition(
                &status,
                lease_epoch,
                cancellation_requested != 0,
            )?
            .activate_reservation()
            .expect("the reservation classifier admitted only Reserved");
            request.session_activity_epoch = Some(session_activity_epoch);
            let changed = sqlx::query(&format!(
                "UPDATE {p}_dispatch SET request = $1, status = $3, \
                 lease_owner = NULL, lease_until = NULL WHERE run_id = $2 AND status = 'reserved'"
            ))
            .bind(Json(&request))
            .bind(&run_id.0)
            .bind(crate::dispatch_state_db(next.state))
            .execute(&mut *tx)
            .await
            .map_err(store_error)?
            .rows_affected();
            if changed == 1 {
                SessionRunReservationActivation::Activated
            } else {
                SessionRunReservationActivation::RecoveryClaimed
            }
        };
        tx.commit().await.map_err(store_error)?;
        Ok(outcome)
    }

    async fn reject_session_run_reservation(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        lock_run_identity(&mut tx, &run_id.0).await?;
        let current = sqlx::query(&format!(
            "SELECT status, lease_epoch, cancel_requested FROM {p}_dispatch \
             WHERE run_id = $1 FOR UPDATE"
        ))
        .bind(&run_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        let removable = if let Some(current) = current {
            let status: String = current.try_get("status").map_err(store_error)?;
            let lease_epoch: i64 = current.try_get("lease_epoch").map_err(store_error)?;
            let cancellation_requested: i64 =
                current.try_get("cancel_requested").map_err(store_error)?;
            matches!(
                crate::persisted_dispatch_transition(
                    &status,
                    lease_epoch,
                    cancellation_requested != 0,
                )?
                .reject_reservation(),
                awaken_dispatch_contract::GuardedTransition::Removed
            )
        } else {
            false
        };
        if !removable {
            tx.commit().await.map_err(store_error)?;
            return Ok(false);
        }
        let changed = sqlx::query(&format!(
            "DELETE FROM {p}_dispatch WHERE run_id = $1 AND status = 'reserved'"
        ))
        .bind(&run_id.0)
        .execute(&mut *tx)
        .await
        .map_err(store_error)?
        .rows_affected();
        if changed == 1 {
            sqlx::query(&format!("DELETE FROM {p}_pending WHERE run_id = $1"))
                .bind(&run_id.0)
                .execute(&mut *tx)
                .await
                .map_err(store_error)?;
        }
        tx.commit().await.map_err(store_error)?;
        Ok(changed == 1)
    }

    async fn enqueue_session_child(
        &self,
        request: RunDispatch,
        admission: SessionChildAdmission,
    ) -> Result<(), DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        lock_run_identity(&mut tx, &request.run_id().0).await?;
        if exact_run_replay(&mut tx, p, &request).await? {
            tx.commit().await.map_err(store_error)?;
            return Ok(());
        }
        admit_session_child(&mut tx, p, &request, &admission).await?;
        insert_new_dispatch(&mut tx, p, &request, &SubmitOptions::default()).await?;
        tx.commit().await.map_err(store_error)?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_dispatch_contract::SessionReservationAuthority for PostgresDispatchStore {
    async fn lock_session_run_reservation_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        self.lock_claim_epoch_in_status(claim, "reservation_running")
            .await
    }
}

impl PostgresDispatchStore {
    pub(super) async fn close_physical_attempt(
        &self,
        claim: &RunClaim,
    ) -> Result<SettleOutcome, DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let row = sqlx::query(&format!("SELECT active_attempt_owner, active_attempt_epoch FROM {p}_dispatch WHERE run_id = $1 FOR UPDATE"))
            .bind(&claim.run_id.0).fetch_optional(&mut *tx).await.map_err(store_error)?;
        let Some(row) = row else {
            return Ok(SettleOutcome::Fenced);
        };
        let owner: Option<String> = row.try_get("active_attempt_owner").map_err(store_error)?;
        let epoch = row
            .try_get::<Option<i64>, _>("active_attempt_epoch")
            .map_err(store_error)?
            .map(|value| durable_u64("physical attempt epoch", value))
            .transpose()?;
        let slot = PhysicalAttemptSlot::from_facts(
            owner.is_some(),
            epoch.is_some(),
            owner.as_deref() == Some(&claim.owner),
            epoch == Some(claim.epoch),
        );
        let outcome = crate::dispatch::decide_physical_attempt_close(slot)?;
        if outcome == SettleOutcome::Applied {
            sqlx::query(&format!(
                "UPDATE {p}_dispatch SET active_attempt_closing = TRUE WHERE run_id = $1"
            ))
            .bind(&claim.run_id.0)
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        }
        tx.commit().await.map_err(store_error)?;
        Ok(outcome)
    }
}
