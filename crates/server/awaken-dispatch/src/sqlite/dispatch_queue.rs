// @formal entrypoint credential_effect.boundary_preserves_exact_materialization_authority
// @formal entrypoint ingress.pending_input_exactly_once
// @formal entrypoint ingress.settle_fence
// @formal entrypoint remote_worker.pending_signal_is_consumed_by_commit
// @formal entrypoint remote_worker.single_live_claim_owner
// @formal entrypoint run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
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
use awaken_time::WallClockExt;

/// The first claimable run of one priority band, in the band's SQL order: the
/// first candidate whose next epoch admits the caller's credential capabilities.
#[allow(clippy::too_many_arguments)]
async fn first_admissible_in_band(
    tx: &mut SqliteConnection,
    sql: &str,
    bind_now: bool,
    terminal_resolution: bool,
    now_ms: u64,
    capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
) -> Result<Option<String>, DispatchError> {
    let mut query = sqlx::query_as::<_, (String, String, i64, i64)>(sql);
    if bind_now {
        query = query.bind(crate::clock::db_millis(now_ms));
    }
    let mut rows = query.fetch(&mut *tx);
    while let Some((run_id, request_json, cancellation_requested, lease_epoch)) =
        rows.try_next().await.map_err(store_error)?
    {
        let cancellation_requested = cancellation_requested != 0;
        let request: RunDispatch = serde_json::from_str(&request_json).map_err(json_err)?;
        let operation = claim_credential_operation(terminal_resolution, cancellation_requested);
        if next_claim_epoch(lease_epoch).is_ok_and(|epoch| {
            can_admit_claim_credentials(&request, capabilities, epoch, now_ms, operation)
        }) {
            return Ok(Some(run_id));
        }
    }
    Ok(None)
}

/// Select the next claimable run, quiescent physical slots first: recover an
/// expired reservation, then an expired lease, then wake an awaiting run with
/// pending input, then a fresh pending run.
async fn select_claim_band(
    tx: &mut SqliteConnection,
    p: &str,
    quiescent_only: bool,
    now_ms: u64,
    capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
) -> Result<Option<String>, DispatchError> {
    let physical_eligible = if quiescent_only {
        "d.active_attempt_owner IS NULL AND d.active_attempt_epoch IS NULL"
    } else {
        "1 = 1"
    };
    // Priority: recover an expired lease, then wake an awaiting run with
    // pending input, then a fresh pending run. SQLite has no SKIP LOCKED;
    // the IMMEDIATE transaction is the single-owner guard.
    let local_eligible = "(d.cancel_requested = 1 OR (\
        COALESCE(json_extract(d.request, '$.placement.location'), 'remote_preferred') \
            <> 'remote_required' AND \
        COALESCE(json_array_length(json_extract(\
            d.request, '$.placement.required_credentials')), 0) = 0))";
    let retry_due = "(d.retry_not_before IS NULL \
        OR d.retry_not_before <= ?1)";
    let reservation_recovery = format!(
        "SELECT d.run_id, d.request, d.cancel_requested, d.lease_epoch FROM {p}_dispatch d \
         WHERE {physical_eligible} AND d.status IN ('reserved', 'reservation_running') \
         AND d.lease_until IS NOT NULL AND d.lease_until <= ?1 AND {retry_due} \
        AND {} ORDER BY d.created_at",
        no_running_peer(p)
    );
    let recovery = format!(
        "SELECT d.run_id, d.request, d.cancel_requested, d.lease_epoch FROM {p}_dispatch d \
         WHERE {physical_eligible} AND d.status = 'running' AND d.lease_until IS NOT NULL \
         AND d.lease_until <= ?1 AND {retry_due} AND {local_eligible} \
         ORDER BY d.cancel_requested DESC, d.created_at"
    );
    // ADR-0022 single writer is an open-Run fence: an Awaiting Run keeps
    // ownership until its exact reply/cancel resumes that same row.
    let thread_available = thread_available_for_claim(p);
    let wake = format!(
        "SELECT d.run_id, d.request, d.cancel_requested, d.lease_epoch FROM {p}_dispatch d \
         WHERE {physical_eligible} AND d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS ( \
             SELECT 1 FROM {p}_pending pe WHERE pe.run_id = d.run_id \
         AND (pe.available_at IS NULL OR pe.available_at <= ?1))) \
         AND {thread_available} AND {retry_due} AND {local_eligible} \
         ORDER BY d.cancel_requested DESC, d.created_at"
    );
    let fresh = format!(
        "SELECT d.run_id, d.request, d.cancel_requested, d.lease_epoch FROM {p}_dispatch d \
         WHERE {physical_eligible} AND d.status = 'pending' AND {thread_available} \
         AND {retry_due} AND {local_eligible} \
         ORDER BY d.cancel_requested DESC, d.priority DESC, d.created_at"
    );

    let picked =
        match first_admissible_in_band(tx, &reservation_recovery, true, true, now_ms, capabilities)
            .await?
        {
            Some(found) => Some(found),
            None => {
                match first_admissible_in_band(tx, &recovery, true, false, now_ms, capabilities)
                    .await?
                {
                    Some(found) => Some(found),
                    None => {
                        match first_admissible_in_band(tx, &wake, true, false, now_ms, capabilities)
                            .await?
                        {
                            Some(found) => Some(found),
                            None => {
                                first_admissible_in_band(
                                    tx,
                                    &fresh,
                                    true,
                                    false,
                                    now_ms,
                                    capabilities,
                                )
                                .await?
                            }
                        }
                    }
                }
            }
        };
    Ok(picked)
}

// SQLite realization of the one neutral DispatchQueue contract. Included by
// `sqlite.rs` so the public adapter and trait implementation paths do not move.

#[async_trait]
impl awaken_dispatch_contract::DispatchCommitAuthority for SqliteDispatchStore {
    async fn worker_owns_run(
        &self,
        identity: &crate::WorkerIdentity,
        run_id: &RunId,
    ) -> Result<Option<RunClaim>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let run = run_id.0.clone();
        let owner = identity.lease_owner();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let epoch = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT lease_epoch FROM {p}_dispatch \
             WHERE run_id = ?1 AND status = 'running' \
             AND lease_owner = ?2 AND lease_until IS NOT NULL \
             AND lease_until > ?3 AND cancel_requested = 0"
        ))
        .bind(&run)
        .bind(&owner)
        .bind(crate::clock::db_millis(now_ms))
        .fetch_optional(&mut *conn)
        .await
        .map_err(store_error)?;
        epoch
            .map(|epoch| {
                Ok(RunClaim {
                    run_id: RunId(run),
                    owner,
                    epoch: durable_u64("dispatch lease epoch", epoch)?,
                })
            })
            .transpose()
    }

    async fn lock_commit_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        self.lock_claim_epoch_in_status(claim, "running").await
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchClaims for SqliteDispatchStore {
    async fn resolve_claimed_session_run_reservation(
        &self,
        claim: &RunClaim,
        resolution: SessionRunReservationResolution,
    ) -> Result<SettleOutcome, DispatchError> {
        let resolution = validate_session_run_reservation_resolution(resolution)?;
        let store_now_ms = self.clock.now_ms();
        let claim = claim.clone();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let claim_epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
        let mut tx = immediate(&mut conn).await?;
        let current: Option<(String, i64)> = sqlx::query_as(&format!(
            "SELECT request, cancel_requested FROM {p}_dispatch WHERE run_id = ?1 \
             AND status = 'reservation_running' AND lease_owner = ?2 \
             AND lease_epoch = ?3"
        ))
        .bind(&claim.run_id.0)
        .bind(&claim.owner)
        .bind(claim_epoch)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        let Some((request_json, cancellation_requested)) = current else {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        };
        let mut request: RunDispatch = serde_json::from_str(&request_json).map_err(json_err)?;
        let transition = crate::persisted_dispatch_transition(
            "reservation_running",
            claim_epoch,
            cancellation_requested != 0,
        )?
        .resolve_reservation(
            claim.epoch,
            true,
            match resolution {
                SessionRunReservationResolution::Admitted { .. } => Some(true),
                SessionRunReservationResolution::Retry { .. } => Some(false),
                SessionRunReservationResolution::Rejected => None,
            },
        );
        let changed = match resolution {
            SessionRunReservationResolution::Admitted {
                session_activity_epoch,
            } => {
                request.session_activity_epoch = Some(session_activity_epoch);
                let awaken_dispatch_contract::GuardedTransition::Applied(next) = transition
                else {
                    unreachable!("the exact recovery claim is admitted by the kernel")
                };
                sqlx::query(&format!(
                    "UPDATE {p}_dispatch SET request = ?1, status = ?5, \
                     lease_owner = NULL, lease_until = NULL, worker_assignment = NULL, \
                     credential_bindings = NULL, credential_receipts = NULL \
                     WHERE run_id = ?2 AND status = 'reservation_running' \
                     AND lease_owner = ?3 AND lease_epoch = ?4"
                ))
                .bind(json(&request)?)
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
                let awaken_dispatch_contract::GuardedTransition::Applied(next) = transition
                else {
                    unreachable!("the exact recovery claim is admitted by the kernel")
                };
                sqlx::query(&format!(
                    "UPDATE {p}_dispatch SET status = ?5, lease_owner = NULL, \
                     lease_until = ?1, worker_assignment = NULL, credential_bindings = NULL, \
                     credential_receipts = NULL, \
                     created_at = strftime('%Y-%m-%d %H:%M:%f', 'now') \
                     WHERE run_id = ?2 AND status = 'reservation_running' \
                     AND lease_owner = ?3 AND lease_epoch = ?4"
                ))
                .bind(crate::clock::db_millis(crate::clock::deadline_millis(
                    store_now_ms,
                    reservation_ttl_ms,
                )))
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
                    "DELETE FROM {p}_dispatch WHERE run_id = ?1 \
                     AND status = 'reservation_running' AND lease_owner = ?2 \
                     AND lease_epoch = ?3"
                ))
                .bind(&claim.run_id.0)
                .bind(&claim.owner)
                .bind(claim_epoch)
                .execute(&mut *tx)
                .await
                .map_err(store_error)?
                .rows_affected();
                if changed == 1 {
                    sqlx::query(&format!("DELETE FROM {p}_pending WHERE run_id = ?1"))
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
        let now_ms = self.clock.now_ms();
        let run_id = request.run_id().0.clone();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let mut tx = immediate(&mut conn).await?;
        if exact_run_replay(&mut tx, p, &request).await? {
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
            tx.commit().await.map_err(store_error)?;
            return Ok(claimed);
        }
        validate_executable_dispatch_admission(&request)?;
        if !can_claim_locally(&request.placement) {
            tx.commit().await.map_err(store_error)?;
            return Ok(None);
        }
        insert_new_dispatch(&mut tx, p, &request, &SubmitOptions::default()).await?;
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
        tx.commit().await.map_err(store_error)?;
        Ok(claimed)
    }

    async fn claim_new_run_compatible(
        &self,
        request: RunDispatch,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let run_id = request.run_id().0.clone();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let mut tx = immediate(&mut conn).await?;
        let owner = worker.identity.lease_owner();
        if exact_run_replay(&mut tx, p, &request).await? {
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
            tx.commit().await.map_err(store_error)?;
            return Ok(claimed);
        }
        validate_executable_dispatch_admission(&request)?;
        if can_assign(worker, &request.placement, None, false, now_ms).is_err() {
            tx.commit().await.map_err(store_error)?;
            return Ok(None);
        }
        insert_new_dispatch(&mut tx, p, &request, &SubmitOptions::default()).await?;
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
        let now_ms = self.clock.now_ms();
        let input = normalize_pending_millis(input);
        let run_id = input.run_id.0.clone();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let mut tx = immediate(&mut conn).await?;
        append_pending_row(&mut tx, p, &input).await?;
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
        tx.commit().await.map_err(store_error)?;
        Ok(claimed)
    }

    async fn deliver_and_claim_compatible(
        &self,
        input: PendingInput,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let input = normalize_pending_millis(input);
        let run_id = input.run_id.0.clone();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let mut tx = immediate(&mut conn).await?;
        append_pending_row(&mut tx, p, &input).await?;
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
        tx.commit().await.map_err(store_error)?;
        Ok(claimed)
    }

    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let mut tx = immediate(&mut conn).await?;
        let picked = match select_claim_band(&mut tx, p, true, now_ms, capabilities).await? {
            Some(run_id) => Some(run_id),
            None => select_claim_band(&mut tx, p, false, now_ms, capabilities).await?,
        };

        let Some(run_id) = picked else {
            return Ok(None);
        };
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
        tx.commit().await.map_err(store_error)?;
        Ok(claimed)
    }

    async fn claim_compatible(
        &self,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let mut tx = immediate(&mut conn).await?;
        let thread_available = thread_available_for_claim(p);
        let sql = format!(
            "SELECT d.run_id, d.request, d.sandbox, d.worker_assignment, d.status, d.cancel_requested, d.lease_epoch FROM {p}_dispatch d WHERE \
             ((d.status IN ('reserved', 'reservation_running') AND \
               d.lease_until IS NOT NULL AND d.lease_until <= ?1 AND {}) OR \
             (d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until <= ?1) OR \
             (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS (SELECT 1 FROM {p}_pending pe \
               WHERE pe.run_id = d.run_id AND (pe.available_at IS NULL OR pe.available_at <= ?1)) \
               ) AND {thread_available}) OR \
             (d.status = 'pending' AND {thread_available})) \
             AND (d.retry_not_before IS NULL OR d.retry_not_before <= ?1) \
             ORDER BY CASE WHEN d.active_attempt_owner IS NOT NULL OR d.active_attempt_epoch IS NOT NULL THEN 1 ELSE 0 END, \
                      CASE WHEN d.cancel_requested = 1 THEN 0 WHEN d.status = 'running' THEN 1 WHEN d.status = 'awaiting' THEN 2 ELSE 3 END, \
                      d.priority DESC, d.created_at",
            no_running_peer(p)
        );
        let capabilities = installed_worker_credential_capabilities(worker)?;
        let selected = {
            let mut rows = sqlx::query_as::<_, CompatibleCandidateRow>(&sql)
                .bind(crate::clock::db_millis(now_ms))
                .fetch(&mut *tx);
            let mut selected = None;
            while let Some(row) = rows.try_next().await.map_err(store_error)? {
                let (
                    run_id,
                    request_json,
                    sandbox,
                    previous_json,
                    status,
                    cancellation_requested,
                    lease_epoch,
                ) = row;
                let cancellation_requested = cancellation_requested != 0;
                let request: RunDispatch = serde_json::from_str(&request_json).map_err(json_err)?;
                let previous: Option<WorkerAssignment> = previous_json
                    .map(|value| serde_json::from_str(&value).map_err(json_err))
                    .transpose()?;
                let next_epoch = next_claim_epoch(lease_epoch)?;
                let credentials_admitted = can_admit_claim_credentials(
                    &request,
                    &capabilities,
                    next_epoch,
                    now_ms,
                    claim_credential_operation(false, cancellation_requested),
                );
                if matches!(status.as_str(), "reserved" | "reservation_running")
                    || (cancellation_requested && credentials_admitted)
                    || (can_assign(
                        worker,
                        &request.placement,
                        previous.as_ref(),
                        sandbox.is_some(),
                        now_ms,
                    )
                    .is_ok()
                        && credentials_admitted)
                {
                    selected = Some(run_id);
                    break;
                }
            }
            selected
        };
        let Some(run_id) = selected else {
            tx.commit().await.map_err(store_error)?;
            return Ok(None);
        };
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
        tx.commit().await.map_err(store_error)?;
        Ok(claimed)
    }

    async fn claim_run(
        &self,
        requested_run: &RunId,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let (_authority, mut conn) = self.with_conn().await?;
        let mut tx = immediate(&mut conn).await?;
        let claimed = claim_exact_transaction(
            &mut tx,
            &requested_run.0,
            owner,
            lease_ms,
            now_ms,
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
        let now_ms = self.clock.now_ms();
        let now_ms = crate::clock::normalize_millis(now_ms);
        let (_authority, mut conn) = self.with_conn().await?;
        claim_retry_exhausted_transaction(&mut conn, NS, owner, lease_ms, now_ms, max_attempts)
            .await
    }

    async fn claim_run_compatible(
        &self,
        requested_run: &RunId,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let (_authority, mut conn) = self.with_conn().await?;
        let mut tx = immediate(&mut conn).await?;
        let claimed = claim_exact_transaction(
            &mut tx,
            &requested_run.0,
            &worker.identity.lease_owner(),
            lease_ms,
            now_ms,
            Some(worker),
            &installed_worker_credential_capabilities(worker)?,
        )
        .await?;
        tx.commit().await.map_err(store_error)?;
        Ok(claimed)
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchEnqueue for SqliteDispatchStore {
    async fn enqueue_with(
        &self,
        request: RunDispatch,
        options: SubmitOptions,
    ) -> Result<(), DispatchError> {
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let mut tx = immediate(&mut conn).await?;

        // A replayed completed run is a no-op before supersession, so it
        // cannot mutate sibling rows on its thread (ADR-0060).
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
impl awaken_dispatch_contract::DispatchExecution for SqliteDispatchStore {
    async fn renew_lease(
        &self,
        claim: &RunClaim,
        lease_ms: u64,
    ) -> Result<LeaseRenewalOutcome, DispatchError> {
        let now_ms = self.clock.now_ms();
        let claim = claim.clone();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let claim_epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
        let mut tx = immediate(&mut conn).await?;
        let current: Option<(
            String,
            Option<String>,
            i64,
            i64,
            Option<String>,
            Option<i64>,
        )> = sqlx::query_as(&format!(
            "SELECT status, lease_owner, lease_epoch, cancel_requested, \
                 active_attempt_owner, active_attempt_epoch FROM {p}_dispatch \
                 WHERE run_id = ?1"
        ))
        .bind(&claim.run_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        let Some((
            status,
            owner,
            persisted_epoch,
            cancellation_requested,
            attempt_owner,
            attempt_epoch,
        )) = current
        else {
            tx.commit().await.map_err(store_error)?;
            return Ok(LeaseRenewalOutcome::Lost);
        };
        let current_claim = matches!(status.as_str(), "running" | "reservation_running")
            && owner.as_deref() == Some(&claim.owner)
            && persisted_epoch == claim_epoch;
        let cancelled_physical_attempt = cancellation_requested != 0
            && attempt_owner.as_deref() == Some(&claim.owner)
            && attempt_epoch == Some(claim_epoch);
        let outcome = decide_lease_renewal(current_claim, cancelled_physical_attempt);
        if outcome == LeaseRenewalOutcome::Current {
            sqlx::query(&format!(
                "UPDATE {p}_dispatch SET lease_until = ?1 WHERE run_id = ?2 \
                 AND status IN ('running', 'reservation_running') \
                 AND lease_owner = ?3 AND lease_epoch = ?4"
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
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let claim_epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
        let changed = sqlx::query(&format!(
            "UPDATE {p}_dispatch SET sandbox = ?1 WHERE run_id = ?2 \
             AND status = 'running' AND lease_owner = ?3 AND lease_epoch = ?4"
        ))
        .bind(sandbox_ref)
        .bind(&claim.run_id.0)
        .bind(&claim.owner)
        .bind(claim_epoch)
        .execute(&mut *conn)
        .await
        .map_err(store_error)?
        .rows_affected();
        Ok(if changed == 1 {
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
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let claim_epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
        let mut tx = immediate(&mut conn).await?;
        let current: Option<(Option<String>, Option<String>)> = sqlx::query_as(&format!(
            "SELECT credential_bindings, credential_receipts FROM {p}_dispatch \
             WHERE run_id = ?1 AND status = 'running' \
             AND lease_owner = ?2 AND lease_epoch = ?3"
        ))
        .bind(&claim.run_id.0)
        .bind(&claim.owner)
        .bind(claim_epoch)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        let Some((bindings_json, receipts_json)) = current else {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        };
        let bindings: Vec<AttemptCredentialBinding> = bindings_json
            .map(|value| serde_json::from_str(&value).map_err(json_err))
            .transpose()?
            .unwrap_or_default();
        verify_credential_realization_receipt(&bindings, &receipt)
            .map_err(|error| DispatchError::Rejected(error.to_string()))?;
        let mut receipts: Vec<CredentialRealizationReceipt> = receipts_json
            .map(|value| serde_json::from_str(&value).map_err(json_err))
            .transpose()?
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
            "UPDATE {p}_dispatch SET credential_receipts = ?1 \
             WHERE run_id = ?2 AND status = 'running' \
             AND lease_owner = ?3 AND lease_epoch = ?4"
        ))
        .bind(json(&receipts)?)
        .bind(&claim.run_id.0)
        .bind(&claim.owner)
        .bind(claim_epoch)
        .execute(&mut *tx)
        .await
        .map_err(store_error)?
        .rows_affected();
        if changed != 1 {
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
        let now_ms = self.clock.now_ms();
        self.admit_physical_attempt(claim, now_ms, false, _binding)
            .await
    }

    async fn handoff_attempt(
        &self,
        claim: &RunClaim,
        _binding: Option<&awaken_dispatch_contract::dispatch::AttemptExecutionBinding>,
    ) -> Result<AttemptAdmission, DispatchError> {
        let now_ms = self.clock.now_ms();
        self.admit_physical_attempt(claim, now_ms, true, _binding)
            .await
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
        let claim = claim.clone();
        let now_ms = self.clock.now_ms();
        let (_authority, mut conn) = self.with_conn().await?;
        release_claim_transaction(&mut conn, NS, claim, None, now_ms).await
    }

    async fn defer_claim(
        &self,
        claim: &RunClaim,
        retry_after_ms: u64,
    ) -> Result<SettleOutcome, DispatchError> {
        let claim = claim.clone();
        let now_ms = self.clock.now_ms();
        let retry_not_before = durable_i64(
            "dispatch resolution retry deadline",
            crate::clock::deadline_millis(now_ms, retry_after_ms),
        )?;
        let (_authority, mut conn) = self.with_conn().await?;
        release_claim_transaction(&mut conn, NS, claim, Some(retry_not_before), now_ms).await
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchClaimOwnership for SqliteDispatchStore {
    /// The store's own clock decides liveness, exactly as it does for every
    /// other deadline this backend judges. This used to ride the trait default
    /// on a caller-supplied `now_ms`; the caller no longer has a say in when
    /// "now" is for rows this store owns.
    async fn claim_is_current(&self, claim: &RunClaim) -> Result<bool, DispatchError> {
        let now_ms = self.clock.now_ms();
        Ok(self
            .lock_commit_epoch(claim)
            .await?
            .is_some_and(|guard| guard.is_live_at(now_ms)))
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchSettlement for SqliteDispatchStore {
    async fn settle(
        &self,
        run_id: &RunId,
        epoch: u64,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<SettleOutcome, DispatchError> {
        let run_id = run_id.0.clone();
        let consumed = consumed.to_vec();
        let now_ms = self.clock.now_ms();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let epoch_i64 = durable_i64("dispatch lease epoch", epoch)?;
        let mut tx = immediate(&mut conn).await?;
        let authority: Option<(
            String,
            Option<String>,
            i64,
            i64,
            String,
            Option<i64>,
            Option<String>,
        )> = sqlx::query_as(&format!(
            "SELECT status, lease_owner, lease_epoch, cancel_requested, request, active_attempt_epoch, active_attempt_owner \
             FROM {p}_dispatch WHERE run_id = ?1"
        ))
        .bind(&run_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        let Some((
            status,
            owner,
            persisted_epoch,
            cancellation_requested,
            request_json,
            active_attempt,
            attempt_owner,
        )) = authority
        else {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        };
        let slot = awaken_dispatch_contract::PhysicalAttemptSlot::from_facts(
            attempt_owner.is_some(),
            active_attempt.is_some(),
            false,
            false,
        );
        let settled_run_id = RunId(run_id.clone());
        let plan = pending_input_settlement_plan(
            crate::persisted_dispatch_transition(
                &status,
                persisted_epoch,
                cancellation_requested != 0,
            )?,
            epoch,
            outcome,
            slot,
            &settled_run_id,
            &consumed,
        );
        if matches!(plan, PendingInputSettlementPlan::Fenced) {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        }
        let Some(owner) = owner else {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        };
        let request: RunDispatch = serde_json::from_str(&request_json).map_err(json_err)?;
        let request_fingerprint = request.admission_fingerprint();
        let claim = RunClaim {
            run_id: RunId(run_id.clone()),
            owner,
            epoch,
        };
        // Fence first: mutate the dispatch row ONLY while the caller still holds
        // the current epoch. A stale owner (lower epoch) affects zero rows, so
        // its settle touches neither the dispatch nor its pending.
        let dispatch_rows = match plan {
            PendingInputSettlementPlan::Done { .. } => sqlx::query(&format!(
                "DELETE FROM {p}_dispatch WHERE run_id = ?1 \
                 AND status = 'running' AND lease_epoch = ?2"
            ))
            .bind(&run_id)
            .bind(epoch_i64)
            .execute(&mut *tx)
            .await
            .map_err(store_error)?
            .rows_affected(),
            PendingInputSettlementPlan::Awaiting { next, .. } => sqlx::query(&format!(
                "UPDATE {p}_dispatch SET status = ?3, lease_owner = NULL, \
                 lease_until = NULL, attempt_count = 0 \
                 WHERE run_id = ?1 AND status = 'running' AND lease_epoch = ?2"
            ))
            .bind(&run_id)
            .bind(epoch_i64)
            .bind(crate::dispatch_state_db(next.state))
            .execute(&mut *tx)
            .await
            .map_err(store_error)?
            .rows_affected(),
            PendingInputSettlementPlan::Fenced => unreachable!("handled before mutation"),
        };
        if dispatch_rows == 0 {
            // Fenced: re-claimed under a higher epoch (or already gone). Change
            // nothing and report the loss so the stale caller abandons.
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        }
        if matches!(plan, PendingInputSettlementPlan::Done { .. }) {
            sqlx::query(&format!(
                "INSERT INTO {p}_dispatch_completion \
                 (run_id, request_fingerprint, thread_id, session_thread_id) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(run_id) DO NOTHING"
            ))
            .bind(&run_id)
            .bind(request_fingerprint)
            .bind(&request.thread_id().0)
            .bind(request.session_thread_id.as_ref().map(|thread| &thread.0))
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        }
        // The fence held; now reconcile the run's pending input.
        match plan {
            PendingInputSettlementPlan::Done { run_id, consumed } => {
                sqlx::query(&format!("DELETE FROM {p}_pending WHERE run_id = ?1"))
                    .bind(&run_id.0)
                    .execute(&mut *tx)
                    .await
                    .map_err(store_error)?;
                // Also drop anything else consumed this attempt (e.g. unbound
                // idle-thread input, ADR-0021).
                for message_id in consumed {
                    sqlx::query(&format!("DELETE FROM {p}_pending WHERE message_id = ?1"))
                        .bind(message_id)
                        .execute(&mut *tx)
                        .await
                        .map_err(store_error)?;
                }
            }
            PendingInputSettlementPlan::Awaiting { consumed, .. } => {
                for message_id in consumed {
                    sqlx::query(&format!("DELETE FROM {p}_pending WHERE message_id = ?1"))
                        .bind(message_id)
                        .execute(&mut *tx)
                        .await
                        .map_err(store_error)?;
                }
            }
            PendingInputSettlementPlan::Fenced => unreachable!("handled before mutation"),
        }
        insert_operation(
            &mut tx,
            p,
            &DispatchOperation::Settled { claim, outcome },
            now_ms,
        )
        .await?;
        tx.commit().await.map_err(store_error)?;
        Ok(SettleOutcome::Applied)
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchMaintenance for SqliteDispatchStore {
    async fn quarantine_retry_exhausted(
        &self,
        max_attempts: u64,
        expired_before_ms: u64,
    ) -> Result<usize, DispatchError> {
        let now_ms = self.clock.now_ms();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let max_attempts_i64 = durable_i64("dispatch retry limit", max_attempts)?;
        let mut tx = immediate(&mut conn).await?;
        let candidates = sqlx::query_as::<
            _,
            (
                String,
                Option<String>,
                i64,
                i64,
                i64,
                Option<String>,
                Option<i64>,
            ),
        >(&format!(
            "SELECT run_id, lease_owner, lease_epoch, attempt_count, cancel_requested, \
             active_attempt_owner, active_attempt_epoch \
             FROM {p}_dispatch WHERE status = 'running' \
             AND lease_until IS NOT NULL AND lease_until <= ?1 \
             AND attempt_count >= ?2 \
             AND (active_attempt_epoch IS NULL OR cancel_requested <> 0) \
             ORDER BY created_at"
        ))
        .bind(crate::clock::db_millis(expired_before_ms))
        .bind(max_attempts_i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_error)?;
        let mut quarantined = 0;
        for (
            run_id,
            owner,
            epoch,
            attempt_count,
            cancellation_requested,
            active_attempt_owner,
            active_attempt_epoch,
        ) in candidates
        {
            let Some(owner) = owner else {
                return Err(DispatchError::Rejected(
                    "expired running dispatch has no persisted lease owner".to_string(),
                ));
            };
            let claim_epoch = durable_u64("dispatch lease epoch", epoch)?;
            // The SQL above only prefilters. The slot disposition is decided
            // here, on the locked row, by the one kernel every backend shares.
            let slot = awaken_dispatch_contract::PhysicalAttemptSlot::from_facts(
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
            let changed = sqlx::query(&format!(
                "UPDATE {p}_dispatch SET status = ?4, \
                 lease_owner = NULL, lease_until = NULL, dead_lettered_at = ?1 \
                 WHERE run_id = ?2 AND status = 'running' AND lease_epoch = ?3"
            ))
            .bind(crate::clock::db_millis(expired_before_ms))
            .bind(&run_id)
            .bind(epoch)
            .bind(crate::dispatch_state_db(next.state))
            .execute(&mut *tx)
            .await
            .map_err(store_error)?
            .rows_affected();
            if changed == 0 {
                continue;
            }
            let claim = RunClaim {
                run_id: RunId(run_id),
                owner,
                epoch: claim_epoch,
            };
            insert_operation(
                &mut tx,
                p,
                &DispatchOperation::LeaseLost {
                    claim: claim.clone(),
                    reason: LeaseLossReason::RetryExhausted,
                },
                now_ms,
            )
            .await?;
            insert_operation(
                &mut tx,
                p,
                &DispatchOperation::DeadLettered {
                    claim,
                    attempt_count: durable_u64("dispatch attempt count", attempt_count)?,
                },
                now_ms,
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

impl SqliteDispatchStore {
    async fn purge_dead_letters_with_cutoff(
        &self,
        cutoff_ms: Option<u64>,
    ) -> Result<usize, DispatchError> {
        let (_authority, mut conn) = self.with_conn().await?;
        let mut tx = immediate(&mut conn).await?;
        let p = NS;
        // SQL projection of the same actually-empty physical slot used by
        // settlement. Check every retained fact: historical unbound, torn
        // owner/epoch, orphan binding and closing rows must survive GC.
        let age = if cutoff_ms.is_some() {
            " AND dead_lettered_at IS NOT NULL AND dead_lettered_at <= ?1"
        } else {
            ""
        };
        let cond = format!(
            "status = 'dead_letter' AND active_attempt_owner IS NULL \
             AND active_attempt_epoch IS NULL AND active_attempt_binding IS NULL \
             AND active_attempt_closing = FALSE{age}"
        );
        let pending = format!(
            "DELETE FROM {p}_pending WHERE run_id IN \
             (SELECT run_id FROM {p}_dispatch WHERE {cond})"
        );
        let mut pending = sqlx::query(&pending);
        if let Some(cutoff_ms) = cutoff_ms {
            pending = pending.bind(crate::clock::db_millis(cutoff_ms));
        }
        pending.execute(&mut *tx).await.map_err(store_error)?;
        let rows = format!("DELETE FROM {p}_dispatch WHERE {cond}");
        let mut rows = sqlx::query(&rows);
        if let Some(cutoff_ms) = cutoff_ms {
            rows = rows.bind(crate::clock::db_millis(cutoff_ms));
        }
        let removed = rows.execute(&mut *tx).await.map_err(store_error)?;
        tx.commit().await.map_err(store_error)?;
        Ok(affected(removed))
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchReads for SqliteDispatchStore {
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
impl awaken_dispatch_contract::DispatchAdministration for SqliteDispatchStore {
    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        let run_id = run_id.0.clone();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let mut tx = immediate(&mut conn).await?;
        let current: Option<(String, i64, i64)> = sqlx::query_as(&format!(
            "SELECT status, lease_epoch, cancel_requested FROM {p}_dispatch \
             WHERE run_id = ?1"
        ))
        .bind(&run_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        let Some((status, lease_epoch, cancellation_requested)) = current else {
            tx.commit().await.map_err(store_error)?;
            return Ok(false);
        };
        let Some(next) = crate::persisted_dispatch_transition(
            &status,
            lease_epoch,
            cancellation_requested != 0,
        )?
        .requeue_dead_letter() else {
            tx.commit().await.map_err(store_error)?;
            return Ok(false);
        };
        let n = sqlx::query(&format!(
            "UPDATE {p}_dispatch SET status = ?2, attempt_count = 0, \
             lease_owner = NULL, lease_until = NULL \
             WHERE run_id = ?1 AND status = 'dead_letter' \
             AND cancel_requested = 0"
        ))
        .bind(&run_id)
        .bind(crate::dispatch_state_db(next.state))
        .execute(&mut *tx)
        .await
        .map_err(store_error)?
        .rows_affected();
        tx.commit().await.map_err(store_error)?;
        Ok(n > 0)
    }

    async fn cancel(&self, run_id: &RunId) -> Result<Option<ThreadId>, DispatchError> {
        let run_id = run_id.0.clone();
        let now_ms = self.clock.now_ms();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let mut tx = immediate(&mut conn).await?;
        let current: Option<(String, String, Option<String>, i64, i64)> = sqlx::query_as(&format!(
            "SELECT thread_id, status, lease_owner, lease_epoch, cancel_requested FROM {p}_dispatch \
             WHERE run_id = ?1 AND status IN \
             ('reserved', 'reservation_running', 'pending', 'awaiting', 'running', 'dead_letter')"
        ))
        .bind(&run_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        if let Some((_, status, _, epoch, cancellation_requested)) = &current {
            let transition =
                crate::persisted_dispatch_transition(status, *epoch, *cancellation_requested != 0)?;
            let awaken_dispatch_contract::CancelTransition::Applied {
                state: next,
                revoked_lease,
                clear_retry_deadline,
            } = transition.cancel().map_err(crate::transition_error)?
            else {
                tx.commit().await.map_err(store_error)?;
                return Ok(None);
            };
            sqlx::query(&format!(
                "UPDATE {p}_dispatch SET cancel_requested = ?2, lease_epoch = ?3, \
                 lease_owner = CASE WHEN ?4 THEN NULL ELSE lease_owner END, \
                 lease_until = CASE WHEN ?4 AND ?5 = 'reserved' THEN 0 \
                   WHEN ?4 THEN NULL ELSE lease_until END, status = ?5, \
                 retry_not_before = CASE WHEN ?6 THEN NULL ELSE retry_not_before END \
                 WHERE run_id = ?1"
            ))
            .bind(&run_id)
            .bind(i64::from(next.cancellation_requested))
            .bind(durable_i64("dispatch lease epoch", next.lease_epoch)?)
            .bind(revoked_lease)
            .bind(crate::dispatch_state_db(next.state))
            .bind(clear_retry_deadline)
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
            if revoked_lease {
                let (_, _, owner, epoch, _) = current.as_ref().expect("current exists");
                insert_operation(
                    &mut tx,
                    p,
                    &DispatchOperation::LeaseLost {
                        claim: RunClaim {
                            run_id: RunId(run_id.clone()),
                            owner: owner.clone().ok_or_else(|| {
                                DispatchError::Rejected(
                                    "claimed dispatch has no persisted lease owner".to_string(),
                                )
                            })?,
                            epoch: u64::try_from(*epoch).map_err(|_| {
                                DispatchError::Rejected(
                                    "persisted dispatch claim epoch is negative".to_string(),
                                )
                            })?,
                        },
                        reason: LeaseLossReason::Cancelled,
                    },
                    now_ms,
                )
                .await?;
            }
        }
        tx.commit().await.map_err(store_error)?;
        Ok(current.map(|(thread, _, _, _, _)| ThreadId(thread)))
    }
}

use awaken_dispatch_contract::DispatchCommitAuthority as _;
